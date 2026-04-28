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
//! boundary). Producers push into a bounded channel; if full, persistence
//! is dropped before delivery is. The actual side-channel wiring lives in
//! `tak-server` (issue #31).
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

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// The migrator over `crates/tak-store/migrations/`. The vendored upstream
/// schema baseline is `V7__create_base_schema.sql`; subsequent migrations
/// build out mission tables, federation cache tables, etc., through V99.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

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

/// A connection-pool-backed Postgres store.
///
/// Owns a `sqlx::PgPool` and exposes it for query construction. Migrations
/// are run in [`Store::connect_and_migrate`] before the pool is handed
/// back, so callers can assume the schema matches `MIGRATOR`'s
/// expectations the moment they receive a `Store`.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connect with sane defaults + run all pending migrations.
    ///
    /// `url` is a Postgres connection string
    /// (`postgres://user:pass@host:port/dbname`).
    ///
    /// # Errors
    ///
    /// - [`Error::Sqlx`] on connection failure.
    /// - [`Error::Migrate`] on migration parse/execute failure.
    pub async fn connect_and_migrate(url: &str) -> Result<Self> {
        let opts: PgConnectOptions = url.parse()?;
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// Connect to an already-migrated database (skips the migration step).
    /// Useful when the migrator has been run separately by an operator
    /// or when sharing a pool with another component.
    ///
    /// # Errors
    ///
    /// - [`Error::Sqlx`] on connection failure.
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
        Ok(Self { pool })
    }

    /// Borrow the underlying pool for query construction.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
