#![cfg(feature = "vertex")]
//! Offline cloud protocol and explicit-identity contracts.
use llmshim::{provider::Provider, providers::vertex::*};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
struct Fixed;
impl TokenSource for Fixed {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(async { AccessToken::new("fixture-token", Duration::from_secs(3600)) })
    }
}
fn identity(region: &str) -> GoogleIdentity {
    GoogleIdentity::token_source("fixture-project", region, "fixture", Arc::new(Fixed))
}
#[path = "vertex/credentials.rs"]
mod credentials;
#[path = "vertex/exchange.rs"]
mod exchange;
#[path = "vertex/pricing.rs"]
mod pricing;
#[path = "vertex/protocol.rs"]
mod protocol;
#[path = "vertex/replay.rs"]
mod replay;
#[path = "vertex/stream_replay.rs"]
mod stream_replay;
