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
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::Error;
use crate::bindings::{TakPlugin, clock, inbound::Action, inbound::CotEvent, log};
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
    /// Capacity of the shared outbound channel that carries
    /// [`Action::Replace`] verdicts back to the firehose for
    /// re-dispatch. Replace is documented as rare (decision 0004),
    /// so 1 024 slots is plenty; full = drop the replacement at
    /// the channel boundary.
    pub outbound_capacity: usize,
}

impl Default for PluginHostConfig {
    fn default() -> Self {
        Self {
            plugin_dir: PathBuf::from("./plugins"),
            queue_capacity: 2_500,
            outbound_capacity: 1_024,
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
/// plugin set, plus the shared outbound channel for
/// [`Action::Replace`] verdicts.
#[derive(Debug)]
pub struct PluginHost {
    plugins: Vec<Plugin>,
    /// Receiver for replacement frames produced by plugins. Taken
    /// once at startup by the firehose's re-dispatch task; `None`
    /// after that.
    outbound_rx: Option<mpsc::Receiver<Bytes>>,
    /// Counts replacement frames dropped because the outbound
    /// channel was full when the worker tried to push them. Useful
    /// for an operator-visible metric without per-plugin counters.
    outbound_dropped: Arc<AtomicU64>,
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
        // `max-cpu-ms-per-msg` budget from decision 0004. The
        // ticker thread (spawned just below) increments the engine
        // epoch every TICK_INTERVAL; per-call deadlines in
        // `run_worker` count those ticks.
        wasm_config.epoch_interruption(true);
        let engine = Engine::new(&wasm_config)?;
        spawn_epoch_ticker(&engine).map_err(|e| Error::Other(anyhow::Error::new(e)))?;

        let (outbound_tx, outbound_rx) = mpsc::channel::<Bytes>(config.outbound_capacity);
        let outbound_dropped = Arc::new(AtomicU64::new(0));

        let mut plugins = Vec::new();
        let entries = std::fs::read_dir(&config.plugin_dir)
            .map_err(|e| Error::Other(anyhow::Error::new(e)))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "wasm") {
                continue;
            }
            match Self::load_plugin(
                &engine,
                &path,
                &config,
                outbound_tx.clone(),
                outbound_dropped.clone(),
            )
            .await
            {
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
        Ok(Self {
            plugins,
            outbound_rx: Some(outbound_rx),
            outbound_dropped,
        })
    }

    /// Take the outbound replacement-frame receiver. The firehose
    /// is expected to call this exactly once at startup and spawn
    /// a re-dispatch task that drains it. Returns `None` on every
    /// call after the first.
    ///
    /// Replacement frames are produced by plugins returning
    /// [`Action::Replace`]; the host validates nothing — the
    /// receiver is responsible for `framing::decode_stream` +
    /// `TakMessage::decode` before re-dispatching.
    pub fn take_outbound(&mut self) -> Option<mpsc::Receiver<Bytes>> {
        self.outbound_rx.take()
    }

    /// Total replacement frames dropped because the outbound
    /// channel was full. Counter is shared across all plugins
    /// (per-plugin attribution is future work).
    #[must_use]
    pub fn outbound_dropped_count(&self) -> u64 {
        self.outbound_dropped.load(Ordering::Relaxed)
    }

    async fn load_plugin(
        engine: &Engine,
        path: &Path,
        config: &PluginHostConfig,
        outbound_tx: mpsc::Sender<Bytes>,
        outbound_dropped: Arc<AtomicU64>,
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
                outbound_tx,
                outbound_dropped,
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

/// How often the epoch ticker bumps the engine. A 1 ms cadence
/// means `[limits].max-cpu-ms-per-msg = N` translates roughly to
/// "trap after N ms of plugin CPU time" — coarse but fine for the
/// "stop a runaway plugin" use case.
const TICK_INTERVAL: Duration = Duration::from_millis(1);

/// Initial CPU budget for `init()`. Decision 0004 says init must
/// return within 1 s; we encode that as the tick count.
const INIT_DEADLINE_TICKS: u64 = 1_000;

/// Spawn the epoch ticker. Holds an [`EngineWeak`] so it auto-
/// exits when the last [`Engine`] clone (one per worker) drops.
/// Runs on its own OS thread rather than a tokio task because:
///
/// - `Engine::increment_epoch` is signal-safe — no need for
///   async.
/// - Workers run on `spawn_blocking`; if the tokio reactor ever
///   stalls under load the ticker still fires, preserving the
///   deadline guarantee.
///
/// Failure to spawn the ticker is fatal: with
/// `epoch_interruption=true` and no ticker, every plugin call
/// would trap on the first epoch check. Caller should treat this
/// as a hard host bringup error.
fn spawn_epoch_ticker(engine: &Engine) -> std::io::Result<()> {
    let weak = engine.weak();
    std::thread::Builder::new()
        .name("tak-plugin-epoch".to_owned())
        .spawn(move || {
            loop {
                std::thread::sleep(TICK_INTERVAL);
                match weak.upgrade() {
                    Some(eng) => eng.increment_epoch(),
                    None => break,
                }
            }
            debug!("plugin-host: epoch ticker exited (engine dropped)");
        })?;
    Ok(())
}

// `run_worker` parks a thread for the lifetime of the plugin and
// owns several heavy resources (Component + Linker + Engine
// clone); bundling them into a struct just to placate the
// "too_many_arguments" lint adds boilerplate without making the
// code easier to read. Internal fn, narrow blast radius.
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn run_worker(
    name: String,
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
    plugin_cfg: PluginConfig,
    mut rx: mpsc::Receiver<PluginEvent>,
    outbound_tx: mpsc::Sender<Bytes>,
    outbound_dropped: Arc<AtomicU64>,
) {
    // Deny-everything WasiCtx: no preopens, no env, no stdio
    // inheritance. Plugins that try to print() or read clock_now()
    // see deterministic empty results / errors.
    let wasi = WasiCtxBuilder::new().build();

    let init_json = plugin_cfg.init_json().to_owned();
    let mem_cap_bytes = plugin_cfg.memory_bytes_cap();
    let cpu_budget_ticks = u64::from(plugin_cfg.limits.max_cpu_ms_per_msg).max(1);

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

    // Init gets a fixed 1 s budget per the design doc. Per-call
    // budget is the operator-supplied `max-cpu-ms-per-msg`.
    store.set_epoch_deadline(INIT_DEADLINE_TICKS);
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
        // Reset the deadline every call — it's relative to the
        // current epoch, so without reset the budget collapses to
        // zero after the first call.
        store.set_epoch_deadline(cpu_budget_ticks);
        match bindings
            .tak_plugin_inbound()
            .call_on_inbound(&mut store, &cot)
        {
            Ok(action) => match action {
                Action::Pass | Action::Drop => {
                    // Pass + Drop are no-ops on the secondary
                    // path: the original frame already went out
                    // through the bus before this worker even
                    // saw it (decision 0004 invariant — plugins
                    // observe AFTER primary dispatch). The
                    // distinction matters only for plugin-
                    // internal logging.
                }
                Action::Replace(new_bytes) => {
                    // Replacement frame — re-dispatch as a new
                    // event. Vec<u8> from wasm becomes a Bytes
                    // here (one-time alloc on the cold path;
                    // Replace is documented as rare). Receiver
                    // is in the firehose; if it's full we
                    // drop and bump the metric rather than
                    // backpressure the worker.
                    let frame = Bytes::from(new_bytes);
                    if let Err(mpsc::error::TrySendError::Full(_)) = outbound_tx.try_send(frame) {
                        outbound_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            },
            Err(e) => {
                // Once a wasmtime component traps (CPU budget,
                // memory limit, panic, anything) it can't be
                // re-entered — the next call returns "cannot enter
                // component instance" forever. So the only sane
                // recovery is to unload the worker. A retry/strike
                // counter sounds appealing but only spams the log
                // with cascading entry failures; future work can
                // reinstantiate the Store on trap to actually
                // recover.
                warn!(
                    plugin = %name,
                    processed,
                    error = ?e,
                    "plugin-host: on_inbound trapped; unloading worker (component dead until restart)"
                );
                break;
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
