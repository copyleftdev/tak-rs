//! `CoreConfig.xml` subset parser.
//!
//! Reads only the sections we support; **errors hard on unknown sections**
//! rather than silently ignoring them — the latter caused real production
//! pain in the Java server. See `docs/architecture.md` §5.8.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]
#![warn(missing_docs, missing_debug_implementations)]

/// Errors emitted while parsing config.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// XML parse failure.
    #[error("xml: {0}")]
    Xml(String),

    /// Unsupported section encountered. By policy we hard-fail rather than ignore.
    #[error("unsupported config section: {0}")]
    UnsupportedSection(String),
}
