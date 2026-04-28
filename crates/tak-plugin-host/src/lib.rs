//! Wasm plugin host runtime for tak-rs.
//!
//! Loads `tak:plugin@0.1.0` component-model plugins via wasmtime
//! and exposes a [`PluginHost`] that the firehose feeds events
//! into. Plugin invocation is **off the H1 hot path** — the host
//! drains a bounded mpsc on its own worker thread, so plugin
//! overload drops messages at the queue boundary instead of
//! stalling dispatch.
//!
//! # API in v0
//!
//! ```ignore
//! use tak_plugin_host::{PluginHost, PluginEvent};
//!
//! // At server startup:
//! let host = PluginHost::new(plugin_dir, num_workers).await?;
//!
//! // From the dispatch path:
//! host.publish(PluginEvent { /* ... */ });
//! ```
//!
//! `publish` is sync + non-blocking; it `try_send`s onto the
//! worker's channel and increments the dropped counter on full.
//!
//! See `docs/decisions/0004-wasm-plugins.md` for design intent.
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

mod bindings;
mod event;
mod host;

pub use event::{PluginAction, PluginEvent};
pub use host::{Plugin, PluginHost, PluginHostConfig};

/// Errors raised by the plugin host.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying wasm runtime error from wasmtime.
    #[error(transparent)]
    Wasmtime(#[from] wasmtime::Error),

    /// Catch-all for anyhow!()-shaped errors raised by the host
    /// (e.g. plugin-dir missing).
    #[error(transparent)]
    Other(#[from] anyhow::Error),

    /// Plugin file load / parse failure.
    #[error("plugin load {path}: {source}")]
    Load {
        /// Path that failed to load.
        path: std::path::PathBuf,
        /// Underlying error.
        #[source]
        source: wasmtime::Error,
    },

    /// Plugin's `init` returned `Err`. The plugin is marked failed
    /// and ignored.
    #[error("plugin init {name}: {message}")]
    InitFailed {
        /// Plugin's filename stem.
        name: String,
        /// Message the plugin returned.
        message: String,
    },

    /// Plugin worker channel was closed before the publish.
    #[error("plugin worker channel closed")]
    ChannelClosed,
}
