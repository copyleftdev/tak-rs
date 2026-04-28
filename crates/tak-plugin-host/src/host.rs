//! Wasmtime engine + per-plugin worker.
//!
//! Lifecycle:
//!
//! 1. [`PluginHost::new`] builds a wasmtime [`Engine`] (one shared
//!    across all plugins — JIT cache amortisation), loads each
//!    `*.wasm` from the configured directory, instantiates a
//!    [`Plugin`] per file, calls its `init`.
//! 2. Each [`Plugin`] owns a tokio mpsc receiver. A worker task
//!    drains the receiver and invokes the plugin's `on-inbound`
//!    inside a wasmtime [`Store`]. The store is per-task so we
//!    don't need cross-thread synchronisation around it.
//! 3. The firehose calls [`PluginHost::publish`] (sync,
//!    non-blocking). It `try_send`s onto every plugin's queue;
//!    full queues drop the event and bump
//!    `tak.plugin.<name>.dropped`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};

use crate::Error;
use crate::bindings::{TakPlugin, clock, inbound::CotEvent, log};
use crate::event::{PluginAction, PluginEvent};

/// Operator-tunable knobs for the plugin host.
#[derive(Debug, Clone)]
pub struct PluginHostConfig {
    /// Directory scanned for `*.wasm` plugin files.
    pub plugin_dir: PathBuf,
    /// Per-plugin queue depth. When full, new events are dropped
    /// at the queue boundary. Sized so that ~50 ms of plugin lag
    /// at 50 k msg/s = 2 500 events fits.
    pub queue_capacity: usize,
}

impl Default for PluginHostConfig {
    fn default() -> Self {
        Self {
            plugin_dir: PathBuf::from("./plugins"),
            queue_capacity: 2_500,
        }
    }
}

/// One loaded plugin.
#[derive(Debug)]
pub struct Plugin {
    /// Filename stem, e.g. `geofence-redact` for
    /// `geofence-redact.wasm`. Used in metric names + log fields.
    pub name: String,
    /// Sender side of the worker mpsc. `try_send` drops on full.
    tx: mpsc::Sender<PluginEvent>,
    /// Total events dropped at the queue boundary because the
    /// worker couldn't keep up.
    dropped: Arc<AtomicU64>,
}

impl Plugin {
    /// Best-effort enqueue. Returns `false` and bumps the
    /// per-plugin dropped counter if the worker queue is full.
    pub fn try_publish(&self, event: PluginEvent) -> bool {
        match self.tx.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Total events dropped at the queue boundary since startup.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The plugin host — owns the wasmtime engine and the loaded
/// plugin set.
#[derive(Debug)]
pub struct PluginHost {
    plugins: Vec<Plugin>,
}

impl PluginHost {
    /// Scan `config.plugin_dir`, load each `*.wasm` as a
    /// [`Plugin`], spawn a worker task per plugin.
    ///
    /// Plugins whose `init` returns `Err` are logged and skipped
    /// (host still comes up).
    ///
    /// # Errors
    ///
    /// - The plugin directory doesn't exist.
    /// - The wasmtime engine couldn't be built.
    pub async fn new(config: PluginHostConfig) -> Result<Self, Error> {
        if !config.plugin_dir.is_dir() {
            return Err(Error::Other(anyhow::anyhow!(
                "plugin dir {:?} does not exist",
                config.plugin_dir
            )));
        }

        // One engine across all plugins — wasmtime shares JIT
        // caches when components come from the same engine, which
        // matters when an operator hot-reloads a plugin (rebuilt
        // version reuses cached compilation).
        let mut wasm_config = Config::new();
        wasm_config.wasm_component_model(true);
        wasm_config.epoch_interruption(true);
        let engine = Engine::new(&wasm_config)?;

        let mut plugins = Vec::new();
        let entries = std::fs::read_dir(&config.plugin_dir)
            .map_err(|e| Error::Other(anyhow::Error::new(e)))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "wasm") {
                continue;
            }
            match Self::load_plugin(&engine, &path, &config).await {
                Ok(plugin) => {
                    info!(
                        plugin = %plugin.name,
                        path = %path.display(),
                        "plugin-host: loaded"
                    );
                    plugins.push(plugin);
                }
                Err(e) => {
                    warn!(path = %path.display(), error = ?e, "plugin-host: skipped");
                }
            }
        }

        info!(loaded = plugins.len(), "plugin-host: ready");
        Ok(Self { plugins })
    }

    async fn load_plugin(
        engine: &Engine,
        path: &Path,
        config: &PluginHostConfig,
    ) -> Result<Plugin, Error> {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_owned();

        let component = Component::from_file(engine, path).map_err(|e| Error::Load {
            path: path.to_path_buf(),
            source: e,
        })?;

        let mut linker = Linker::<HostState>::new(engine);
        log::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |st| st).map_err(|e| {
            Error::Load {
                path: path.to_path_buf(),
                source: e,
            }
        })?;
        clock::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |st| st).map_err(
            |e| Error::Load {
                path: path.to_path_buf(),
                source: e,
            },
        )?;

        let (tx, rx) = mpsc::channel::<PluginEvent>(config.queue_capacity);
        let dropped = Arc::new(AtomicU64::new(0));

        // Spawn the worker. Owns the Store, Component, and Linker.
        let engine_for_worker = engine.clone();
        let component_for_worker = component;
        let linker_for_worker = linker;
        let name_for_worker = name.clone();
        let _dropped_for_worker = dropped.clone();
        #[allow(clippy::disallowed_methods)]
        tokio::task::spawn_blocking(move || {
            run_worker(
                name_for_worker,
                engine_for_worker,
                component_for_worker,
                linker_for_worker,
                rx,
            );
        });

        Ok(Plugin { name, tx, dropped })
    }

    /// Best-effort enqueue to every loaded plugin. Returns the
    /// count of plugins that accepted the event (others either
    /// rejected for full queue or had failed init).
    ///
    /// Sync + non-blocking — safe to call from the H1 hot path.
    /// Caller passes the event by value because each plugin needs
    /// its own clone (`Bytes::clone` is an Arc bump per H3).
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn publish(&self, event: PluginEvent) -> usize {
        let mut accepted = 0;
        for plugin in &self.plugins {
            if plugin.try_publish(event.clone()) {
                accepted += 1;
            }
        }
        accepted
    }

    /// Number of plugins that successfully loaded and are
    /// receiving events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// True iff no plugins loaded successfully.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

#[allow(clippy::needless_pass_by_value)] // takes ownership of Component+Linker for the worker's lifetime
fn run_worker(
    name: String,
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
    mut rx: mpsc::Receiver<PluginEvent>,
) {
    let mut store = Store::new(
        &engine,
        HostState {
            plugin_name: name.clone(),
            started_at: std::time::Instant::now(),
        },
    );

    let bindings = match TakPlugin::instantiate(&mut store, &component, &linker) {
        Ok(b) => b,
        Err(e) => {
            warn!(plugin = %name, error = ?e, "plugin-host: instantiate failed; worker exiting");
            return;
        }
    };

    // Plugins get an empty config in v0; per-plugin TOML is
    // future work.
    if let Err(e) = bindings.tak_plugin_inbound().call_init(&mut store, "{}") {
        warn!(plugin = %name, error = ?e, "plugin-host: init() trapped; worker exiting");
        return;
    }

    let mut processed = 0u64;
    while let Some(event) = rx.blocking_recv() {
        let cot = CotEvent {
            wire_bytes: event.payload.to_vec(),
            cot_type: event.cot_type,
            uid: event.uid,
            callsign: event.callsign,
            lat: event.lat,
            lon: event.lon,
            hae: event.hae,
            send_time_ms: event.send_time_ms,
            sender_groups_low: event.sender_groups_low,
        };
        match bindings
            .tak_plugin_inbound()
            .call_on_inbound(&mut store, &cot)
        {
            Ok(_action) => {
                // v0: we observe but don't act on the action yet.
                // The firehose-side wiring (which interprets
                // Pass/Drop/Replace) lands in a follow-up commit.
            }
            Err(e) => {
                warn!(plugin = %name, error = ?e, "plugin-host: on_inbound trapped");
            }
        }
        processed += 1;
    }
    let _ = bindings.tak_plugin_inbound().call_shutdown(&mut store);
    debug!(plugin = %name, processed, "plugin-host: worker exited");
}

/// Per-store host state. Imports use this to stash anything
/// scoped to a single plugin instance.
#[derive(Debug)]
struct HostState {
    plugin_name: String,
    started_at: std::time::Instant,
}

impl log::Host for HostState {
    fn emit(&mut self, level: log::Level, message: String) {
        match level {
            log::Level::Trace => {
                tracing::trace!(plugin = %self.plugin_name, "{message}");
            }
            log::Level::Debug => {
                tracing::debug!(plugin = %self.plugin_name, "{message}");
            }
            log::Level::Info => {
                tracing::info!(plugin = %self.plugin_name, "{message}");
            }
            log::Level::Warn => {
                tracing::warn!(plugin = %self.plugin_name, "{message}");
            }
            log::Level::Error => {
                tracing::error!(plugin = %self.plugin_name, "{message}");
            }
        }
    }
}

impl clock::Host for HostState {
    fn now_ms(&mut self) -> u64 {
        // Monotonic ms since plugin load. u64 wraparound is
        // ~584 million years — not a practical concern.
        u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

// Convert wasm-component `Action` → host `PluginAction`. v0 doesn't
// use this yet, but it's defined so the firehose-side wiring is a
// pure addition.
#[allow(dead_code)]
fn lift_action(action: crate::bindings::inbound::Action) -> PluginAction {
    use crate::bindings::inbound::Action as A;
    match action {
        A::Pass => PluginAction::Pass,
        A::Drop => PluginAction::Drop,
        A::Replace(bytes) => PluginAction::Replace(bytes::Bytes::from(bytes)),
    }
}
