//! A bearer token held until it is about to expire (note 0050, slice 3b): one fetch at a time, a failed
//! fetch remembered for a few seconds, and a refusal that forces a refetch only for the token that was
//! refused and not more than every few seconds. The same rules as the AWS credential chain's.

use super::BlobError;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// A token is replaced this long before it expires.
const RENEW_BEFORE: Duration = Duration::from_secs(60);
/// How long a failure to get a token is remembered, so a brown-out is one shared wait.
const FAILURE_KEEP: Duration = Duration::from_secs(5);
/// The soonest a refusal may force a refetch.
const FORCE_EVERY: Duration = Duration::from_secs(5);

struct Held {
    token: String,
    renew_at: Instant,
    fetched: Instant,
}

#[derive(Default)]
struct State {
    held: Option<Held>,
    failed: Option<(Instant, BlobError)>,
}

#[derive(Default)]
pub(super) struct TokenHolder {
    state: Mutex<State>,
}

impl TokenHolder {
    /// The token, from the cache while it is good, else from `load` (token and lifetime).
    pub(super) async fn get<F, Fut>(&self, load: F) -> Result<String, BlobError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(String, Duration), BlobError>>,
    {
        let mut state = self.state.lock().await;
        if let Some(held) = &state.held {
            if Instant::now() < held.renew_at {
                return Ok(held.token.clone());
            }
        }
        if let Some((at, error)) = state.failed {
            if at.elapsed() < FAILURE_KEEP {
                return Err(error);
            }
        }
        match load().await {
            Ok((token, lifetime)) => {
                let now = Instant::now();
                // A token that lives less than the margin is used until it is nearly gone.
                let keep = lifetime
                    .checked_sub(RENEW_BEFORE)
                    .filter(|keep| !keep.is_zero())
                    .unwrap_or_else(|| (lifetime / 2).min(Duration::from_secs(5)));
                state.failed = None;
                state.held = Some(Held {
                    token: token.clone(),
                    renew_at: now + keep,
                    fetched: now,
                });
                Ok(token)
            }
            Err(error) => {
                state.failed = Some((Instant::now(), error));
                Err(error)
            }
        }
    }

    /// The store refused a request signed with `refused`: forget it, if it is still the token held and
    /// was fetched at least five seconds ago.
    pub(super) async fn invalidate(&self, refused: &str) {
        let mut state = self.state.lock().await;
        if state
            .held
            .as_ref()
            .is_some_and(|held| held.token == refused && held.fetched.elapsed() >= FORCE_EVERY)
        {
            state.held = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting<'a>(
        calls: &'a AtomicUsize,
        token: &'static str,
        lifetime: u64,
    ) -> impl FnOnce() -> std::future::Ready<Result<(String, Duration), BlobError>> + 'a {
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok((token.to_owned(), Duration::from_secs(lifetime))))
        }
    }

    #[tokio::test]
    async fn a_good_token_is_kept_and_a_failure_is_remembered() {
        let holder = TokenHolder::default();
        let calls = AtomicUsize::new(0);
        assert_eq!(
            holder.get(counting(&calls, "t1", 3600)).await.unwrap(),
            "t1"
        );
        assert_eq!(
            holder.get(counting(&calls, "t2", 3600)).await.unwrap(),
            "t1"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A holder that has never succeeded remembers its failure.
        let failing = TokenHolder::default();
        let asked = AtomicUsize::new(0);
        for _ in 0..3 {
            let result = failing
                .get(|| {
                    asked.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Err(BlobError::Unavailable))
                })
                .await;
            assert_eq!(result, Err(BlobError::Unavailable));
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refusal_forces_a_refetch_only_for_the_token_refused_and_not_at_once() {
        let holder = TokenHolder::default();
        let calls = AtomicUsize::new(0);
        holder.get(counting(&calls, "t1", 3600)).await.unwrap();
        // Just fetched: a refusal does not force a refetch (a missing permission is not a rotation).
        holder.invalidate("t1").await;
        assert_eq!(
            holder.get(counting(&calls, "t2", 3600)).await.unwrap(),
            "t1"
        );
        // Another token's refusal never evicts this one.
        holder.invalidate("someone-elses").await;
        assert_eq!(
            holder.get(counting(&calls, "t2", 3600)).await.unwrap(),
            "t1"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_token_that_lives_less_than_the_margin_is_still_used_briefly() {
        let holder = TokenHolder::default();
        let calls = AtomicUsize::new(0);
        assert_eq!(
            holder.get(counting(&calls, "short", 30)).await.unwrap(),
            "short"
        );
        assert_eq!(
            holder.get(counting(&calls, "again", 30)).await.unwrap(),
            "short"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
