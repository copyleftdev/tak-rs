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
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::Error;
use crate::bindings::{TakPlugin, clock, inbound::CotEvent, log};
use crate::config::PluginConfig;
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
        // Epoch interruption is the mechanism behind the per-plugin
        // `max-cpu-ms-per-msg` budget from decision 0004. We don't
        // wire the budget enforcer yet — turning the flag on
        // without a ticker traps every plugin call immediately.
        // The per-plugin TOML + epoch ticker land together in a
        // follow-up commit.
        wasm_config.epoch_interruption(false);
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
                Ok(Some(plugin)) => {
                    info!(
                        plugin = %plugin.name,
                        path = %path.display(),
                        "plugin-host: loaded"
                    );
                    plugins.push(plugin);
                }
                Ok(None) => {
                    // Already-logged: disabled by config.
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
    ) -> Result<Option<Plugin>, Error> {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_owned();

        // Per-plugin TOML lives next to the wasm. Missing file =
        // defaults; bad TOML is a hard error so a typo doesn't
        // silently fall back to "no config" and surprise the
        // operator.
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let plugin_cfg = match PluginConfig::try_load(dir, &name)? {
            Some((cfg, cfg_path)) => {
                if let Some(declared) = cfg.plugin.name.as_deref()
                    && declared != name
                {
                    warn!(
                        plugin = %name,
                        declared,
                        cfg = %cfg_path.display(),
                        "plugin-host: name in TOML doesn't match wasm filename stem; using stem"
                    );
                }
                debug!(plugin = %name, cfg = %cfg_path.display(), "plugin-host: loaded TOML config");
                cfg
            }
            None => {
                debug!(plugin = %name, "plugin-host: no TOML config; using defaults");
                PluginConfig::default()
            }
        };

        if !plugin_cfg.plugin.enabled {
            info!(plugin = %name, "plugin-host: disabled by config; skipping");
            return Ok(None);
        }

        let component = Component::from_file(engine, path).map_err(|e| Error::Load {
            path: path.to_path_buf(),
            source: e,
        })?;

        let mut linker = Linker::<HostState>::new(engine);

        // Plugins built for `wasm32-wasip2` import the WASI 0.2
        // surface from their std library even when they don't use
        // it. We satisfy those imports with a deny-everything
        // WasiCtx (no preopens, no env, no inherited stdio); the
        // capabilities described in decision 0004 are still
        // enforced because the WasiCtx exposes nothing the plugin
        // can act on.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| Error::Load {
            path: path.to_path_buf(),
            source: e,
        })?;

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
        let plugin_cfg_for_worker = plugin_cfg;
        let _dropped_for_worker = dropped.clone();
        #[allow(clippy::disallowed_methods)]
        tokio::task::spawn_blocking(move || {
            run_worker(
                name_for_worker,
                engine_for_worker,
                component_for_worker,
                linker_for_worker,
                plugin_cfg_for_worker,
                rx,
            );
        });

        Ok(Some(Plugin { name, tx, dropped }))
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
    plugin_cfg: PluginConfig,
    mut rx: mpsc::Receiver<PluginEvent>,
) {
    // Deny-everything WasiCtx: no preopens, no env, no stdio
    // inheritance. Plugins that try to print() or read clock_now()
    // see deterministic empty results / errors.
    let wasi = WasiCtxBuilder::new().build();

    let init_json = plugin_cfg.init_json().to_owned();
    let mem_cap_bytes = plugin_cfg.memory_bytes_cap();

    let mut store = Store::new(
        &engine,
        HostState {
            plugin_name: name.clone(),
            started_at: std::time::Instant::now(),
            wasi,
            wasi_table: ResourceTable::new(),
            limiter: MemoryLimiter::new(mem_cap_bytes),
        },
    );
    // Wire the per-store ResourceLimiter so any linear-memory grow
    // beyond the configured cap fails inside wasmtime. The wasm
    // module sees the alloc as Err and decides what to do — most
    // plugins will trap, which we surface as an `on_inbound`
    // failure log.
    store.limiter(|st| &mut st.limiter);

    let bindings = match TakPlugin::instantiate(&mut store, &component, &linker) {
        Ok(b) => b,
        Err(e) => {
            warn!(plugin = %name, error = ?e, "plugin-host: instantiate failed; worker exiting");
            return;
        }
    };

    if let Err(e) = bindings
        .tak_plugin_inbound()
        .call_init(&mut store, &init_json)
    {
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
/// scoped to a single plugin instance, including the deny-
/// everything WASI context required to satisfy
/// `wasm32-wasip2` plugins' transitive WASI imports.
struct HostState {
    plugin_name: String,
    started_at: std::time::Instant,
    wasi: WasiCtx,
    wasi_table: ResourceTable,
    limiter: MemoryLimiter,
}

/// Caps wasm linear-memory growth at the configured byte budget.
/// Other resource classes (tables, instances) are left at wasmtime
/// defaults — the threat we're fencing is "plugin allocates 4 GiB
/// and the host OOMs," not "plugin creates 50 tables".
#[derive(Debug)]
struct MemoryLimiter {
    max_memory_bytes: usize,
}

impl MemoryLimiter {
    fn new(max_memory_bytes: usize) -> Self {
        Self { max_memory_bytes }
    }
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= self.max_memory_bytes)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.wasi_table,
        }
    }
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
