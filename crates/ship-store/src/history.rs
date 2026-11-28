//! Repositories for the deployment history (ADR-0007): revisions and
//! releases, which are immutable, and deployments, which are opened once,
//! may gain a release once and are finished once.
//!
//! The schema enforces every rule here (see `migrations/0002_phase1.sql`);
//! the store maps its refusals to typed errors.

use ship_core::{
    DeploymentAction, DeploymentId, DeploymentResult, DesiredState, EnvironmentId, OperationId,
    ProjectId, ReleaseId, RevisionId, RevisionKind, Timestamp,
};
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row};

use crate::Store;
use crate::clock::{new_id, now};
use crate::error::{StoreError, db, illegal, invalid, not_found, write_err};
use crate::repo::{Environment, begin_immediate, environment_in, opt_text, text, time};

/// Input for [`Store::create_revision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewRevision<'a> {
    pub kind: RevisionKind,
    /// Content hash, resolved commit or image digest, per kind.
    pub source_ref: &'a str,
    pub manifest_fingerprint: Option<&'a str>,
}

/// Captured source. Never modified after creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub id: RevisionId,
    pub project_id: ProjectId,
    pub kind: RevisionKind,
    pub source_ref: String,
    pub manifest_fingerprint: Option<String>,
    pub created_at: Timestamp,
}

/// Input for [`Store::create_release`]. Holds no secret value (ADR-0007
/// rule 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewRelease<'a> {
    /// Image ids or digests, in service order.
    pub images: &'a [String],
    /// Fingerprint of the effective non-secret configuration.
    pub config_fingerprint: &'a str,
}

/// Images built from a revision plus their configuration fingerprint. Never
/// modified after creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub id: ReleaseId,
    pub revision_id: RevisionId,
    pub images: Vec<String>,
    pub config_fingerprint: String,
    pub created_at: Timestamp,
}

/// Input for [`Store::create_deployment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewDeployment {
    pub environment_id: EnvironmentId,
    /// Of the environment's project.
    pub revision_id: RevisionId,
    /// Of the same environment. One deployment per operation.
    pub operation_id: OperationId,
    /// The release running when the deployment started, if any.
    pub previous_release_id: Option<ReleaseId>,
}

/// One attempt to apply a revision's release to an environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub id: DeploymentId,
    pub environment_id: EnvironmentId,
    pub revision_id: RevisionId,
    /// `None` until the build produced one; stays `None` when it failed.
    pub release_id: Option<ReleaseId>,
    pub operation_id: OperationId,
    /// `None` while in progress.
    pub result: Option<DeploymentResult>,
    pub previous_release_id: Option<ReleaseId>,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    /// `deploy`, or `remove` for the removal of the environment, which
    /// names the revision that was serving and records no release
    /// (migration 0005).
    pub action: DeploymentAction,
    /// What a deploy was asked with (`--file`, `--port`) as JSON, so it
    /// can be repeated (`ship redeploy`); `None` for a removal and a
    /// deployment from before migration 0005.
    pub options: Option<String>,
}

fn revision_from(row: &SqliteRow) -> Result<Revision, StoreError> {
    const E: &str = "revision";
    Ok(Revision {
        id: text(row, "id", E)?,
        project_id: text(row, "project_id", E)?,
        kind: text(row, "kind", E)?,
        source_ref: row.try_get("source_ref").map_err(db)?,
        manifest_fingerprint: row.try_get("manifest_fingerprint").map_err(db)?,
        created_at: time(row, "created_at", E)?,
    })
}

fn release_from(row: &SqliteRow) -> Result<Release, StoreError> {
    const E: &str = "release";
    let images: String = row.try_get("images").map_err(db)?;
    let images = serde_json::from_str(&images).map_err(|_| StoreError::Corrupt {
        entity: E,
        field: "images",
    })?;
    Ok(Release {
        id: text(row, "id", E)?,
        revision_id: text(row, "revision_id", E)?,
        images,
        config_fingerprint: row.try_get("config_fingerprint").map_err(db)?,
        created_at: time(row, "created_at", E)?,
    })
}

fn deployment_from(row: &SqliteRow) -> Result<Deployment, StoreError> {
    const E: &str = "deployment";
    let ended_at: Option<i64> = row.try_get("ended_at").map_err(db)?;
    let ended_at = ended_at
        .map(Timestamp::from_unix_millis)
        .transpose()
        .map_err(|_| StoreError::Corrupt {
            entity: E,
            field: "ended_at",
        })?;
    Ok(Deployment {
        id: text(row, "id", E)?,
        environment_id: text(row, "environment_id", E)?,
        revision_id: text(row, "revision_id", E)?,
        release_id: opt_text(row, "release_id", E)?,
        operation_id: text(row, "operation_id", E)?,
        result: opt_text(row, "result", E)?,
        previous_release_id: opt_text(row, "previous_release_id", E)?,
        started_at: time(row, "started_at", E)?,
        ended_at,
        action: text(row, "action", E)?,
        options: row.try_get("options").map_err(db)?,
    })
}

const REVISION_COLUMNS: &str = "id, project_id, kind, source_ref, manifest_fingerprint, created_at";
const RELEASE_COLUMNS: &str = "id, revision_id, images, config_fingerprint, created_at";
const DEPLOYMENT_COLUMNS: &str = "id, environment_id, revision_id, release_id, operation_id, \
     result, previous_release_id, started_at, ended_at, action, options";

impl Store {
    // ---- revisions ----

    /// Records a revision of `project`. An unknown project is
    /// [`StoreError::Conflict`].
    pub async fn create_revision(
        &self,
        project: ProjectId,
        new: &NewRevision<'_>,
    ) -> Result<Revision, StoreError> {
        let created_at = now()?;
        let revision = Revision {
            id: new_id(created_at, RevisionId::from_parts)?,
            project_id: project,
            kind: new.kind,
            source_ref: new.source_ref.to_owned(),
            manifest_fingerprint: new.manifest_fingerprint.map(str::to_owned),
            created_at,
        };
        sqlx::query(
            "INSERT INTO revision \
             (id, project_id, kind, source_ref, manifest_fingerprint, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(revision.id.to_string())
        .bind(revision.project_id.to_string())
        .bind(revision.kind.as_str())
        .bind(revision.source_ref.as_str())
        .bind(revision.manifest_fingerprint.as_deref())
        .bind(revision.created_at.unix_millis())
        .execute(&self.pool)
        .await
        .map_err(write_err("revision"))?;
        Ok(revision)
    }

    /// The revision with `id`.
    pub async fn get_revision(&self, id: RevisionId) -> Result<Revision, StoreError> {
        // Built from a compile-time constant only; values are bound.
        let sql = format!("SELECT {REVISION_COLUMNS} FROM revision WHERE id = ?");
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or(not_found("revision"))?;
        revision_from(&row)
    }

    /// Every revision of `project`, oldest first.
    pub async fn list_revisions(&self, project: ProjectId) -> Result<Vec<Revision>, StoreError> {
        // Built from a compile-time constant only; values are bound.
        let sql = format!(
            "SELECT {REVISION_COLUMNS} FROM revision WHERE project_id = ? \
             ORDER BY created_at, rowid"
        );
        sqlx::query(AssertSqlSafe(sql))
            .bind(project.to_string())
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
            .iter()
            .map(revision_from)
            .collect()
    }

    // ---- releases ----

    /// Records a release built from `revision`. An unknown revision is
    /// [`StoreError::Conflict`].
    pub async fn create_release(
        &self,
        revision: RevisionId,
        new: &NewRelease<'_>,
    ) -> Result<Release, StoreError> {
        let id = self.allocate_release_id()?;
        self.create_release_as(id, revision, new).await
    }

    /// A new release id, recorded nowhere yet. A deployment needs its
    /// release's id before the build (for the image tag and the labels);
    /// [`Store::create_release_as`] records the release under it.
    pub fn allocate_release_id(&self) -> Result<ReleaseId, StoreError> {
        new_id(now()?, ReleaseId::from_parts)
    }

    /// [`Store::create_release`] under an id from
    /// [`Store::allocate_release_id`]. An id already recorded is
    /// [`StoreError::Conflict`].
    pub async fn create_release_as(
        &self,
        id: ReleaseId,
        revision: RevisionId,
        new: &NewRelease<'_>,
    ) -> Result<Release, StoreError> {
        let created_at = now()?;
        let release = Release {
            id,
            revision_id: revision,
            images: new.images.to_vec(),
            config_fingerprint: new.config_fingerprint.to_owned(),
            created_at,
        };
        let images = serde_json::to_string(&release.images).map_err(|_| invalid("release"))?;
        sqlx::query(
            "INSERT INTO release (id, revision_id, images, config_fingerprint, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(release.id.to_string())
        .bind(release.revision_id.to_string())
        .bind(images)
        .bind(release.config_fingerprint.as_str())
        .bind(release.created_at.unix_millis())
        .execute(&self.pool)
        .await
        .map_err(write_err("release"))?;
        Ok(release)
    }

    /// The release with `id`.
    pub async fn get_release(&self, id: ReleaseId) -> Result<Release, StoreError> {
        // Built from a compile-time constant only; values are bound.
        let sql = format!("SELECT {RELEASE_COLUMNS} FROM release WHERE id = ?");
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or(not_found("release"))?;
        release_from(&row)
    }

    /// Every release built from a revision of `project`, oldest first.
    pub async fn list_releases(&self, project: ProjectId) -> Result<Vec<Release>, StoreError> {
        sqlx::query(
            "SELECT l.id, l.revision_id, l.images, l.config_fingerprint, l.created_at \
               FROM release l JOIN revision r ON r.id = l.revision_id \
              WHERE r.project_id = ? ORDER BY l.created_at, l.rowid",
        )
        .bind(project.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .iter()
        .map(release_from)
        .collect()
    }

    // ---- deployments ----

    /// Opens a deployment, with no release and no result yet.
    ///
    /// An unknown environment, revision, operation or previous release, or
    /// an operation that already has a deployment, is
    /// [`StoreError::Conflict`]. An operation of another environment, or a
    /// revision of another project, is [`StoreError::Invalid`].
    pub async fn create_deployment(&self, new: &NewDeployment) -> Result<Deployment, StoreError> {
        self.create_deployment_with(new, None).await
    }

    /// [`Store::create_deployment`] recording the options the deploy was
    /// asked with (JSON), so `ship redeploy` can repeat it.
    pub async fn create_deployment_with(
        &self,
        new: &NewDeployment,
        options: Option<&str>,
    ) -> Result<Deployment, StoreError> {
        let deployment = open_deployment(new, DeploymentAction::Deploy, options)?;
        insert_deployment(&mut *self.pool.acquire().await.map_err(db)?, &deployment).await?;
        Ok(deployment)
    }

    /// Records the removal of `new.environment_id` in its history, opened
    /// and succeeded at once: a `remove` deployment naming the revision
    /// that was serving (`new.revision_id`) and the release it replaced
    /// (`new.previous_release_id`), with no release of its own. The rules
    /// of [`Store::create_deployment`] apply.
    pub async fn record_removal(&self, new: &NewDeployment) -> Result<Deployment, StoreError> {
        let deployment = open_deployment(new, DeploymentAction::Remove, None)?;
        let mut tx = begin_immediate(self).await?;
        insert_deployment(&mut tx, &deployment).await?;
        let ended_at = now()?;
        sqlx::query("UPDATE deployment SET result = 'succeeded', ended_at = ? WHERE id = ?")
            .bind(ended_at.unix_millis())
            .bind(deployment.id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(write_err("deployment"))?;
        tx.commit().await.map_err(db)?;
        Ok(Deployment {
            result: Some(DeploymentResult::Succeeded),
            ended_at: Some(ended_at),
            ..deployment
        })
    }

    /// Brings a removed environment back to desired `running` through
    /// `deployment`, the `ship push --recreate` that just succeeded (Human,
    /// 2026-11-28). The only way out of `removed`: `deployment` must be a
    /// succeeded `deploy` of this environment with no removal after it in
    /// the history's order (started_at, then insertion), else [`StoreError::IllegalTransition`],
    /// which the schema's trigger enforces too. An environment that is not
    /// removed is returned unchanged.
    pub async fn recreate_environment(
        &self,
        environment: EnvironmentId,
        deployment: DeploymentId,
    ) -> Result<Environment, StoreError> {
        let mut tx = begin_immediate(self).await?;
        let env = environment_in(&mut tx, environment).await?;
        if env.desired_state != DesiredState::Removed {
            return Ok(env);
        }
        // Built from a compile-time constant only; values are bound.
        let sql = format!("SELECT {DEPLOYMENT_COLUMNS} FROM deployment WHERE id = ?");
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(deployment.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or(not_found("deployment"))?;
        let d = deployment_from(&row)?;
        // "Later" in the history's order, (started_at, rowid): a deploy
        // started in the removal's millisecond but inserted before it is
        // not after it.
        let removed_later: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM deployment d, deployment r
              WHERE d.id = ? AND r.environment_id = ? AND r.action = 'remove'
                AND (r.started_at, r.rowid) > (d.started_at, d.rowid))",
        )
        .bind(deployment.to_string())
        .bind(environment.to_string())
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let through = d.environment_id == environment
            && d.action == DeploymentAction::Deploy
            && d.result == Some(DeploymentResult::Succeeded)
            && !removed_later;
        if !through {
            return Err(illegal("environment"));
        }
        sqlx::query("UPDATE environment SET desired_state = 'running' WHERE id = ?")
            .bind(environment.to_string())
            .execute(&mut *tx)
            .await
            .map_err(write_err("environment"))?;
        tx.commit().await.map_err(db)?;
        Ok(Environment {
            desired_state: DesiredState::Running,
            ..env
        })
    }

    /// The deployment with `id`.
    pub async fn get_deployment(&self, id: DeploymentId) -> Result<Deployment, StoreError> {
        // Built from a compile-time constant only; values are bound.
        let sql = format!("SELECT {DEPLOYMENT_COLUMNS} FROM deployment WHERE id = ?");
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .ok_or(not_found("deployment"))?;
        deployment_from(&row)
    }

    /// The deployment `operation` recorded, if any (one per operation).
    pub async fn deployment_for_operation(
        &self,
        operation: OperationId,
    ) -> Result<Option<Deployment>, StoreError> {
        // Built from a compile-time constant only; values are bound.
        let sql = format!("SELECT {DEPLOYMENT_COLUMNS} FROM deployment WHERE operation_id = ?");
        sqlx::query(AssertSqlSafe(sql))
            .bind(operation.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?
            .as_ref()
            .map(deployment_from)
            .transpose()
    }

    /// Every deployment of `environment`, newest first, as
    /// `ship deployments` prints them.
    pub async fn list_deployments(
        &self,
        environment: EnvironmentId,
    ) -> Result<Vec<Deployment>, StoreError> {
        // Built from a compile-time constant only; values are bound.
        let sql = format!(
            "SELECT {DEPLOYMENT_COLUMNS} FROM deployment WHERE environment_id = ? \
             ORDER BY started_at DESC, rowid DESC"
        );
        sqlx::query(AssertSqlSafe(sql))
            .bind(environment.to_string())
            .fetch_all(&self.pool)
            .await
            .map_err(db)?
            .iter()
            .map(deployment_from)
            .collect()
    }

    /// Records the release the deployment's build produced. Allowed once,
    /// before the deployment ends; otherwise
    /// [`StoreError::IllegalTransition`]. A release of another revision is
    /// [`StoreError::Invalid`].
    pub async fn set_deployment_release(
        &self,
        id: DeploymentId,
        release: ReleaseId,
    ) -> Result<Deployment, StoreError> {
        // Built from a compile-time constant only; values are bound.
        let sql = format!(
            "UPDATE deployment SET release_id = ? WHERE id = ? RETURNING {DEPLOYMENT_COLUMNS}"
        );
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(release.to_string())
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(write_err("deployment"))?
            .ok_or(not_found("deployment"))?;
        deployment_from(&row)
    }

    /// Ends the deployment with `result`, now. Allowed once; `succeeded`
    /// needs a release. Otherwise [`StoreError::IllegalTransition`].
    pub async fn finish_deployment(
        &self,
        id: DeploymentId,
        result: DeploymentResult,
    ) -> Result<Deployment, StoreError> {
        let ended_at = now()?;
        // Built from a compile-time constant only; values are bound.
        let sql = format!(
            "UPDATE deployment SET result = ?, ended_at = ? WHERE id = ? \
             RETURNING {DEPLOYMENT_COLUMNS}"
        );
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(result.as_str())
            .bind(ended_at.unix_millis())
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(write_err("deployment"))?
            .ok_or(not_found("deployment"))?;
        deployment_from(&row)
    }
}

/// A deployment of `action` about to be inserted, open.
fn open_deployment(
    new: &NewDeployment,
    action: DeploymentAction,
    options: Option<&str>,
) -> Result<Deployment, StoreError> {
    let started_at = now()?;
    Ok(Deployment {
        id: new_id(started_at, DeploymentId::from_parts)?,
        environment_id: new.environment_id,
        revision_id: new.revision_id,
        release_id: None,
        operation_id: new.operation_id,
        result: None,
        previous_release_id: new.previous_release_id,
        started_at,
        ended_at: None,
        action,
        options: options.map(str::to_owned),
    })
}

async fn insert_deployment(
    conn: &mut sqlx::sqlite::SqliteConnection,
    d: &Deployment,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO deployment \
         (id, environment_id, revision_id, operation_id, previous_release_id, started_at, \
          action, options) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(d.id.to_string())
    .bind(d.environment_id.to_string())
    .bind(d.revision_id.to_string())
    .bind(d.operation_id.to_string())
    .bind(d.previous_release_id.map(|r| r.to_string()))
    .bind(d.started_at.unix_millis())
    .bind(d.action.as_str())
    .bind(d.options.as_deref())
    .execute(&mut *conn)
    .await
    .map_err(write_err("deployment"))?;
    Ok(())
}
