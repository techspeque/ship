//! The append-only audit log (OVERVIEW §2.2). The schema refuses any
//! update or delete of an audit event.

use ship_core::Timestamp;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::Store;
use crate::clock::now;
use crate::error::{StoreError, db, write_err};
use crate::repo::time;

/// Input for [`Store::append_audit_event`]. Never carries a secret value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewAuditEvent<'a> {
    /// Who acted: an OS user, a tailnet identity, or `policy` for a
    /// decision taken by a host setting.
    pub actor: &'a str,
    pub action: &'a str,
    /// The id or name acted on.
    pub target: &'a str,
    /// What exactly was decided, as JSON (migration 0004): a Compose
    /// file's fingerprint and items, say.
    pub detail: Option<&'a str>,
}

/// One audit event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// Increases with every append.
    pub id: i64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: Option<String>,
    pub ts: Timestamp,
}

fn audit_from(row: &SqliteRow) -> Result<AuditEvent, StoreError> {
    const E: &str = "audit_event";
    Ok(AuditEvent {
        id: row.try_get("id").map_err(db)?,
        actor: row.try_get("actor").map_err(db)?,
        action: row.try_get("action").map_err(db)?,
        target: row.try_get("target").map_err(db)?,
        detail: row.try_get("detail").map_err(db)?,
        ts: time(row, "ts", E)?,
    })
}

impl Store {
    /// Appends an audit event, stamped now.
    pub async fn append_audit_event(
        &self,
        event: &NewAuditEvent<'_>,
    ) -> Result<AuditEvent, StoreError> {
        let ts = now()?;
        let row = sqlx::query(
            "INSERT INTO audit_event (actor, action, target, detail, ts) VALUES (?, ?, ?, ?, ?) \
             RETURNING id, actor, action, target, detail, ts",
        )
        .bind(event.actor)
        .bind(event.action)
        .bind(event.target)
        .bind(event.detail)
        .bind(ts.unix_millis())
        .fetch_one(&self.pool)
        .await
        .map_err(write_err("audit_event"))?;
        audit_from(&row)
    }

    /// Every audit event, in the order appended.
    pub async fn list_audit_events(&self) -> Result<Vec<AuditEvent>, StoreError> {
        sqlx::query("SELECT id, actor, action, target, detail, ts FROM audit_event ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
            .iter()
            .map(audit_from)
            .collect()
    }

    /// The audit events with `action` on `target`, in the order appended.
    pub async fn audit_events_for(
        &self,
        action: &str,
        target: &str,
    ) -> Result<Vec<AuditEvent>, StoreError> {
        sqlx::query(
            "SELECT id, actor, action, target, detail, ts FROM audit_event \
             WHERE action = ? AND target = ? ORDER BY id",
        )
        .bind(action)
        .bind(target)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .iter()
        .map(audit_from)
        .collect()
    }
}
