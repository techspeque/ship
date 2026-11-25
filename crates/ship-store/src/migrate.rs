//! In-repo schema migrations.
//!
//! The schema version is SQLite's `PRAGMA user_version`. Each migration runs
//! in one transaction together with its version bump, so a failed migration
//! leaves the previous version in place.
//!
//! Order on open:
//! 1. Read the version of an existing database on a read-only connection.
//! 2. Newer than this binary → [`StoreError::SchemaTooNew`], nothing written.
//! 3. An existing, non-empty database with pending migrations is copied with
//!    `VACUUM INTO` beside itself first.
//! 4. Switch to WAL, then apply the pending migrations in order.

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqliteConnection;
use sqlx::{ConnectOptions, Connection};

use crate::error::{StoreError, db};
use crate::{DATABASE_FILE, base_options, clock};

/// One schema migration.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

/// Every migration this binary knows, in order. Versions start at 1 and
/// increase by one.
pub(crate) const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("../migrations/0001_initial.sql"),
}];

/// The schema version this binary migrates to.
pub const LATEST_VERSION: u32 = MIGRATIONS[MIGRATIONS.len() - 1].version;

/// What [`crate::Store::open`] did to the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenReport {
    /// Schema version found on open (0 for a new database).
    pub from_version: u32,
    /// Schema version after open.
    pub to_version: u32,
    /// The backup taken before migrating, if one was needed.
    pub backup: Option<PathBuf>,
}

/// Brings the database at `path` to the last version in `migrations`.
pub(crate) async fn run(path: &Path, migrations: &[Migration]) -> Result<OpenReport, StoreError> {
    let existed = std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);
    if existed {
        let latest = migrations.last().map_or(0, |m| m.version);
        let found = probe_version(path).await?;
        if found > latest {
            return Err(StoreError::SchemaTooNew {
                found,
                supported: latest,
            });
        }
    }
    // No journal_mode here: WAL is switched on only after the backup.
    let mut conn = base_options(path)
        .create_if_missing(true)
        .connect()
        .await
        .map_err(db)?;
    let result = run_on(&mut conn, path, existed, migrations).await;
    let closed = conn.close().await;
    let report = result?;
    closed.map_err(db)?;
    Ok(report)
}

async fn run_on(
    conn: &mut SqliteConnection,
    path: &Path,
    existed: bool,
    migrations: &[Migration],
) -> Result<OpenReport, StoreError> {
    let latest = migrations.last().map_or(0, |m| m.version);
    let from = user_version(conn).await?;
    if from > latest {
        return Err(StoreError::SchemaTooNew {
            found: from,
            supported: latest,
        });
    }
    let pending: Vec<&Migration> = migrations.iter().filter(|m| m.version > from).collect();

    let backup = if existed && !pending.is_empty() {
        Some(backup(conn, path, from).await?)
    } else {
        None
    };

    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(&mut *conn)
        .await
        .map_err(db)?;

    for m in pending {
        let mut tx = conn.begin().await.map_err(db)?;
        sqlx::raw_sql(m.sql).execute(&mut *tx).await.map_err(db)?;
        // `user_version` cannot be bound; `m.version` is a compiled-in u32.
        let bump = format!("PRAGMA user_version = {}", m.version);
        sqlx::raw_sql(&bump).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
    }

    Ok(OpenReport {
        from_version: from,
        to_version: user_version(conn).await?,
        backup,
    })
}

/// Reads the schema version of an existing database through a read-only
/// connection without changing any file in its directory.
///
/// Even a read-only SQLite connection creates or writes the `-wal` and `-shm`
/// sidecars of a WAL database. So without a WAL file the main file is opened
/// `immutable` (it alone holds every committed page); with one, the database
/// and its WAL are copied to a private temporary directory and read there.
///
/// A connection still closing elsewhere can checkpoint and unlink the WAL
/// between the size check and the copy. A WAL that has gone was fully
/// checkpointed into the main file, so a `NotFound` while copying sends the
/// probe back to the check, which then takes the `immutable` path.
async fn probe_version(path: &Path) -> Result<u32, StoreError> {
    probe_version_with(path, |from, to| std::fs::copy(from, to)).await
}

/// Attempts at reading a WAL database before a vanishing WAL is an error.
const PROBE_ATTEMPTS: usize = 3;

async fn probe_version_with(
    path: &Path,
    copy: impl Fn(&Path, &Path) -> std::io::Result<u64>,
) -> Result<u32, StoreError> {
    let wal = sidecar(path, "-wal")?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let wal_has_frames = std::fs::metadata(&wal)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if !wal_has_frames {
            return read_only_version(path, true).await;
        }
        match probe_copy(path, &wal, &copy).await {
            Err(StoreError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound && attempt < PROBE_ATTEMPTS => {}
            other => return other,
        }
    }
}

/// Reads the version from a private copy of the database and its WAL.
async fn probe_copy(
    path: &Path,
    wal: &Path,
    copy: &impl Fn(&Path, &Path) -> std::io::Result<u64>,
) -> Result<u32, StoreError> {
    let mut entropy = [0u8; 8];
    getrandom::fill(&mut entropy).map_err(|_| StoreError::Entropy)?;
    let tmp = std::env::temp_dir().join(format!(
        "ship-store-probe-{}-{:016x}",
        std::process::id(),
        u64::from_le_bytes(entropy)
    ));
    std::fs::create_dir(&tmp).map_err(StoreError::Io)?;
    let dest = tmp.join(DATABASE_FILE);
    let result = async {
        copy(path, &dest).map_err(StoreError::Io)?;
        copy(wal, &sidecar(&dest, "-wal")?).map_err(StoreError::Io)?;
        read_only_version(&dest, false).await
    }
    .await;
    // Best effort: the copy is private and outside the state directory.
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

async fn read_only_version(path: &Path, immutable: bool) -> Result<u32, StoreError> {
    let mut conn = base_options(path)
        .read_only(true)
        .immutable(immutable)
        .connect()
        .await
        .map_err(db)?;
    let result = user_version(&mut conn).await;
    let closed = conn.close().await;
    let version = result?;
    closed.map_err(db)?;
    Ok(version)
}

/// `<path><suffix>`, e.g. the `-wal` file beside a database.
pub(crate) fn sidecar(path: &Path, suffix: &str) -> Result<PathBuf, StoreError> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid_path("database file name is not UTF-8"))?;
    Ok(path.with_file_name(format!("{file_name}{suffix}")))
}

pub(crate) async fn user_version(conn: &mut SqliteConnection) -> Result<u32, StoreError> {
    let v: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *conn)
        .await
        .map_err(db)?;
    u32::try_from(v).map_err(|_| StoreError::Corrupt {
        entity: "database",
        field: "user_version",
    })
}

/// Copies the database to `<file>.backup-v<from>-<unix_ms>` beside it.
async fn backup(
    conn: &mut SqliteConnection,
    path: &Path,
    from: u32,
) -> Result<PathBuf, StoreError> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid_path("database file name is not UTF-8"))?;
    let dest = path.with_file_name(format!(
        "{file_name}.backup-v{from}-{}",
        clock::now_millis()?
    ));
    let dest_str = dest
        .to_str()
        .ok_or_else(|| invalid_path("backup path is not UTF-8"))?;
    sqlx::query("VACUUM INTO ?")
        .bind(dest_str)
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    Ok(dest)
}

fn invalid_path(msg: &'static str) -> StoreError {
    StoreError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DATABASE_FILE, Store};

    const EXTRA: Migration = Migration {
        version: 2,
        sql: "CREATE TABLE extra (x INTEGER) STRICT;",
    };

    async fn table_exists(conn: &mut SqliteConnection, name: &str) -> bool {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?",
        )
        .bind(name)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        n == 1
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".backup-"))
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn migration_versions_increase_by_one_from_one() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(m.version as usize, i + 1);
        }
        assert_eq!(LATEST_VERSION as usize, MIGRATIONS.len());
    }

    #[tokio::test]
    async fn no_backup_on_fresh_database() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let report = store.open_report().clone();
        store.close().await;
        assert_eq!(report.from_version, 0);
        assert_eq!(report.to_version, LATEST_VERSION);
        assert_eq!(report.backup, None);
        assert!(backups(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn backup_before_migrating_existing_database() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_with(dir.path(), &MIGRATIONS[..1])
            .await
            .unwrap();
        store.close().await;

        let mut migrations = MIGRATIONS[..1].to_vec();
        migrations.push(EXTRA);
        let store = Store::open_with(dir.path(), &migrations).await.unwrap();
        let report = store.open_report().clone();
        store.close().await;

        assert_eq!(report.from_version, 1);
        assert_eq!(report.to_version, 2);
        let backup = report.backup.expect("a backup was taken");
        assert_eq!(backups(dir.path()), vec![backup.clone()]);
        let name = backup.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(&format!("{DATABASE_FILE}.backup-v1-")));

        // The backup holds the pre-migration schema.
        let mut conn = base_options(&backup).connect().await.unwrap();
        assert_eq!(user_version(&mut conn).await.unwrap(), 1);
        assert!(table_exists(&mut conn, "project").await);
        assert!(!table_exists(&mut conn, "extra").await);
        conn.close().await.unwrap();

        // The live database has the new one.
        let mut conn = base_options(&dir.path().join(DATABASE_FILE))
            .connect()
            .await
            .unwrap();
        assert_eq!(user_version(&mut conn).await.unwrap(), 2);
        assert!(table_exists(&mut conn, "extra").await);
        conn.close().await.unwrap();

        // Reopening at the current version takes no further backup.
        let store = Store::open_with(dir.path(), &migrations).await.unwrap();
        assert_eq!(store.open_report().backup, None);
        store.close().await;
        assert_eq!(backups(dir.path()).len(), 1);
    }

    /// A closed store's directory with a non-empty `-wal` beside the database,
    /// so the probe takes the copy path.
    async fn closed_store_with_wal() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path()).await.unwrap().close().await;
        let db = dir.path().join(DATABASE_FILE);
        let wal = sidecar(&db, "-wal").unwrap();
        std::fs::write(&wal, b"not yet checkpointed").unwrap();
        (dir, db, wal)
    }

    #[tokio::test]
    async fn probe_tolerates_wal_vanishing_mid_copy() {
        let (_dir, db, wal) = closed_store_with_wal().await;
        // The WAL is checkpointed and unlinked just as the probe reads it.
        let copy = |from: &Path, to: &Path| {
            if from == wal {
                std::fs::remove_file(&wal).unwrap();
            }
            std::fs::copy(from, to)
        };
        assert_eq!(probe_version_with(&db, copy).await.unwrap(), LATEST_VERSION);
        assert!(!wal.exists());
    }

    #[tokio::test]
    async fn probe_propagates_other_copy_errors() {
        let (_dir, db, _wal) = closed_store_with_wal().await;
        let denied = |_: &Path, _: &Path| -> std::io::Result<u64> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        };
        match probe_version_with(&db, denied).await {
            Err(StoreError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied),
            other => panic!("expected Io(PermissionDenied), got {other:?}"),
        }

        // A WAL that keeps reporting NotFound while still present gives up
        // after a bounded number of attempts.
        let missing = |_: &Path, _: &Path| -> std::io::Result<u64> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        match probe_version_with(&db, missing).await {
            Err(StoreError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Io(NotFound), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn binary_behind_the_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut migrations = MIGRATIONS[..1].to_vec();
        migrations.push(EXTRA);
        Store::open_with(dir.path(), &migrations)
            .await
            .unwrap()
            .close()
            .await;
        match Store::open_with(dir.path(), &MIGRATIONS[..1]).await {
            Err(StoreError::SchemaTooNew { found, supported }) => {
                assert_eq!((found, supported), (2, 1));
            }
            other => panic!("expected SchemaTooNew, got {other:?}"),
        }
    }
}
