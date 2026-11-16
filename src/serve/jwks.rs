//! The JWKS client (0017): fetch an issuer's verification keys, cache them, and
//! refresh them within bounds. Not yet wired to a request.
//!
//! The rules, from the note:
//!
//! - **One fetch at a time per issuer**, and a refresh is never started more often
//!   than once per `MIN_RETRY` (30 s): a failing issuer is not hammered by every
//!   request, and a flood of tokens with unknown `kid`s cannot make the gateway an
//!   amplifier against it.
//! - **The fetch is a task of its own**, so a caller that goes away does not abort
//!   it, and cached keys still within the stale window are served at once while a
//!   refresh runs in the background: only a caller with nothing usable waits.
//! - **Keys are fresh for `refresh`.** After that a refresh is tried on the next
//!   use; if it fails, the cached keys keep verifying for up to `max_stale` more
//!   (an issuer outage need not take the platform down), then the issuer is
//!   unavailable. The revocation bound is therefore `refresh + max_stale` while
//!   the issuer is unreachable, and `refresh` otherwise.
//! - **The fetch is bounded and configured, never chosen by a token**: the
//!   configured address only (https, or http to a loopback address literal), no
//!   redirects, no ambient proxy, 5 s to connect and 10 s in all, a 1 MiB body
//!   limit, no userinfo in the address.
//! - A document with no usable key is a failed refresh: the previous keys stay.

use super::jwt::KeySet;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// The shortest time between two refresh attempts, successful or not.
pub const MIN_RETRY: Duration = Duration::from_secs(30);
/// The largest JWKS document read.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

pub const REFRESHES: &str = "llmshim_jwks_refresh_total";
pub const STALE_SERVED: &str = "llmshim_jwks_stale_served_total";
pub const REFUSED: &str = "llmshim_jwks_refresh_refused_total";

/// Where the document comes from. An error is a class (`network`, `status`,
/// `too_large`, `malformed`), never a message that could carry a response body.
#[async_trait]
pub trait Fetch: Send + Sync {
    async fn fetch(&self) -> Result<Vec<u8>, &'static str>;
}

/// The issuer cannot be used right now: no keys were ever loaded, or the cached
/// ones are older than `refresh + max_stale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub refresh: Duration,
    pub max_stale: Duration,
}

#[derive(Default)]
struct State {
    keys: Option<Arc<KeySet>>,
    fetched_at: Option<Instant>,
    last_attempt: Option<Instant>,
    last_error: Option<&'static str>,
    in_flight: bool,
}

/// What `/v1/serve/config` reports for an issuer.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Status {
    pub loaded: bool,
    pub keys: usize,
    pub age_secs: Option<u64>,
    /// The class of the last failed refresh, cleared by the next success.
    pub last_error: Option<&'static str>,
}

pub struct Jwks {
    inner: Arc<Inner>,
}

struct Inner {
    name: String,
    fetcher: Arc<dyn Fetch>,
    settings: Settings,
    state: Mutex<State>,
    done: tokio::sync::Notify,
}

/// Clears the in-flight flag however the refresh task ends, a panic included.
struct Finish(Arc<Inner>);

impl Drop for Finish {
    fn drop(&mut self) {
        self.0.locked().in_flight = false;
        self.0.done.notify_waiters();
    }
}

impl Jwks {
    pub fn new(name: &str, fetcher: Arc<dyn Fetch>, settings: Settings) -> Self {
        for outcome in ["ok", "failed"] {
            crate::gateway::metrics::add(REFRESHES, &[("issuer", name), ("outcome", outcome)], 0);
        }
        crate::gateway::metrics::add(STALE_SERVED, &[("issuer", name)], 0);
        crate::gateway::metrics::add(REFUSED, &[("issuer", name)], 0);
        Self {
            inner: Arc::new(Inner {
                name: name.to_owned(),
                fetcher,
                settings,
                state: Mutex::new(State::default()),
                done: tokio::sync::Notify::new(),
            }),
        }
    }

    pub fn status(&self) -> Status {
        let state = self.inner.locked();
        Status {
            loaded: state.keys.is_some(),
            keys: state.keys.as_ref().map_or(0, |keys| keys.len()),
            age_secs: state.fetched_at.map(|at| at.elapsed().as_secs()),
            last_error: state.last_error,
        }
    }

    /// The keys to verify with now. Past `refresh` a refresh is started in the
    /// background, and cached keys still within `refresh + max_stale` are served
    /// at once, so a slow issuer never holds a request that has keys to use; only
    /// a caller with nothing usable waits for the fetch.
    pub async fn keys(&self) -> Result<Arc<KeySet>, Unavailable> {
        if let Some(keys) = self.inner.fresh() {
            return Ok(keys);
        }
        self.inner.start_refresh();
        if let Ok(keys) = self.inner.usable() {
            return Ok(keys);
        }
        self.inner.wait_idle().await;
        self.inner.usable()
    }

    /// For a token whose `kid` the cached keys do not know: one more refresh,
    /// but never sooner than `MIN_RETRY` after the last attempt, waited for
    /// because the caller needs the answer. Returns the keys to try again with
    /// (the same ones when the refresh was not allowed).
    pub async fn keys_for_unknown_kid(&self) -> Result<Arc<KeySet>, Unavailable> {
        if !self.inner.may_attempt() {
            crate::gateway::metrics::incr(REFUSED, &[("issuer", &self.inner.name)]);
            return self.inner.usable();
        }
        self.inner.start_refresh();
        self.inner.wait_idle().await;
        self.inner.usable()
    }
}

impl Inner {
    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fresh(&self) -> Option<Arc<KeySet>> {
        let state = self.locked();
        let (keys, at) = (state.keys.as_ref()?, state.fetched_at?);
        (at.elapsed() < self.settings.refresh).then(|| keys.clone())
    }

    /// The cached keys while they are within `refresh + max_stale`; serving them
    /// past `refresh` is counted.
    fn usable(&self) -> Result<Arc<KeySet>, Unavailable> {
        let state = self.locked();
        let (Some(keys), Some(at)) = (state.keys.as_ref(), state.fetched_at) else {
            return Err(Unavailable);
        };
        let age = at.elapsed();
        if age
            >= self
                .settings
                .refresh
                .saturating_add(self.settings.max_stale)
        {
            return Err(Unavailable);
        }
        if age >= self.settings.refresh {
            crate::gateway::metrics::incr(STALE_SERVED, &[("issuer", &self.name)]);
        }
        Ok(keys.clone())
    }

    fn may_attempt(&self) -> bool {
        self.locked()
            .last_attempt
            .is_none_or(|at| at.elapsed() >= MIN_RETRY)
    }

    /// Start a refresh unless one is running or the last attempt was under
    /// `MIN_RETRY` ago. The fetch runs in a task of its own: a caller that goes
    /// away (a client that disconnected) neither aborts it nor leaves the issuer
    /// looking unavailable for the interval. The attempt is stamped as it starts.
    fn start_refresh(self: &Arc<Self>) {
        {
            let mut state = self.locked();
            let too_soon = state
                .last_attempt
                .is_some_and(|at| at.elapsed() < MIN_RETRY);
            if state.in_flight || too_soon {
                return;
            }
            state.in_flight = true;
            state.last_attempt = Some(Instant::now());
        }
        let inner = self.clone();
        tokio::spawn(async move {
            let _finish = Finish(inner.clone());
            inner.refresh().await;
        });
    }

    async fn refresh(&self) {
        let outcome = match self.fetcher.fetch().await {
            Ok(document) => match KeySet::from_jwks(&document) {
                Ok(keys) if !keys.is_empty() => Ok(keys),
                Ok(_) => Err("no_usable_keys"),
                Err(_) => Err("malformed"),
            },
            Err(class) => Err(class),
        };
        let mut state = self.locked();
        match outcome {
            Ok(keys) => {
                state.keys = Some(Arc::new(keys));
                state.fetched_at = Some(Instant::now());
                state.last_error = None;
                crate::gateway::metrics::incr(
                    REFRESHES,
                    &[("issuer", &self.name), ("outcome", "ok")],
                );
            }
            Err(class) => {
                state.last_error = Some(class);
                crate::gateway::metrics::incr(
                    REFRESHES,
                    &[("issuer", &self.name), ("outcome", "failed")],
                );
            }
        }
    }

    /// Wait until no refresh is running.
    async fn wait_idle(&self) {
        loop {
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.locked().in_flight {
                return;
            }
            notified.await;
        }
    }
}

/// Why `url` cannot be a JWKS address, if it cannot: https, or http only to a
/// loopback address literal, and no credentials in it.
pub fn url_problem(url: &str) -> Option<String> {
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return Some("is not a valid URL".into()),
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Some("must not carry credentials; use `headers`".into());
    }
    if parsed.fragment().is_some() {
        return Some("must not have a fragment".into());
    }
    match parsed.scheme() {
        "https" => None,
        "http" => {
            let loopback = parsed
                .host_str()
                .unwrap_or("")
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
            (!loopback).then(|| "must be https (http only to a loopback address literal)".into())
        }
        _ => Some("must be an https URL".into()),
    }
}

/// The real fetch: its own client, never the shared provider one.
pub struct HttpFetch {
    client: reqwest::Client,
    url: reqwest::Url,
}

impl HttpFetch {
    /// `ca_pem`, when given, is the only trust anchor for the fetch (a private
    /// issuer's CA). `headers` are sent as given and never logged.
    pub fn new(
        url: &str,
        ca_pem: Option<&[u8]>,
        headers: Vec<(String, String)>,
    ) -> Result<Self, String> {
        if let Some(problem) = url_problem(url) {
            return Err(format!("the JWKS address {problem}"));
        }
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "a JWKS header name is not valid".to_string())?;
            let mut value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| "a JWKS header value is not valid".to_string())?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        map.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .default_headers(map);
        if let Some(pem) = ca_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|_| "the JWKS CA file holds no readable certificate".to_string())?;
            if certificates.is_empty() {
                return Err("the JWKS CA file holds no certificate".into());
            }
            builder = builder.tls_built_in_root_certs(false);
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        Ok(Self {
            client: builder
                .build()
                .map_err(|_| "cannot build the JWKS client".to_string())?,
            url: reqwest::Url::parse(url).map_err(|_| "is not a valid URL".to_string())?,
        })
    }
}

#[async_trait]
impl Fetch for HttpFetch {
    async fn fetch(&self) -> Result<Vec<u8>, &'static str> {
        let mut response = self
            .client
            .get(self.url.clone())
            .send()
            .await
            .map_err(|_| "network")?;
        if !response.status().is_success() {
            return Err("status");
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err("too_large");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "network")? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err("too_large");
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DOC: &str = r#"{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"k",
        "x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}]}"#;

    /// Answers from a script, then repeats the last answer; counts the calls.
    struct Script {
        answers: Mutex<VecDeque<Result<Vec<u8>, &'static str>>>,
        calls: AtomicUsize,
        delay: Duration,
    }

    impl Script {
        fn new(answers: Vec<Result<Vec<u8>, &'static str>>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into()),
                calls: AtomicUsize::new(0),
                delay: Duration::ZERO,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Fetch for Script {
        async fn fetch(&self) -> Result<Vec<u8>, &'static str> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let mut answers = self.answers.lock().unwrap();
            if answers.len() > 1 {
                answers.pop_front().unwrap()
            } else {
                answers.front().cloned().unwrap()
            }
        }
    }

    fn jwks(name: &str, script: &Arc<Script>) -> Jwks {
        Jwks::new(
            name,
            script.clone(),
            Settings {
                refresh: Duration::from_secs(300),
                max_stale: Duration::from_secs(3600),
            },
        )
    }

    /// Let background refresh tasks run (the clock is paused, so this is instant).
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    fn good() -> Result<Vec<u8>, &'static str> {
        Ok(DOC.as_bytes().to_vec())
    }

    fn metric(name: &str, labels: &str) -> u64 {
        crate::gateway::metrics::render()
            .lines()
            .find(|line| line.starts_with(&format!("{name}{{{labels}}} ")))
            .and_then(|line| line.rsplit(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }

    #[tokio::test(start_paused = true)]
    async fn keys_load_once_and_stay_fresh_for_the_refresh_interval() {
        let script = Script::new(vec![good()]);
        let jwks = jwks("fresh", &script);
        assert!(!jwks.status().loaded);
        assert_eq!(jwks.keys().await.unwrap().len(), 1);
        assert_eq!(jwks.keys().await.unwrap().len(), 1);
        assert_eq!(script.calls(), 1);
        tokio::time::advance(Duration::from_secs(299)).await;
        jwks.keys().await.unwrap();
        assert_eq!(script.calls(), 1);
        tokio::time::advance(Duration::from_secs(2)).await;
        jwks.keys().await.unwrap();
        settle().await;
        assert_eq!(script.calls(), 2, "refreshed once past the interval");
        let status = jwks.status();
        assert!(status.loaded && status.keys == 1 && status.last_error.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_share_one_fetch() {
        let script = Arc::new(Script {
            answers: Mutex::new(vec![good()].into()),
            calls: AtomicUsize::new(0),
            delay: Duration::from_secs(2),
        });
        let jwks = Arc::new(jwks("flight", &script));
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let jwks = jwks.clone();
                tokio::spawn(async move { jwks.keys().await })
            })
            .collect();
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }
        assert_eq!(script.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_issuer_that_was_never_reachable_is_unavailable_and_not_hammered() {
        let script = Script::new(vec![Err("network")]);
        let jwks = jwks("down", &script);
        for _ in 0..50 {
            assert_eq!(jwks.keys().await.err(), Some(Unavailable));
            tokio::time::advance(Duration::from_millis(500)).await;
        }
        assert_eq!(script.calls(), 1, "retries wait for MIN_RETRY");
        assert_eq!(jwks.status().last_error, Some("network"));
        tokio::time::advance(MIN_RETRY).await;
        assert_eq!(jwks.keys().await.err(), Some(Unavailable));
        assert_eq!(script.calls(), 2);
        assert_eq!(metric(REFRESHES, "issuer=\"down\",outcome=\"failed\""), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_keys_serve_for_the_stale_window_then_the_issuer_is_unavailable() {
        let script = Script::new(vec![good(), Err("status")]);
        let jwks = jwks("stale", &script);
        jwks.keys().await.unwrap();
        // Past the refresh interval, the refresh fails: the cached keys serve.
        tokio::time::advance(Duration::from_secs(301)).await;
        assert!(
            jwks.keys().await.is_ok(),
            "served at once, refresh in the background"
        );
        settle().await;
        assert_eq!(jwks.status().last_error, Some("status"));
        assert!(metric(STALE_SERVED, "issuer=\"stale\"") >= 1);
        // Up to refresh + max_stale.
        tokio::time::advance(Duration::from_secs(3598)).await;
        assert!(jwks.keys().await.is_ok());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(jwks.keys().await.err(), Some(Unavailable));
    }

    #[tokio::test(start_paused = true)]
    async fn a_caller_that_goes_away_neither_aborts_the_fetch_nor_costs_the_issuer() {
        let script = Arc::new(Script {
            answers: Mutex::new(vec![good()].into()),
            calls: AtomicUsize::new(0),
            delay: Duration::from_secs(5),
        });
        let jwks = Arc::new(jwks("cancel", &script));
        let task = {
            let jwks = jwks.clone();
            tokio::spawn(async move { jwks.keys().await })
        };
        tokio::time::advance(Duration::from_secs(1)).await;
        task.abort();
        let _ = task.await;
        // The fetch carries on without its caller, and the next caller gets the
        // keys it loaded: one fetch in all, no 30 s of an unavailable issuer.
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(jwks.keys().await.is_ok());
        assert_eq!(script.calls(), 1);
        assert!(jwks.status().loaded);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_issuer_does_not_hold_callers_that_have_stale_keys() {
        let script = Arc::new(Script {
            answers: Mutex::new(vec![good()].into()),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
        });
        let jwks = Arc::new(jwks("hang", &script));
        jwks.keys().await.unwrap();
        // Now the issuer hangs for longer than any caller should wait.
        let hanging = Arc::new(Script {
            answers: Mutex::new(vec![good()].into()),
            calls: AtomicUsize::new(0),
            delay: Duration::from_secs(1000),
        });
        let jwks = Jwks {
            inner: Arc::new(Inner {
                name: "hang".into(),
                fetcher: hanging.clone(),
                settings: jwks.inner.settings,
                state: Mutex::new(std::mem::take(&mut *jwks.inner.state.lock().unwrap())),
                done: tokio::sync::Notify::new(),
            }),
        };
        tokio::time::advance(Duration::from_secs(301)).await;
        let started = Instant::now();
        for _ in 0..10 {
            assert!(jwks.keys().await.is_ok());
        }
        assert!(started.elapsed() < Duration::from_secs(1), "nobody waited");
        settle().await;
        assert_eq!(hanging.calls(), 1, "one background refresh, shared");
    }

    #[tokio::test(start_paused = true)]
    async fn huge_windows_do_not_overflow() {
        let script = Script::new(vec![good()]);
        let jwks = Jwks::new(
            "huge",
            script.clone(),
            Settings {
                refresh: Duration::from_secs(u64::MAX),
                max_stale: Duration::from_secs(u64::MAX),
            },
        );
        assert!(jwks.keys().await.is_ok());
        assert!(jwks.keys().await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_stale_window_never_serves_past_the_refresh_interval() {
        let script = Script::new(vec![good(), Err("network")]);
        let jwks = Jwks::new(
            "nostale",
            script.clone(),
            Settings {
                refresh: Duration::from_secs(300),
                max_stale: Duration::ZERO,
            },
        );
        jwks.keys().await.unwrap();
        tokio::time::advance(Duration::from_secs(301)).await;
        assert_eq!(jwks.keys().await.err(), Some(Unavailable));
    }

    #[tokio::test(start_paused = true)]
    async fn a_bad_document_keeps_the_previous_keys() {
        let script = Script::new(vec![
            good(),
            Ok(b"{\"keys\":[]}".to_vec()),
            Ok(b"not json".to_vec()),
        ]);
        let jwks = jwks("bad", &script);
        jwks.keys().await.unwrap();
        tokio::time::advance(Duration::from_secs(301)).await;
        assert_eq!(jwks.keys().await.unwrap().len(), 1);
        settle().await;
        assert_eq!(jwks.status().last_error, Some("no_usable_keys"));
        tokio::time::advance(Duration::from_secs(31)).await;
        assert_eq!(jwks.keys().await.unwrap().len(), 1);
        settle().await;
        assert_eq!(jwks.status().last_error, Some("malformed"));
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_kids_trigger_at_most_one_refresh_per_interval() {
        let script = Script::new(vec![good()]);
        let jwks = jwks("kids", &script);
        jwks.keys().await.unwrap();
        // Right after the load: refused, and counted.
        for _ in 0..100 {
            assert!(jwks.keys_for_unknown_kid().await.is_ok());
        }
        assert_eq!(script.calls(), 1);
        assert_eq!(metric(REFUSED, "issuer=\"kids\""), 100);
        tokio::time::advance(MIN_RETRY).await;
        for _ in 0..100 {
            jwks.keys_for_unknown_kid().await.unwrap();
        }
        assert_eq!(script.calls(), 2, "one refresh, not a hundred");
    }

    #[test]
    fn addresses_are_https_or_loopback_http_without_credentials() {
        for ok in [
            "https://oidc.example.com/keys",
            "https://oidc.example.com:8443/.well-known/jwks.json?x=1",
            "http://127.0.0.1:9000/keys",
            "http://[::1]/keys",
        ] {
            assert_eq!(url_problem(ok), None, "{ok}");
        }
        for bad in [
            "http://oidc.example.com/keys",
            "http://localhost/keys",
            "http://10.0.0.1/keys",
            "ftp://x/keys",
            "https://user:pw@oidc.example.com/keys",
            "https://oidc.example.com/keys#frag",
            "not a url",
            "",
        ] {
            assert!(url_problem(bad).is_some(), "{bad}");
        }
    }

    /// A local server that answers `/keys` as told, and records the headers.
    async fn server(
        status: u16,
        body: Vec<u8>,
        redirect: bool,
    ) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        use axum::routing::get;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let app = axum::Router::new()
            .route(
                "/keys",
                get(move |headers: axum::http::HeaderMap| {
                    let (body, sink) = (body.clone(), sink.clone());
                    async move {
                        for (name, value) in &headers {
                            sink.lock().unwrap().push((
                                name.to_string(),
                                value.to_str().unwrap_or_default().to_owned(),
                            ));
                        }
                        if redirect {
                            return (
                                axum::http::StatusCode::FOUND,
                                [("location", "/elsewhere")],
                                Vec::new(),
                            );
                        }
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            [("content-type", "application/json")],
                            body,
                        )
                    }
                }),
            )
            .route("/elsewhere", get(|| async { DOC }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/keys"), seen)
    }

    #[tokio::test]
    async fn the_real_fetch_reads_a_document_and_sends_the_configured_headers() {
        let (url, seen) = server(200, DOC.as_bytes().to_vec(), false).await;
        let fetch = HttpFetch::new(
            &url,
            None,
            vec![("authorization".into(), "Bearer t0k".into())],
        )
        .unwrap();
        assert_eq!(fetch.fetch().await.unwrap(), DOC.as_bytes());
        let seen = seen.lock().unwrap();
        assert!(seen
            .iter()
            .any(|(n, v)| n == "authorization" && v == "Bearer t0k"));
        assert!(seen
            .iter()
            .any(|(n, v)| n == "accept" && v == "application/json"));
    }

    #[tokio::test]
    async fn the_real_fetch_refuses_errors_redirects_and_oversized_bodies() {
        let (url, _) = server(500, b"boom".to_vec(), false).await;
        assert_eq!(
            HttpFetch::new(&url, None, vec![]).unwrap().fetch().await,
            Err("status")
        );
        // A redirect is a failure, never followed to somewhere the token chose.
        let (url, _) = server(200, vec![], true).await;
        assert_eq!(
            HttpFetch::new(&url, None, vec![]).unwrap().fetch().await,
            Err("status")
        );
        let (url, _) = server(200, vec![b' '; MAX_BODY_BYTES + 1], false).await;
        assert_eq!(
            HttpFetch::new(&url, None, vec![]).unwrap().fetch().await,
            Err("too_large")
        );
        let (url, _) = server(200, vec![b' '; MAX_BODY_BYTES], false).await;
        assert!(HttpFetch::new(&url, None, vec![])
            .unwrap()
            .fetch()
            .await
            .is_ok());
        // Nothing listening.
        let gone = HttpFetch::new("http://127.0.0.1:1/keys", None, vec![]).unwrap();
        assert_eq!(gone.fetch().await, Err("network"));
    }

    #[tokio::test]
    async fn a_body_without_a_length_is_cut_off_at_the_limit() {
        use axum::routing::get;
        // Chunked, so there is no Content-Length to refuse up front.
        let app = axum::Router::new().route(
            "/keys",
            get(|| async {
                let chunk = vec![b' '; 64 * 1024];
                let stream = futures::stream::iter(
                    (0..20).map(move |_| Ok::<_, std::convert::Infallible>(chunk.clone())),
                );
                axum::body::Body::from_stream(stream)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let fetch = HttpFetch::new(&format!("http://{address}/keys"), None, vec![]).unwrap();
        assert_eq!(fetch.fetch().await, Err("too_large"));
    }

    #[test]
    fn construction_refuses_a_bad_address_or_ca() {
        assert!(HttpFetch::new("http://example.com/keys", None, vec![]).is_err());
        assert!(HttpFetch::new("https://example.com/keys", Some(b"not pem"), vec![]).is_err());
        assert!(HttpFetch::new(
            "https://example.com/keys",
            None,
            vec![("bad name".into(), "v".into())]
        )
        .is_err());
    }
}
