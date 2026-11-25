//! Repositories: create, get and list for projects, environments and
//! services; create, get and event append for operations.

use std::str::FromStr;

use ship_core::{
    DesiredState, EnvironmentId, OperationId, OperationPhase, OperationStatus, ProjectId,
    RecoveryPolicy, ServiceId, ServiceKind, Slug, Timestamp, compose_project_name,
};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::Store;
use crate::clock::{new_id, now};
use crate::error::{StoreError, db, write_err};

/// A project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    pub slug: Slug,
    pub created_at: Timestamp,
}

/// Input for [`Store::create_environment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEnvironment {
    pub name: Slug,
    pub desired_state: DesiredState,
    pub recovery_policy: RecoveryPolicy,
}

/// An environment of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub id: EnvironmentId,
    pub project_id: ProjectId,
    pub name: Slug,
    pub desired_state: DesiredState,
    pub recovery_policy: RecoveryPolicy,
    pub created_at: Timestamp,
}

impl Environment {
    /// The Compose project name, derived from the id alone (ADR-0001 rule 4).
    pub fn compose_project(&self) -> String {
        compose_project_name(&self.id)
    }
}

/// Input for [`Store::create_service`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewService {
    /// 1–63 characters. Compose naming rules are checked by the importer.
    pub name: String,
    pub kind: ServiceKind,
    pub port: Option<u16>,
}

/// A service in an environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub id: ServiceId,
    pub environment_id: EnvironmentId,
    pub name: String,
    pub kind: ServiceKind,
    pub port: Option<u16>,
    pub created_at: Timestamp,
}

/// Input for [`Store::create_operation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewOperation<'a> {
    /// Client-supplied; a replay returns the original operation (ADR-0011
    /// rule 1).
    pub idempotency_key: &'a str,
    pub kind: &'a str,
    pub environment_id: Option<EnvironmentId>,
}

/// A durable operation (ADR-0011).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub id: OperationId,
    pub idempotency_key: String,
    pub kind: String,
    pub environment_id: Option<EnvironmentId>,
    pub phase: OperationPhase,
    pub status: OperationStatus,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    pub log_cursor: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Result of [`Store::create_operation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedOperation {
    pub operation: Operation,
    /// `false` when the idempotency key was replayed and the existing
    /// operation was returned.
    pub created: bool,
}

/// Input for [`Store::append_event`]. The caller redacts `message`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewEvent<'a> {
    pub event_type: &'a str,
    pub phase: OperationPhase,
    pub message: &'a str,
}

/// One persisted operation event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationEvent {
    pub operation_id: OperationId,
    /// Starts at 1 and increases by one per operation, with no gaps.
    pub sequence: i64,
    pub event_type: String,
    pub phase: OperationPhase,
    pub message: String,
    pub ts: Timestamp,
}

fn text<T: FromStr>(
    row: &SqliteRow,
    col: &'static str,
    entity: &'static str,
) -> Result<T, StoreError> {
    let s: String = row.try_get(col).map_err(db)?;
    s.parse()
        .map_err(|_| StoreError::Corrupt { entity, field: col })
}

fn opt_text<T: FromStr>(
    row: &SqliteRow,
    col: &'static str,
    entity: &'static str,
) -> Result<Option<T>, StoreError> {
    let s: Option<String> = row.try_get(col).map_err(db)?;
    s.map(|s| {
        s.parse()
            .map_err(|_| StoreError::Corrupt { entity, field: col })
    })
    .transpose()
}

fn time(row: &SqliteRow, col: &'static str, entity: &'static str) -> Result<Timestamp, StoreError> {
    let ms: i64 = row.try_get(col).map_err(db)?;
    Timestamp::from_unix_millis(ms).map_err(|_| StoreError::Corrupt { entity, field: col })
}

fn project_from(row: &SqliteRow) -> Result<Project, StoreError> {
    const E: &str = "project";
    Ok(Project {
        id: text(row, "id", E)?,
        slug: text(row, "slug", E)?,
        created_at: time(row, "created_at", E)?,
    })
}

fn environment_from(row: &SqliteRow) -> Result<Environment, StoreError> {
    const E: &str = "environment";
    Ok(Environment {
        id: text(row, "id", E)?,
        project_id: text(row, "project_id", E)?,
        name: text(row, "name", E)?,
        desired_state: text(row, "desired_state", E)?,
        recovery_policy: text(row, "recovery_policy", E)?,
        created_at: time(row, "created_at", E)?,
    })
}

fn service_from(row: &SqliteRow) -> Result<Service, StoreError> {
    const E: &str = "service";
    let port: Option<i64> = row.try_get("port").map_err(db)?;
    let port = port
        .map(u16::try_from)
        .transpose()
        .map_err(|_| StoreError::Corrupt {
            entity: E,
            field: "port",
        })?;
    Ok(Service {
        id: text(row, "id", E)?,
        environment_id: text(row, "environment_id", E)?,
        name: row.try_get("name").map_err(db)?,
        kind: text(row, "kind", E)?,
        port,
        created_at: time(row, "created_at", E)?,
    })
}

fn operation_from(row: &SqliteRow) -> Result<Operation, StoreError> {
    const E: &str = "operation";
    Ok(Operation {
        id: text(row, "id", E)?,
        idempotency_key: row.try_get("idempotency_key").map_err(db)?,
        kind: row.try_get("kind").map_err(db)?,
        environment_id: opt_text(row, "environment_id", E)?,
        phase: text(row, "phase", E)?,
        status: text(row, "status", E)?,
        failure_code: row.try_get("failure_code").map_err(db)?,
        failure_message: row.try_get("failure_message").map_err(db)?,
        log_cursor: row.try_get("log_cursor").map_err(db)?,
        created_at: time(row, "created_at", E)?,
        updated_at: time(row, "updated_at", E)?,
    })
}

fn event_from(row: &SqliteRow) -> Result<OperationEvent, StoreError> {
    const E: &str = "operation_event";
    Ok(OperationEvent {
        operation_id: text(row, "operation_id", E)?,
        sequence: row.try_get("sequence").map_err(db)?,
        event_type: row.try_get("type").map_err(db)?,
        phase: text(row, "phase", E)?,
        message: row.try_get("message").map_err(db)?,
        ts: time(row, "ts", E)?,
    })
}

const OPERATION_COLUMNS: &str = "id, idempotency_key, kind, environment_id, phase, status, \
     failure_code, failure_message, log_cursor, created_at, updated_at";

impl Store {
    // ---- projects ----

    /// Creates a project. A slug already in use is [`StoreError::Conflict`].
    pub async fn create_project(&self, slug: &Slug) -> Result<Project, StoreError> {
        let created_at = now()?;
        let project = Project {
            id: new_id(created_at, ProjectId::from_parts)?,
            slug: slug.clone(),
            created_at,
        };
        sqlx::query("INSERT INTO project (id, slug, created_at) VALUES (?, ?, ?)")
            .bind(project.id.to_string())
            .bind(project.slug.as_str())
            .bind(project.created_at.unix_millis())
            .execute(&self.pool)
            .await
            .map_err(write_err("project"))?;
        Ok(project)
    }

    /// The project with `id`.
    pub async fn get_project(&self, id: ProjectId) -> Result<Project, StoreError> {
        let row = sqlx::query("SELECT id, slug, created_at FROM project WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or(StoreError::NotFound { entity: "project" })?;
        project_from(&row)
    }

    /// Every project, oldest first.
    pub async fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        sqlx::query("SELECT id, slug, created_at FROM project ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
            .iter()
            .map(project_from)
            .collect()
    }

    // ---- environments ----

    /// Creates an environment in `project`. A name already used in that
    /// project, or an unknown project, is [`StoreError::Conflict`].
    pub async fn create_environment(
        &self,
        project: ProjectId,
        new: &NewEnvironment,
    ) -> Result<Environment, StoreError> {
        let created_at = now()?;
        let env = Environment {
            id: new_id(created_at, EnvironmentId::from_parts)?,
            project_id: project,
            name: new.name.clone(),
            desired_state: new.desired_state,
            recovery_policy: new.recovery_policy,
            created_at,
        };
        sqlx::query(
            "INSERT INTO environment \
             (id, project_id, name, desired_state, recovery_policy, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(env.id.to_string())
        .bind(env.project_id.to_string())
        .bind(env.name.as_str())
        .bind(env.desired_state.as_str())
        .bind(env.recovery_policy.as_str())
        .bind(env.created_at.unix_millis())
        .execute(&self.pool)
        .await
        .map_err(write_err("environment"))?;
        Ok(env)
    }

    /// The environment with `id`.
    pub async fn get_environment(&self, id: EnvironmentId) -> Result<Environment, StoreError> {
        let row = sqlx::query(
            "SELECT id, project_id, name, desired_state, recovery_policy, created_at \
             FROM environment WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .ok_or(StoreError::NotFound {
            entity: "environment",
        })?;
        environment_from(&row)
    }

    /// Every environment of `project`, oldest first.
    pub async fn list_environments(
        &self,
        project: ProjectId,
    ) -> Result<Vec<Environment>, StoreError> {
        sqlx::query(
            "SELECT id, project_id, name, desired_state, recovery_policy, created_at \
             FROM environment WHERE project_id = ? ORDER BY id",
        )
        .bind(project.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .iter()
        .map(environment_from)
        .collect()
    }

    // ---- services ----

    /// Creates a service in `environment`. A name already used there, or an
    /// unknown environment, is [`StoreError::Conflict`]; a name outside 1–63
    /// characters is [`StoreError::Invalid`].
    pub async fn create_service(
        &self,
        environment: EnvironmentId,
        new: &NewService,
    ) -> Result<Service, StoreError> {
        let created_at = now()?;
        let svc = Service {
            id: new_id(created_at, ServiceId::from_parts)?,
            environment_id: environment,
            name: new.name.clone(),
            kind: new.kind,
            port: new.port,
            created_at,
        };
        sqlx::query(
            "INSERT INTO service (id, environment_id, name, kind, port, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(svc.id.to_string())
        .bind(svc.environment_id.to_string())
        .bind(svc.name.as_str())
        .bind(svc.kind.as_str())
        .bind(svc.port.map(i64::from))
        .bind(svc.created_at.unix_millis())
        .execute(&self.pool)
        .await
        .map_err(write_err("service"))?;
        Ok(svc)
    }

    /// The service with `id`.
    pub async fn get_service(&self, id: ServiceId) -> Result<Service, StoreError> {
        let row = sqlx::query(
            "SELECT id, environment_id, name, kind, port, created_at FROM service WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?
        .ok_or(StoreError::NotFound { entity: "service" })?;
        service_from(&row)
    }

    /// Every service of `environment`, oldest first.
    pub async fn list_services(
        &self,
        environment: EnvironmentId,
    ) -> Result<Vec<Service>, StoreError> {
        sqlx::query(
            "SELECT id, environment_id, name, kind, port, created_at \
             FROM service WHERE environment_id = ? ORDER BY id",
        )
        .bind(environment.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .iter()
        .map(service_from)
        .collect()
    }

    // ---- operations ----

    /// Records a new operation in phase `queued`, before any side effect
    /// (ADR-0011).
    ///
    /// A replayed idempotency key returns the original operation with
    /// `created = false`; replayed with a different kind or environment it
    /// is [`StoreError::Conflict`]. An unknown environment is
    /// [`StoreError::Conflict`].
    pub async fn create_operation(
        &self,
        new: &NewOperation<'_>,
    ) -> Result<CreatedOperation, StoreError> {
        let created_at = now()?;
        let id = new_id(created_at, OperationId::from_parts)?;
        let phase = OperationPhase::Queued;
        let result = sqlx::query(
            "INSERT INTO operation \
             (id, idempotency_key, kind, environment_id, phase, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (idempotency_key) DO NOTHING",
        )
        .bind(id.to_string())
        .bind(new.idempotency_key)
        .bind(new.kind)
        .bind(new.environment_id.map(|e| e.to_string()))
        .bind(phase.as_str())
        .bind(phase.status().as_str())
        .bind(created_at.unix_millis())
        .bind(created_at.unix_millis())
        .execute(&self.pool)
        .await
        .map_err(write_err("operation"))?;
        let created = result.rows_affected() == 1;

        let sql = format!("SELECT {OPERATION_COLUMNS} FROM operation WHERE idempotency_key = ?");
        let row = sqlx::query(&sql)
            .bind(new.idempotency_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or(StoreError::NotFound {
                entity: "operation",
            })?;
        let operation = operation_from(&row)?;
        if !created
            && (operation.kind != new.kind || operation.environment_id != new.environment_id)
        {
            return Err(StoreError::Conflict {
                entity: "operation",
            });
        }
        Ok(CreatedOperation { operation, created })
    }

    /// The operation with `id`.
    pub async fn get_operation(&self, id: OperationId) -> Result<Operation, StoreError> {
        let sql = format!("SELECT {OPERATION_COLUMNS} FROM operation WHERE id = ?");
        let row = sqlx::query(&sql)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or(StoreError::NotFound {
                entity: "operation",
            })?;
        operation_from(&row)
    }

    /// Appends an event to `operation` with the next sequence number.
    ///
    /// The number is assigned inside one `INSERT`, which SQLite runs under
    /// its single write lock, so concurrent appends to one operation get
    /// distinct, gapless numbers starting at 1. An unknown operation is
    /// [`StoreError::NotFound`].
    pub async fn append_event(
        &self,
        operation: OperationId,
        event: &NewEvent<'_>,
    ) -> Result<OperationEvent, StoreError> {
        let ts = now()?;
        let row = sqlx::query(
            "INSERT INTO operation_event (operation_id, sequence, type, phase, message, ts) \
             SELECT o.id, \
                    (SELECT COALESCE(MAX(e.sequence), 0) + 1 \
                       FROM operation_event e WHERE e.operation_id = o.id), \
                    ?, ?, ?, ? \
               FROM operation o WHERE o.id = ? \
             RETURNING operation_id, sequence, type, phase, message, ts",
        )
        .bind(event.event_type)
        .bind(event.phase.as_str())
        .bind(event.message)
        .bind(ts.unix_millis())
        .bind(operation.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(write_err("operation_event"))?
        .ok_or(StoreError::NotFound {
            entity: "operation",
        })?;
        event_from(&row)
    }

    /// Events of `operation` with a sequence greater than `after`, in order.
    /// `after = 0` returns all of them.
    pub async fn events_after(
        &self,
        operation: OperationId,
        after: i64,
    ) -> Result<Vec<OperationEvent>, StoreError> {
        sqlx::query(
            "SELECT operation_id, sequence, type, phase, message, ts FROM operation_event \
             WHERE operation_id = ? AND sequence > ? ORDER BY sequence",
        )
        .bind(operation.to_string())
        .bind(after)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .iter()
        .map(event_from)
        .collect()
    }
}
