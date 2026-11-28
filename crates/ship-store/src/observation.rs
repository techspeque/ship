//! Observations (ADR-0006): the latest of each service, what Docker
//! reported for it, kept apart from desired state and from deployment
//! results. Docker unreachable is stored as such: nothing is known.

use ship_core::{ContainerState, EnvironmentId, HealthCheck, Observation, ServiceId, Timestamp};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::Store;
use crate::clock::now;
use crate::error::{StoreError, db, write_err};
use crate::repo::{text, time};

/// Input for [`Store::record_observations`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewObservation {
    pub service_id: ServiceId,
    pub observation: Observation,
    /// Restarts Docker made under its restart policy.
    pub restarts: u64,
}

/// A stored observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredObservation {
    pub service_id: ServiceId,
    pub observation: Observation,
    pub restarts: u64,
    pub observed_at: Timestamp,
}

const E: &str = "observation";

fn container_text(c: ContainerState) -> (&'static str, Option<i64>) {
    match c {
        ContainerState::Absent => ("absent", None),
        ContainerState::Created => ("created", None),
        ContainerState::Running => ("running", None),
        ContainerState::Restarting => ("restarting", None),
        ContainerState::Paused => ("paused", None),
        ContainerState::Exited { exit_code } => ("exited", Some(exit_code)),
        ContainerState::Dead => ("dead", None),
    }
}

fn health_text(h: HealthCheck) -> &'static str {
    match h {
        HealthCheck::None => "none",
        HealthCheck::Starting => "starting",
        HealthCheck::Passing => "passing",
        HealthCheck::Failing => "failing",
    }
}

fn corrupt(field: &'static str) -> StoreError {
    StoreError::Corrupt { entity: E, field }
}

fn observation_from(row: &SqliteRow) -> Result<StoredObservation, StoreError> {
    let reachable: i64 = row.try_get("reachable").map_err(db)?;
    let observation = if reachable == 0 {
        Observation::DockerUnreachable
    } else {
        let state: Option<String> = row.try_get("container_state").map_err(db)?;
        let exit_code: Option<i64> = row.try_get("exit_code").map_err(db)?;
        let container = match (state.as_deref(), exit_code) {
            (Some("absent"), None) => ContainerState::Absent,
            (Some("created"), None) => ContainerState::Created,
            (Some("running"), None) => ContainerState::Running,
            (Some("restarting"), None) => ContainerState::Restarting,
            (Some("paused"), None) => ContainerState::Paused,
            (Some("exited"), Some(exit_code)) => ContainerState::Exited { exit_code },
            (Some("dead"), None) => ContainerState::Dead,
            _ => return Err(corrupt("container_state")),
        };
        let health: Option<String> = row.try_get("health").map_err(db)?;
        let health = match health.as_deref() {
            Some("none") => HealthCheck::None,
            Some("starting") => HealthCheck::Starting,
            Some("passing") => HealthCheck::Passing,
            Some("failing") => HealthCheck::Failing,
            _ => return Err(corrupt("health")),
        };
        let oom: i64 = row.try_get("oom_killed").map_err(db)?;
        Observation::Reachable {
            container,
            health,
            oom_killed: oom != 0,
        }
    };
    let restarts: i64 = row.try_get("restarts").map_err(db)?;
    Ok(StoredObservation {
        service_id: text(row, "service_id", E)?,
        observation,
        restarts: u64::try_from(restarts).map_err(|_| corrupt("restarts"))?,
        observed_at: time(row, "observed_at", E)?,
    })
}

impl Store {
    /// Records `observations`, now, each replacing the service's previous
    /// one, in one transaction; returns them as stored, in the order
    /// given. An unknown service is [`StoreError::Conflict`].
    pub async fn record_observations(
        &self,
        observations: &[NewObservation],
    ) -> Result<Vec<StoredObservation>, StoreError> {
        let at = now()?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        let mut out = Vec::with_capacity(observations.len());
        for new in observations {
            let (reachable, state, exit_code, health, oom) = match new.observation {
                Observation::DockerUnreachable => (0i64, None, None, None, 0i64),
                Observation::Reachable {
                    container,
                    health,
                    oom_killed,
                } => {
                    let (state, exit_code) = container_text(container);
                    (
                        1,
                        Some(state),
                        exit_code,
                        Some(health_text(health)),
                        i64::from(oom_killed),
                    )
                }
            };
            let restarts = i64::try_from(new.restarts).unwrap_or(i64::MAX);
            sqlx::query(
                "INSERT INTO observation (service_id, reachable, container_state, exit_code, \
                 health, oom_killed, restarts, observed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (service_id) DO UPDATE SET reachable = excluded.reachable, \
                 container_state = excluded.container_state, exit_code = excluded.exit_code, \
                 health = excluded.health, oom_killed = excluded.oom_killed, \
                 restarts = excluded.restarts, observed_at = excluded.observed_at",
            )
            .bind(new.service_id.to_string())
            .bind(reachable)
            .bind(state)
            .bind(exit_code)
            .bind(health)
            .bind(oom)
            .bind(restarts)
            .bind(at.unix_millis())
            .execute(&mut *tx)
            .await
            .map_err(write_err(E))?;
            out.push(StoredObservation {
                service_id: new.service_id,
                observation: new.observation,
                restarts: u64::try_from(restarts).unwrap_or(0),
                observed_at: at,
            });
        }
        tx.commit().await.map_err(db)?;
        Ok(out)
    }

    /// The latest observation of each service of `environment` that has
    /// one, in the services' order (oldest first).
    pub async fn list_observations(
        &self,
        environment: EnvironmentId,
    ) -> Result<Vec<StoredObservation>, StoreError> {
        sqlx::query(
            "SELECT o.service_id, o.reachable, o.container_state, o.exit_code, o.health, \
             o.oom_killed, o.restarts, o.observed_at FROM observation o \
             JOIN service s ON s.id = o.service_id WHERE s.environment_id = ? \
             ORDER BY s.created_at, s.rowid",
        )
        .bind(environment.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .iter()
        .map(observation_from)
        .collect()
    }
}
