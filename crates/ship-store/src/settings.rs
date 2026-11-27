//! Daemon settings (migration 0003): values the operator sets on the host,
//! never read from a pushed directory.

use ship_core::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::Store;
use crate::clock::now;
use crate::error::{StoreError, db, write_err};
use crate::repo::time;

/// A stored setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub updated_at: Timestamp,
}

fn setting_from(row: &SqliteRow) -> Result<Setting, StoreError> {
    Ok(Setting {
        key: row.try_get("key").map_err(db)?,
        value: row.try_get("value").map_err(db)?,
        updated_at: time(row, "updated_at", "setting")?,
    })
}

impl Store {
    /// The host-wide setting `key`, if it was ever set.
    pub async fn host_setting(&self, key: &str) -> Result<Option<Setting>, StoreError> {
        sqlx::query(
            "SELECT key, value, updated_at FROM setting \
             WHERE scope = 'host' AND project_id IS NULL AND key = ?",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .as_ref()
        .map(setting_from)
        .transpose()
    }

    /// Sets the host-wide setting `key` to `value`, replacing any earlier
    /// value. A key or value outside the schema's lengths (1–64, 1–256) is
    /// [`StoreError::Invalid`]. Callers validate the value itself.
    pub async fn set_host_setting(&self, key: &str, value: &str) -> Result<Setting, StoreError> {
        let updated_at = now()?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;
        sqlx::query("DELETE FROM setting WHERE scope = 'host' AND project_id IS NULL AND key = ?")
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(write_err("setting"))?;
        sqlx::query(
            "INSERT INTO setting (scope, project_id, key, value, updated_at) \
             VALUES ('host', NULL, ?, ?, ?)",
        )
        .bind(key)
        .bind(value)
        .bind(updated_at.unix_millis())
        .execute(&mut *tx)
        .await
        .map_err(write_err("setting"))?;
        tx.commit().await.map_err(db)?;
        Ok(Setting {
            key: key.to_owned(),
            value: value.to_owned(),
            updated_at,
        })
    }
}
