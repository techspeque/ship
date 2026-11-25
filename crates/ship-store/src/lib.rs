//! Ship SQLite access and migrations.
//!
//! Leaf crate: depends only on `ship-core` (OVERVIEW §2.3). One database
//! file, `ship.db`, in the state directory, in WAL mode (ADR-0002). Schema
//! changes only through the migrations in `migrations/`.

mod clock;
mod error;
mod migrate;
mod repo;

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::{ConnectOptions, Connection};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};

pub use error::StoreError;
pub use migrate::{LATEST_VERSION, OpenReport};
pub use repo::{
    CreatedOperation, Environment, NewEnvironment, NewEvent, NewOperation, NewService, Operation,
    OperationEvent, Project, Service,
};

/// The database file name inside the state directory.
pub const DATABASE_FILE: &str = "ship.db";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const POOL_SIZE: u32 = 4;
const CLOSE_POLL: Duration = Duration::from_millis(10);

/// The open database.
///
/// Cheap to clone; clones share one connection pool.
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
    path: PathBuf,
    report: OpenReport,
}

impl Store {
    /// Opens `<state_dir>/ship.db`, creating it if absent, and migrates it to
    /// [`LATEST_VERSION`].
    ///
    /// `state_dir` must exist. A database newer than this binary is refused
    /// with [`StoreError::SchemaTooNew`] and left unchanged. An existing
    /// database is backed up beside itself before any migration is applied.
    pub async fn open(state_dir: &Path) -> Result<Store, StoreError> {
        Store::open_with(state_dir, migrate::MIGRATIONS).await
    }

    pub(crate) async fn open_with(
        state_dir: &Path,
        migrations: &[migrate::Migration],
    ) -> Result<Store, StoreError> {
        let meta = std::fs::metadata(state_dir).map_err(StoreError::Io)?;
        if !meta.is_dir() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "state directory is not a directory",
            )));
        }
        let path = state_dir.join(DATABASE_FILE);
        let report = migrate::run(&path, migrations).await?;
        let options = base_options(&path)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(POOL_SIZE)
            .connect_with(options)
            .await
            .map_err(error::db)?;
        Ok(Store { pool, path, report })
    }

    /// Path of the database file.
    pub fn database_path(&self) -> &Path {
        &self.path
    }

    /// What opening did to the schema.
    pub fn open_report(&self) -> &OpenReport {
        &self.report
    }

    /// The current schema version.
    pub async fn schema_version(&self) -> Result<u32, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(error::db)?;
        migrate::user_version(&mut conn).await
    }

    /// Closes every connection. Clones of this store stop working.
    ///
    /// Returns once no connection to the database is open, which SQLite
    /// shows by removing the `-wal` and `-shm` sidecars when the last one
    /// closes. Until they are gone, a short-lived barrier connection is
    /// opened and closed: whichever connection closes last checkpoints and
    /// removes them. The wait is bounded by the busy timeout, after which
    /// the sidecars are left for the next open to deal with.
    pub async fn close(self) {
        self.pool.close().await;
        let deadline = tokio::time::Instant::now() + BUSY_TIMEOUT;
        while sidecars_present(&self.path) && tokio::time::Instant::now() < deadline {
            // Errors are not fatal here: a failed barrier only means another
            // round, and `open` copes with leftover sidecars.
            if let Ok(mut conn) = base_options(&self.path).connect().await {
                let _ = sqlx::query("SELECT count(*) FROM sqlite_schema")
                    .execute(&mut conn)
                    .await;
                let _ = conn.close().await;
            }
            if sidecars_present(&self.path) {
                tokio::time::sleep(CLOSE_POLL).await;
            }
        }
    }
}

/// Whether the `-wal` or `-shm` file beside `path` exists.
fn sidecars_present(path: &Path) -> bool {
    ["-wal", "-shm"].iter().any(|suffix| {
        migrate::sidecar(path, suffix)
            .map(|p| p.exists())
            .unwrap_or(false)
    })
}

/// Options every connection shares. Sets no persistent pragma.
pub(crate) fn base_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
        .disable_statement_logging()
}
