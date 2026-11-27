//! Daemon settings (migrations 0003 and 0004): values the operator sets on
//! the host, for the whole host or for one project, never read from a
//! pushed directory.

use ship_core::{ProjectId, Timestamp};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::Store;
use crate::clock::now;
use crate::error::{StoreError, db, write_err};
use crate::repo::time;

/// Where a setting applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingScope {
    Host,
    Project(ProjectId),
}

impl SettingScope {
    fn parts(self) -> (&'static str, Option<String>) {
        match self {
            SettingScope::Host => ("host", None),
            SettingScope::Project(p) => ("project", Some(p.to_string())),
        }
    }
}

/// A stored setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub scope: SettingScope,
    pub key: String,
    pub value: String,
    pub updated_at: Timestamp,
    /// Who set it, as the audit log names them.
    pub updated_by: String,
}

fn setting_from(scope: SettingScope, row: &SqliteRow) -> Result<Setting, StoreError> {
    Ok(Setting {
        scope,
        key: row.try_get("key").map_err(db)?,
        value: row.try_get("value").map_err(db)?,
        updated_at: time(row, "updated_at", "setting")?,
        updated_by: row.try_get("updated_by").map_err(db)?,
    })
}

impl Store {
    /// The setting `key` of `scope`, if it was ever set there.
    pub async fn setting(
        &self,
        scope: SettingScope,
        key: &str,
    ) -> Result<Option<Setting>, StoreError> {
        let (kind, project) = scope.parts();
        sqlx::query(
            "SELECT key, value, updated_at, updated_by FROM setting \
             WHERE scope = ? AND coalesce(project_id, '') = ? AND key = ?",
        )
        .bind(kind)
        .bind(project.unwrap_or_default())
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .as_ref()
        .map(|row| setting_from(scope, row))
        .transpose()
    }

    /// Sets `key` in `scope` to `value`, recording `by`, replacing any
    /// earlier value. A key, value or actor outside the schema's lengths
    /// (1–64, 1–256) is [`StoreError::Invalid`]; a project that does not
    /// exist is refused by the schema. Callers validate the value itself.
    pub async fn set_setting(
        &self,
        scope: SettingScope,
        key: &str,
        value: &str,
        by: &str,
    ) -> Result<Setting, StoreError> {
        if by.is_empty() || by.len() > 256 {
            return Err(StoreError::Invalid { entity: "setting" });
        }
        let updated_at = now()?;
        let (kind, project) = scope.parts();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;
        sqlx::query(
            "DELETE FROM setting WHERE scope = ? AND coalesce(project_id, '') = ? AND key = ?",
        )
        .bind(kind)
        .bind(project.clone().unwrap_or_default())
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(write_err("setting"))?;
        sqlx::query(
            "INSERT INTO setting (scope, project_id, key, value, updated_at, updated_by) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(kind)
        .bind(project)
        .bind(key)
        .bind(value)
        .bind(updated_at.unix_millis())
        .bind(by)
        .execute(&mut *tx)
        .await
        .map_err(write_err("setting"))?;
        tx.commit().await.map_err(db)?;
        Ok(Setting {
            scope,
            key: key.to_owned(),
            value: value.to_owned(),
            updated_at,
            updated_by: by.to_owned(),
        })
    }

    /// The host-wide setting `key`, if it was ever set.
    pub async fn host_setting(&self, key: &str) -> Result<Option<Setting>, StoreError> {
        self.setting(SettingScope::Host, key).await
    }

    /// Sets the host-wide setting `key` ([`Store::set_setting`]).
    pub async fn set_host_setting(
        &self,
        key: &str,
        value: &str,
        by: &str,
    ) -> Result<Setting, StoreError> {
        self.set_setting(SettingScope::Host, key, value, by).await
    }

    /// The setting `key` in effect for `project`: its own, else the
    /// host's.
    pub async fn effective_setting(
        &self,
        project: ProjectId,
        key: &str,
    ) -> Result<Option<Setting>, StoreError> {
        match self.setting(SettingScope::Project(project), key).await? {
            Some(s) => Ok(Some(s)),
            None => self.host_setting(key).await,
        }
    }
}
