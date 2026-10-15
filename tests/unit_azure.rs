#![cfg(feature = "azure")]
//! Offline v1 wire and explicit selected-identity contracts.
use llmshim::{provider::Provider, providers::azure::*};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
fn provider() -> Azure {
    Azure::new(
        "https://fixture.openai.azure.com",
        AzureIdentity::api_key("fixture", "test-key").unwrap(),
    )
    .unwrap()
    .with_deployment("chat-deployment", "gpt-6-sol")
    .unwrap()
}
#[path = "azure/credentials.rs"]
mod credentials;
#[path = "azure/protocol.rs"]
mod protocol;
