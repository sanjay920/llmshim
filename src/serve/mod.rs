//! Config-file-driven server mode (`llmshim serve`), behind the default-off
//! `serve` feature. In development: the configuration contract and its
//! validation exist; serving from the file does not yet.
//!
//! **Unstable.** Until this feature is declared stable, its types, file format
//! and command are exempt from the crate's semver promise and may change in
//! any release.
//!
//! Nothing in this module is reachable from a default build, and nothing here
//! changes `llmshim proxy` or `llmshim gateway`.
//!
//! Roadmap and decisions: `docs/design/enterprise-gateway/README.md`.

pub mod build;
pub mod config;
pub mod keys;

pub use build::{admin_token_digest, build_app, fleet_redis_url, start_fleet_workers, Built};
pub use config::{Format, Issue, ServeConfig};
pub use keys::build_key_store;
