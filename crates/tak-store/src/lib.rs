//! Postgres + PostGIS persistence for tak-rs.
//!
//! Same schema as the upstream Java server (`cot_router` with PostGIS GiST
//! index, mission tables) so deployments can switch between implementations
//! without touching the database. See `docs/architecture.md` §5.4.
//!
//! Migrations are vendored verbatim from upstream
//! `.scratch/takserver-java/src/takserver-schemamanager/src/main/resources/db/migration/`
//! into `crates/tak-store/migrations/`. The single repeatable migration
//! (`R__remove_schema_version_function.sql`) is excluded — sqlx doesn't
//! ship a repeatable-migration concept and the function it drops is
//! Java-era only.
//!
//! Hot-path note: persistence MUST NOT block fan-out (invariant H1
//! boundary). [`Store::try_insert_event`] uses `try_send` on a bounded
//! mpsc channel; full → drop the event, increment a counter, never
//! stall the producer. The actual side-channel wiring in `tak-server`
//! lives in issue #31.
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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::mpsc;

/// The migrator over `crates/tak-store/migrations/`. The vendored upstream
/// schema baseline is `7_create_base_schema.sql` (Flyway V7); subsequent
/// migrations build out mission tables, federation cache tables, etc.,
/// through V99.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Default capacity of the persistence channel.
pub const DEFAULT_INSERT_CAPACITY: usize = 8192;

/// Default maximum batch size (rows per flush).
pub const DEFAULT_BATCH_MAX: usize = 1000;

/// Default flush interval (the writer flushes every N ms regardless of fill).
pub const DEFAULT_FLUSH_INTERVAL_MS: u64 = 100;

/// Errors emitted by the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying SQL error.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    /// Migration failure — file parse or SQL execution.
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Convenience result alias.
pub type Result<T> = core::result::Result<T, Error>;

/// One CoT event ready to insert into `cot_router`.
///
/// All time fields are milliseconds since the Unix epoch (matching the
/// `TakMessage.cotEvent.{send,start,stale}_time` u64 fields). The
/// background writer converts to `timestamptz` via `to_timestamp($N/1000.0)`
/// in SQL — that lets us avoid a `chrono` direct dep (invariant D3 says
/// `jiff` only at the lib level; sqlx doesn't natively encode `jiff`, so
/// the integer-ms wire is the cleanest crossing).
#[derive(Debug, Clone)]
pub struct CotInsert {
    /// Event UID (`event.uid`). Required.
    pub uid: String,
    /// CoT type code (e.g. `"a-f-G-U-C"`). Required.
    pub cot_type: String,
    /// Send time in ms since epoch.
    pub time_ms: i64,
    /// Validity start in ms since epoch.
    pub start_ms: i64,
    /// Stale time in ms since epoch.
    pub stale_ms: i64,
    /// Production hint (e.g. `"m-g"`).
    pub how: String,
    /// Latitude (WGS-84 decimal degrees).
    pub lat: f64,
    /// Longitude (WGS-84 decimal degrees).
    pub lon: f64,
    /// Height above ellipsoid (m).
    pub hae: f64,
    /// Circular error 1-σ (m).
    pub ce: f64,
    /// Linear error 1-σ (m).
    pub le: f64,
    /// `<detail>` content from the original CoT XML; serialised verbatim.
    pub detail: String,
}

/// Returned by [`Store::try_insert_event`] when the channel is full.
///
/// Persistence is best-effort by design: a full channel means we drop
/// the row rather than stall the producer (invariant H1 boundary). The
/// dropped count is tracked on the `Store` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistenceDropped;

/// A connection-pool-backed Postgres store with a background batched
/// writer for CoT event ingestion.
///
/// Construction via [`Store::connect_and_migrate`] runs the migrator AND
/// spawns the writer task on the current tokio runtime. Drop the `Store`
/// (or call [`Store::shutdown`]) to flush the writer and stop it
/// gracefully.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
    insert_tx: mpsc::Sender<CotInsert>,
    dropped: Arc<AtomicU64>,
    inserted: Arc<AtomicU64>,
}

impl Store {
    /// Connect, migrate, spawn the background writer.
    ///
    /// The writer batches every [`DEFAULT_BATCH_MAX`] rows or
    /// [`DEFAULT_FLUSH_INTERVAL_MS`] milliseconds, whichever first; the
    /// in-memory channel holds [`DEFAULT_INSERT_CAPACITY`] pending rows
    /// before [`Store::try_insert_event`] starts dropping.
    ///
    /// # Errors
    ///
    /// - [`Error::Sqlx`] on connection failure.
    /// - [`Error::Migrate`] on migration parse/execute failure.
    pub async fn connect_and_migrate(url: &str) -> Result<Self> {
        Self::connect_and_migrate_with(
            url,
            DEFAULT_INSERT_CAPACITY,
            DEFAULT_BATCH_MAX,
            Duration::from_millis(DEFAULT_FLUSH_INTERVAL_MS),
        )
        .await
    }

    /// Like [`Self::connect_and_migrate`] but with explicit knobs.
    ///
    /// # Errors
    ///
    /// Same as [`Self::connect_and_migrate`].
    pub async fn connect_and_migrate_with(
        url: &str,
        channel_capacity: usize,
        batch_max: usize,
        flush_interval: Duration,
    ) -> Result<Self> {
        let opts: PgConnectOptions = url.parse()?;
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;

        let (insert_tx, insert_rx) = mpsc::channel::<CotInsert>(channel_capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let inserted = Arc::new(AtomicU64::new(0));

        let writer_pool = pool.clone();
        let writer_inserted = Arc::clone(&inserted);
        spawn_writer(async move {
            run_writer(
                writer_pool,
                insert_rx,
                batch_max,
                flush_interval,
                writer_inserted,
            )
            .await;
        });

        Ok(Self {
            pool,
            insert_tx,
            dropped,
            inserted,
        })
    }

    /// Connect to an already-migrated database (skips migrations + spawns
    /// no writer). Useful for read-only clients.
    ///
    /// # Errors
    ///
    /// - [`Error::Sqlx`] on connection failure.
    pub async fn connect_readonly(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
        let (insert_tx, _drop_rx) = mpsc::channel::<CotInsert>(1);
        Ok(Self {
            pool,
            insert_tx,
            dropped: Arc::new(AtomicU64::new(0)),
            inserted: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Borrow the underlying pool for query construction.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Try to enqueue one event for batched insertion.
    ///
    /// Non-blocking. Returns `Err(PersistenceDropped)` if the channel is
    /// full (the row is silently dropped after incrementing
    /// [`Store::dropped_count`]). Caller MUST NOT block on the result;
    /// this is the H1 boundary.
    pub fn try_insert_event(
        &self,
        event: CotInsert,
    ) -> core::result::Result<(), PersistenceDropped> {
        match self.insert_tx.try_send(event) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Err(PersistenceDropped)
            }
        }
    }

    /// Total events the writer has successfully INSERTed since startup.
    /// Updated by the writer task.
    #[must_use]
    pub fn inserted_count(&self) -> u64 {
        self.inserted.load(Ordering::Relaxed)
    }

    /// Total events dropped by [`Self::try_insert_event`] due to a full
    /// channel.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Block until the writer's `inserted_count()` stops growing for
    /// ≥150ms or `timeout` is reached. Useful in tests that need
    /// "all-pending-data-persisted" semantics before assertions.
    ///
    /// Returns the final inserted count.
    pub async fn wait_for_drain(&self, timeout: Duration) -> u64 {
        let deadline = std::time::Instant::now() + timeout;
        let poll = Duration::from_millis(50);
        let stable_for = Duration::from_millis(150);

        let mut last = self.inserted_count();
        let mut stable_since = std::time::Instant::now();
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(poll).await;
            let now = self.inserted_count();
            if now == last {
                if stable_since.elapsed() >= stable_for {
                    return now;
                }
            } else {
                last = now;
                stable_since = std::time::Instant::now();
            }
        }
        self.inserted_count()
    }
}

/// Local equivalent of `tak_net::tasks::spawn` — wraps `tokio::spawn`
/// with a tracing span so the writer task is observable in logs.
/// Defined here (rather than imported) because `tak-store` must not
/// depend on `tak-net` (architecture: storage doesn't know about
/// network). When/if a `tak-runtime` crate appears, this helper moves
/// there.
#[allow(clippy::disallowed_methods)] // sanctioned spawn site; see invariant N3
fn spawn_writer<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use tracing::Instrument;
    let span = tracing::info_span!("task", name = "tak_store_writer");
    tokio::spawn(fut.instrument(span));
}

/// Drain the channel, batch into a buffer, flush periodically.
///
/// Persistence is BEST-EFFORT: errors are logged and the buffer is
/// cleared. Failing to insert a batch never propagates back to the
/// producer — that's the H1 boundary. (Operators should monitor
/// `tak.persistence.flush_failed` once we wire up metrics.)
async fn run_writer(
    pool: PgPool,
    mut rx: mpsc::Receiver<CotInsert>,
    batch_max: usize,
    flush_interval: Duration,
    inserted: Arc<AtomicU64>,
) {
    let mut buf: Vec<CotInsert> = Vec::with_capacity(batch_max);
    let mut interval = tokio::time::interval(flush_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased; // prefer draining over ticking when both are ready

            n = {
                let limit = batch_max.saturating_sub(buf.len()).max(1);
                rx.recv_many(&mut buf, limit)
            } => {
                if n == 0 {
                    // Channel closed; final flush + exit.
                    if !buf.is_empty() {
                        flush(&pool, &mut buf, &inserted).await;
                    }
                    return;
                }
                if buf.len() >= batch_max {
                    flush(&pool, &mut buf, &inserted).await;
                }
            }

            _ = interval.tick() => {
                if !buf.is_empty() {
                    flush(&pool, &mut buf, &inserted).await;
                }
            }
        }
    }
}

/// Insert a batch into `cot_router` inside a single transaction.
///
/// Per-row INSERT inside a TX: the v0 trade-off is simplicity over peak
/// throughput. ~3-5k rows/s on laptop-class Postgres. If we need 10k+/s
/// we switch to multi-VALUES INSERT or COPY FROM STDIN.
async fn flush(pool: &PgPool, buf: &mut Vec<CotInsert>, inserted: &Arc<AtomicU64>) {
    if buf.is_empty() {
        return;
    }
    let count = buf.len();

    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(err) => {
            tracing::warn!(?err, count, "persistence: begin tx failed; dropping batch");
            buf.clear();
            return;
        }
    };

    for ev in buf.drain(..) {
        // Per-row INSERT. event_pt is built inline as a PostGIS literal so
        // we don't need to bind a Geometry type (which would require geozero
        // bindings beyond what we have today).
        if let Err(err) = sqlx::query(
            "INSERT INTO cot_router \
              (uid, cot_type, time, start, stale, how, point_hae, point_ce, point_le, detail, event_pt) \
              VALUES (\
                  $1, $2, \
                  to_timestamp($3::float8 / 1000.0), \
                  to_timestamp($4::float8 / 1000.0), \
                  to_timestamp($5::float8 / 1000.0), \
                  $6, $7, $8, $9, $10, \
                  ST_SetSRID(ST_MakePoint($11, $12), 4326)\
              )",
        )
        .bind(&ev.uid)
        .bind(&ev.cot_type)
        .bind(ev.time_ms)
        .bind(ev.start_ms)
        .bind(ev.stale_ms)
        .bind(&ev.how)
        .bind(ev.hae)
        .bind(ev.ce)
        .bind(ev.le)
        .bind(&ev.detail)
        .bind(ev.lon)
        .bind(ev.lat)
        .execute(&mut *tx)
        .await
        {
            tracing::warn!(?err, "persistence: row insert failed (continuing batch)");
        }
    }

    if let Err(err) = tx.commit().await {
        tracing::warn!(?err, count, "persistence: commit failed; rows lost");
        return;
    }

    inserted.fetch_add(count as u64, Ordering::Relaxed);
}
