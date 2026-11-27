//! Config values and secret metadata (migration 0002, ADR-0009).
//!
//! A config value is stored with its value. A secret is stored as its name,
//! its scope and a backend reference to where the daemon keeps the value;
//! the `secret` table has no column that could hold the value. A config
//! value and a secret of one name in one scope are refused, since both
//! become the same environment variable.
//!
//! A row sets exactly the id column its scope names (a schema check), so
//! `coalesce(project_id, environment_id, service_id)` is that id, as in the
//! tables' unique indexes.

use ship_core::{ConfigScope, EnvironmentId, ProjectId, ServiceId, Timestamp};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::Store;
use crate::clock::now;
use crate::error::{StoreError, db, write_err};
use crate::repo::time;

/// Where a config value or a secret is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConfigTarget {
    Project(ProjectId),
    Environment(EnvironmentId),
    Service(ServiceId),
}

impl ConfigTarget {
    /// The scope's name.
    pub fn scope(self) -> ConfigScope {
        match self {
            ConfigTarget::Project(_) => ConfigScope::Project,
            ConfigTarget::Environment(_) => ConfigScope::Environment,
            ConfigTarget::Service(_) => ConfigScope::Service,
        }
    }

    /// The id the scope names.
    fn id(self) -> String {
        match self {
            ConfigTarget::Project(p) => p.to_string(),
            ConfigTarget::Environment(e) => e.to_string(),
            ConfigTarget::Service(s) => s.to_string(),
        }
    }

    /// The values of `project_id`, `environment_id` and `service_id`.
    fn columns(self) -> [Option<String>; 3] {
        match self {
            ConfigTarget::Project(p) => [Some(p.to_string()), None, None],
            ConfigTarget::Environment(e) => [None, Some(e.to_string()), None],
            ConfigTarget::Service(s) => [None, None, Some(s.to_string())],
        }
    }
}

/// A stored config value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValue {
    pub target: ConfigTarget,
    pub key: String,
    pub value: String,
    pub updated_at: Timestamp,
}

/// A stored secret's metadata. Never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRecord {
    pub target: ConfigTarget,
    pub name: String,
    /// Where the secret store keeps the value.
    pub backend_ref: String,
    pub updated_at: Timestamp,
}

/// Longest backend reference the schema holds.
const MAX_REF_LEN: usize = 256;

fn target_from(row: &SqliteRow, entity: &'static str) -> Result<ConfigTarget, StoreError> {
    let scope: String = row.try_get("scope").map_err(db)?;
    let corrupt = |field| StoreError::Corrupt { entity, field };
    let id = |col: &'static str| -> Result<String, StoreError> {
        let v: Option<String> = row.try_get(col).map_err(db)?;
        v.ok_or(corrupt(col))
    };
    Ok(match scope.parse::<ConfigScope>() {
        Ok(ConfigScope::Project) => ConfigTarget::Project(
            id("project_id")?
                .parse()
                .map_err(|_| corrupt("project_id"))?,
        ),
        Ok(ConfigScope::Environment) => ConfigTarget::Environment(
            id("environment_id")?
                .parse()
                .map_err(|_| corrupt("environment_id"))?,
        ),
        Ok(ConfigScope::Service) => ConfigTarget::Service(
            id("service_id")?
                .parse()
                .map_err(|_| corrupt("service_id"))?,
        ),
        Err(_) => return Err(corrupt("scope")),
    })
}

fn value_from(row: &SqliteRow) -> Result<ConfigValue, StoreError> {
    const E: &str = "config_value";
    Ok(ConfigValue {
        target: target_from(row, E)?,
        key: row.try_get("key").map_err(db)?,
        value: row.try_get("value").map_err(db)?,
        updated_at: time(row, "updated_at", E)?,
    })
}

fn secret_from(row: &SqliteRow) -> Result<SecretRecord, StoreError> {
    const E: &str = "secret";
    Ok(SecretRecord {
        target: target_from(row, E)?,
        name: row.try_get("name").map_err(db)?,
        backend_ref: row.try_get("backend_ref").map_err(db)?,
        updated_at: time(row, "updated_at", E)?,
    })
}

const SECRET_NAMED: &str = "SELECT 1 FROM secret \
     WHERE scope = ? AND coalesce(project_id, environment_id, service_id) = ? AND name = ?";
const VALUE_NAMED: &str = "SELECT 1 FROM config_value \
     WHERE scope = ? AND coalesce(project_id, environment_id, service_id) = ? AND key = ?";
const DELETE_VALUE: &str = "DELETE FROM config_value \
     WHERE scope = ? AND coalesce(project_id, environment_id, service_id) = ? AND key = ?";
const INSERT_VALUE: &str = "INSERT INTO config_value \
     (scope, project_id, environment_id, service_id, key, value, updated_at) \
     VALUES (?, ?, ?, ?, ?, ?, ?)";
const LIST_VALUES: &str = "SELECT scope, project_id, environment_id, service_id, key, value, \
     updated_at FROM config_value \
     WHERE scope = ? AND coalesce(project_id, environment_id, service_id) = ? ORDER BY key";
const SECRET_REF: &str = "SELECT backend_ref FROM secret \
     WHERE scope = ? AND coalesce(project_id, environment_id, service_id) = ? AND name = ?";
const DELETE_SECRET: &str = "DELETE FROM secret \
     WHERE scope = ? AND coalesce(project_id, environment_id, service_id) = ? AND name = ?";
const INSERT_SECRET: &str = "INSERT INTO secret \
     (scope, project_id, environment_id, service_id, name, backend_ref, updated_at) \
     VALUES (?, ?, ?, ?, ?, ?, ?)";
const LIST_SECRETS: &str = "SELECT scope, project_id, environment_id, service_id, name, \
     backend_ref, updated_at FROM secret \
     WHERE scope = ? AND coalesce(project_id, environment_id, service_id) = ? ORDER BY name";
const ALL_SECRETS: &str = "SELECT scope, project_id, environment_id, service_id, name, \
     backend_ref, updated_at FROM secret ORDER BY scope, name";

impl Store {
    /// A new backend reference for a secret value: 32 random hex digits.
    pub fn new_secret_ref(&self) -> Result<String, StoreError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| StoreError::Entropy)?;
        Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Sets the config value `key` of `target`, replacing an earlier one. A
    /// secret of that name in the same scope is [`StoreError::Conflict`];
    /// an unknown target is refused by the schema (`Conflict`); a key
    /// outside 1–256 bytes is [`StoreError::Invalid`]. Callers check the
    /// name and value rules (`ship_core::config`).
    pub async fn set_config_value(
        &self,
        target: ConfigTarget,
        key: &str,
        value: &str,
    ) -> Result<ConfigValue, StoreError> {
        let at = now()?;
        let scope = target.scope().as_str();
        let [project, environment, service] = target.columns();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;
        let clash: Option<i64> = sqlx::query_scalar(SECRET_NAMED)
            .bind(scope)
            .bind(target.id())
            .bind(key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        if clash.is_some() {
            return Err(StoreError::Conflict {
                entity: "config_value",
            });
        }
        sqlx::query(DELETE_VALUE)
            .bind(scope)
            .bind(target.id())
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(write_err("config_value"))?;
        sqlx::query(INSERT_VALUE)
            .bind(scope)
            .bind(project)
            .bind(environment)
            .bind(service)
            .bind(key)
            .bind(value)
            .bind(at.unix_millis())
            .execute(&mut *tx)
            .await
            .map_err(write_err("config_value"))?;
        tx.commit().await.map_err(db)?;
        Ok(ConfigValue {
            target,
            key: key.to_owned(),
            value: value.to_owned(),
            updated_at: at,
        })
    }

    /// Removes the config value `key` of `target`; whether there was one.
    pub async fn unset_config_value(
        &self,
        target: ConfigTarget,
        key: &str,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query(DELETE_VALUE)
            .bind(target.scope().as_str())
            .bind(target.id())
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(write_err("config_value"))?;
        Ok(done.rows_affected() > 0)
    }

    /// The config values of `target`, by key.
    pub async fn list_config_values(
        &self,
        target: ConfigTarget,
    ) -> Result<Vec<ConfigValue>, StoreError> {
        sqlx::query(LIST_VALUES)
            .bind(target.scope().as_str())
            .bind(target.id())
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
            .iter()
            .map(value_from)
            .collect()
    }

    /// Records the secret `name` of `target` as kept under `backend_ref`,
    /// replacing an earlier record, and returns the new record with the
    /// backend reference it replaced, which the caller then removes. A
    /// config value of that name in the same scope is
    /// [`StoreError::Conflict`]; an unknown target is `Conflict`; a name
    /// outside 1–256 bytes or an empty or overlong reference is
    /// [`StoreError::Invalid`].
    pub async fn set_secret(
        &self,
        target: ConfigTarget,
        name: &str,
        backend_ref: &str,
    ) -> Result<(SecretRecord, Option<String>), StoreError> {
        if backend_ref.is_empty() || backend_ref.len() > MAX_REF_LEN {
            return Err(StoreError::Invalid { entity: "secret" });
        }
        let at = now()?;
        let scope = target.scope().as_str();
        let [project, environment, service] = target.columns();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;
        let clash: Option<i64> = sqlx::query_scalar(VALUE_NAMED)
            .bind(scope)
            .bind(target.id())
            .bind(name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        if clash.is_some() {
            return Err(StoreError::Conflict { entity: "secret" });
        }
        let replaced: Option<String> = sqlx::query_scalar(SECRET_REF)
            .bind(scope)
            .bind(target.id())
            .bind(name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query(DELETE_SECRET)
            .bind(scope)
            .bind(target.id())
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(write_err("secret"))?;
        sqlx::query(INSERT_SECRET)
            .bind(scope)
            .bind(project)
            .bind(environment)
            .bind(service)
            .bind(name)
            .bind(backend_ref)
            .bind(at.unix_millis())
            .execute(&mut *tx)
            .await
            .map_err(write_err("secret"))?;
        tx.commit().await.map_err(db)?;
        Ok((
            SecretRecord {
                target,
                name: name.to_owned(),
                backend_ref: backend_ref.to_owned(),
                updated_at: at,
            },
            replaced,
        ))
    }

    /// Removes the secret `name` of `target`; the backend reference it had,
    /// if there was one, which the caller then removes.
    pub async fn unset_secret(
        &self,
        target: ConfigTarget,
        name: &str,
    ) -> Result<Option<String>, StoreError> {
        let scope = target.scope().as_str();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;
        let replaced: Option<String> = sqlx::query_scalar(SECRET_REF)
            .bind(scope)
            .bind(target.id())
            .bind(name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query(DELETE_SECRET)
            .bind(scope)
            .bind(target.id())
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(write_err("secret"))?;
        tx.commit().await.map_err(db)?;
        Ok(replaced)
    }

    /// The secrets of `target`, by name.
    pub async fn list_secrets(
        &self,
        target: ConfigTarget,
    ) -> Result<Vec<SecretRecord>, StoreError> {
        sqlx::query(LIST_SECRETS)
            .bind(target.scope().as_str())
            .bind(target.id())
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
            .iter()
            .map(secret_from)
            .collect()
    }

    /// Every stored secret, of every scope.
    pub async fn all_secrets(&self) -> Result<Vec<SecretRecord>, StoreError> {
        sqlx::query(ALL_SECRETS)
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
            .iter()
            .map(secret_from)
            .collect()
    }
}
