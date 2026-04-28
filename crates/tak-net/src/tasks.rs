//! Spawn helper for invariant **N3** (`docs/invariants.md`).
//!
//! Every async task in tak-rs runs inside a tracing span so production
//! observers can attribute load and follow causality across awaits.
//! [`spawn`] is the only sanctioned way to start a tokio task in lib code;
//! `tokio::spawn` is denied via `clippy.toml`.

use std::future::Future;

use tokio::task::JoinHandle;
use tracing::Instrument;

/// Spawn a named, span-attached tokio task.
///
/// `name` is recorded as a span field so log lines from inside the future
/// are tagged. The span follows the future across every `.await` via
/// `tracing::Instrument`, so observability is preserved even when the
/// future is suspended on a tokio runtime worker thread different from
/// the one that started it.
///
/// # Example
/// ```no_run
/// # use tak_net::tasks;
/// # async fn ex() {
/// let handle = tasks::spawn("hello", async {
///     tracing::info!("running inside the span");
///     42
/// });
/// let n = handle.await.unwrap();
/// assert_eq!(n, 42);
/// # }
/// ```
#[allow(clippy::disallowed_methods)]
// `tokio::spawn` is denied workspace-wide; this wrapper IS the sanctioned
// entry point — see invariant N3.
pub fn spawn<F>(name: &'static str, fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let span = tracing::info_span!("task", name = name);
    tokio::spawn(fut.instrument(span))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[tokio::test]
    async fn spawn_runs_future_to_completion() {
        let h = spawn("test-spawn", async { 7 + 35 });
        assert_eq!(h.await.unwrap(), 42);
    }

    #[tokio::test]
    async fn spawn_propagates_panics_via_join() {
        let h = spawn("test-panic", async {
            panic!("intentional");
        });
        assert!(h.await.is_err(), "panicking task must produce a JoinError");
    }
}
