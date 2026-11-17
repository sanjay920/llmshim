//! Deployments: one name, several targets (design note 0031).
//!
//! A deployment is addressed `deployment/<name>` like a route is addressed `route/<name>`, and
//! is resolved **once**, in [`crate::router::Router::expand_route`], to one concrete
//! `provider/model` that replaces the name in the request. Nothing after that sees the name, so
//! admission, the queue, a worker on another replica and the allowlist all agree on the pick.

use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// How a deployment chooses among its targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Each target in turn.
    RoundRobin,
    /// Smooth weighted round robin: over any `sum(weights)` requests (at steady state) a target is
    /// chosen exactly `weight` times, evenly interleaved.
    Weighted,
    /// The first target that is not skipped, in order (a primary and its standbys).
    Ordered,
    /// Weighted rendezvous hashing on the conversation, so one conversation keeps one target on
    /// every replica with no shared state (prompt caches and encrypted reasoning are per account).
    Conversation,
}

/// One target of a deployment: a concrete `provider/model` and a weight (1 to 1000).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub model: String,
    pub weight: u32,
}

/// A named set of targets and the rule for choosing one.
pub struct Deployment {
    strategy: Strategy,
    targets: Vec<Target>,
    state: Mutex<State>,
}

struct State {
    /// The next index to try, for round robin.
    next: usize,
    /// The running weights of smooth weighted round robin, one per target.
    current: Vec<i64>,
}

impl std::fmt::Debug for Deployment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deployment")
            .field("strategy", &self.strategy)
            .field("targets", &self.targets)
            .finish()
    }
}

impl Deployment {
    /// `targets` must not be empty (the file's validation guarantees it).
    pub fn new(strategy: Strategy, targets: Vec<Target>) -> Self {
        let len = targets.len();
        Self {
            strategy,
            targets,
            state: Mutex::new(State {
                next: 0,
                current: vec![0; len],
            }),
        }
    }

    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// Choose a target. `usable(model)` says whether the target's provider may be sent a request
    /// now (its circuit is not open, or a probe was just admitted); when no target is usable
    /// health is ignored, because refusing locally would only turn an upstream failure into a local
    /// one. `conversation` is a hash of the conversation, used by [`Strategy::Conversation`].
    pub fn pick(&self, usable: &dyn Fn(&str) -> bool, conversation: u64) -> &Target {
        let mut flags: Vec<bool> = self.targets.iter().map(|t| usable(&t.model)).collect();
        if !flags.iter().any(|ok| *ok) {
            flags.iter_mut().for_each(|ok| *ok = true);
        }
        let index = match self.strategy {
            Strategy::Ordered => flags.iter().position(|ok| *ok).unwrap_or(0),
            Strategy::RoundRobin => {
                let mut state = self.lock();
                let len = self.targets.len();
                let index = (0..len)
                    .map(|offset| (state.next + offset) % len)
                    .find(|i| flags[*i])
                    .unwrap_or(0);
                state.next = (index + 1) % len;
                index
            }
            Strategy::Weighted => {
                let mut state = self.lock();
                let total: i64 = self
                    .targets
                    .iter()
                    .zip(&flags)
                    .filter(|(_, ok)| **ok)
                    .map(|(t, _)| i64::from(t.weight))
                    .sum();
                let mut best: Option<usize> = None;
                for (i, target) in self.targets.iter().enumerate() {
                    if !flags[i] {
                        continue;
                    }
                    state.current[i] += i64::from(target.weight);
                    if best.is_none_or(|b| state.current[i] > state.current[b]) {
                        best = Some(i);
                    }
                }
                let index = best.unwrap_or(0);
                state.current[index] -= total;
                index
            }
            Strategy::Conversation => {
                // Weighted rendezvous: the target with the smallest -ln(u) / weight wins, so
                // removing a target moves only the conversations that mapped to it.
                let mut best: Option<(usize, f64)> = None;
                for (i, target) in self.targets.iter().enumerate() {
                    if !flags[i] {
                        continue;
                    }
                    let mut hasher = Sha256::new();
                    hasher.update(conversation.to_be_bytes());
                    hasher.update(target.model.as_bytes());
                    let digest = hasher.finalize();
                    let bits = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"));
                    let u = ((bits >> 11) as f64 + 1.0) / 9_007_199_254_740_993.0;
                    let score = -u.ln() / f64::from(target.weight);
                    if best.is_none_or(|(_, s)| score < s) {
                        best = Some((i, score));
                    }
                }
                best.map_or(0, |(i, _)| i)
            }
        };
        &self.targets[index]
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A stable hash of a conversation: its first two messages, as the request carries them.
pub fn conversation_hash(request: &serde_json::Value) -> u64 {
    let mut hasher = Sha256::new();
    if let Some(messages) = request.get("messages").and_then(|m| m.as_array()) {
        for message in messages.iter().take(2) {
            hasher.update(message.to_string().as_bytes());
        }
    }
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn deployment(strategy: Strategy, weights: &[u32]) -> Deployment {
        Deployment::new(
            strategy,
            weights
                .iter()
                .enumerate()
                .map(|(i, w)| Target {
                    model: format!("p{i}/m"),
                    weight: *w,
                })
                .collect(),
        )
    }

    fn all(_: &str) -> bool {
        true
    }

    #[test]
    fn weighted_picks_each_target_its_weight_over_a_period_and_interleaves() {
        let d = deployment(Strategy::Weighted, &[5, 1, 1]);
        let mut seen: HashMap<String, u32> = HashMap::new();
        let mut order = Vec::new();
        for _ in 0..7 {
            let t = d.pick(&all, 0);
            *seen.entry(t.model.clone()).or_default() += 1;
            order.push(t.model.clone());
        }
        assert_eq!(seen["p0/m"], 5);
        assert_eq!(seen["p1/m"], 1);
        assert_eq!(seen["p2/m"], 1);
        // Smooth: the heavy target is not picked five times in a row.
        let longest_run = order
            .chunk_by(|a, b| a == b)
            .map(|run| run.len())
            .max()
            .unwrap();
        assert!(longest_run <= 3, "{order:?}");
        // The period repeats.
        let mut again: HashMap<String, u32> = HashMap::new();
        for _ in 0..7 {
            *again.entry(d.pick(&all, 0).model.clone()).or_default() += 1;
        }
        assert_eq!(seen, again);
    }

    #[test]
    fn round_robin_cycles_and_ordered_takes_the_first_usable() {
        let d = deployment(Strategy::RoundRobin, &[1, 1, 1]);
        let picks: Vec<_> = (0..6).map(|_| d.pick(&all, 0).model.clone()).collect();
        assert_eq!(picks, ["p0/m", "p1/m", "p2/m", "p0/m", "p1/m", "p2/m"]);
        let o = deployment(Strategy::Ordered, &[1, 1]);
        assert_eq!(o.pick(&all, 0).model, "p0/m");
        assert_eq!(o.pick(&|m| m != "p0/m", 0).model, "p1/m");
    }

    #[test]
    fn an_unusable_target_loses_its_share_and_when_none_is_usable_health_is_ignored() {
        let d = deployment(Strategy::Weighted, &[1, 1]);
        for _ in 0..6 {
            assert_eq!(d.pick(&|m| m != "p0/m", 0).model, "p1/m");
        }
        // Nothing usable: still a pick, never a refusal.
        let picked = d.pick(&|_| false, 0).model.clone();
        assert!(picked == "p0/m" || picked == "p1/m");
        let r = deployment(Strategy::RoundRobin, &[1, 1, 1]);
        let seen: Vec<_> = (0..4)
            .map(|_| r.pick(&|m| m != "p1/m", 0).model.clone())
            .collect();
        assert_eq!(seen, ["p0/m", "p2/m", "p0/m", "p2/m"]);
    }

    #[test]
    fn a_conversation_keeps_its_target_and_losing_one_moves_only_its_own() {
        let d = deployment(Strategy::Conversation, &[1, 1, 1]);
        let mut home = Vec::new();
        for c in 0..300_u64 {
            let first = d.pick(&all, c).model.clone();
            // Stable across calls (no state is consulted).
            assert_eq!(first, d.pick(&all, c).model);
            home.push(first);
        }
        // Roughly even over the three targets.
        for t in ["p0/m", "p1/m", "p2/m"] {
            let n = home.iter().filter(|h| h.as_str() == t).count();
            assert!((60..=140).contains(&n), "{t}: {n}");
        }
        // Skipping p1 re-homes only what was on p1.
        for (c, before) in home.iter().enumerate() {
            let after = d.pick(&|m| m != "p1/m", c as u64).model.clone();
            if before != "p1/m" {
                assert_eq!(&after, before, "conversation {c} moved needlessly");
            } else {
                assert_ne!(after, "p1/m");
            }
        }
    }

    #[test]
    fn the_conversation_hash_follows_the_first_two_messages_only() {
        let a = serde_json::json!({"messages": [{"role":"user","content":"hi"},
            {"role":"assistant","content":"yo"}, {"role":"user","content":"later"}]});
        let b = serde_json::json!({"messages": [{"role":"user","content":"hi"},
            {"role":"assistant","content":"yo"}, {"role":"user","content":"different"}]});
        let c = serde_json::json!({"messages": [{"role":"user","content":"other"}]});
        assert_eq!(conversation_hash(&a), conversation_hash(&b));
        assert_ne!(conversation_hash(&a), conversation_hash(&c));
    }
}
