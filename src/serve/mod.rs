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

mod access_log;
pub mod build;
pub mod config;
mod jsonl;
pub mod keys;
pub mod mtls;

pub use build::{build_app, Built};
pub use config::{Format, Issue, ServeConfig};
pub use keys::build_key_store;
