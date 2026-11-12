//! Config-file-driven server mode (`llmshim serve`), behind the default-off
//! `serve` feature. In development: the configuration contract and its
//! validation exist; serving from the file does not yet.
//!
//! Nothing in this module is reachable from a default build, and nothing here
//! changes `llmshim proxy` or `llmshim gateway`.
//!
//! Roadmap and decisions: `docs/design/enterprise-gateway/README.md`.

pub mod config;

pub use config::{Format, Issue, ServeConfig};
