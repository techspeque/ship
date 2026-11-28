//! Routes (ADR-0010): how an environment's services are reached. The set
//! of an environment's routes is replaced as a whole when a deployment
//! succeeds, so it always describes the release that is serving.

use ship_core::{EnvironmentId, RouteId, RouteProvider, RouteStatus, ServiceId, Timestamp};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::Store;
use crate::clock::{new_id, now};
use crate::error::{StoreError, db, write_err};
use crate::repo::{text, time};

/// Input for [`Store::replace_routes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRoute {
    /// A service of the same environment.
    pub service_id: ServiceId,
    pub provider: RouteProvider,
    /// `host:port` (`[v6]:port` for IPv6), 1–255 characters.
    pub address: String,
    pub status: RouteStatus,
}

/// A stored route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub id: RouteId,
    pub environment_id: EnvironmentId,
    pub service_id: ServiceId,
    pub provider: RouteProvider,
    pub address: String,
    pub status: RouteStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

fn route_from(row: &SqliteRow) -> Result<Route, StoreError> {
    const E: &str = "route";
    Ok(Route {
        id: text(row, "id", E)?,
        environment_id: text(row, "environment_id", E)?,
        service_id: text(row, "service_id", E)?,
        provider: text(row, "provider", E)?,
        address: row.try_get("address").map_err(db)?,
        status: text(row, "status", E)?,
        created_at: time(row, "created_at", E)?,
        updated_at: time(row, "updated_at", E)?,
    })
}

impl Store {
    /// Replaces every route of `environment` with `routes`, in one
    /// transaction, and returns the new ones in the order given. An empty
    /// list leaves the environment with no route. A service of another
    /// environment is [`StoreError::Invalid`]; an unknown one, or an
    /// unknown environment, is [`StoreError::Conflict`].
    pub async fn replace_routes(
        &self,
        environment: EnvironmentId,
        routes: &[NewRoute],
    ) -> Result<Vec<Route>, StoreError> {
        let at = now()?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(db)?;
        sqlx::query("DELETE FROM route WHERE environment_id = ?")
            .bind(environment.to_string())
            .execute(&mut *tx)
            .await
            .map_err(write_err("route"))?;
        let mut out = Vec::with_capacity(routes.len());
        for new in routes {
            let owner: Option<String> =
                sqlx::query_scalar("SELECT environment_id FROM service WHERE id = ?")
                    .bind(new.service_id.to_string())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db)?;
            match owner {
                None => return Err(StoreError::Conflict { entity: "route" }),
                Some(env) if env != environment.to_string() => {
                    return Err(StoreError::Invalid { entity: "route" });
                }
                Some(_) => {}
            }
            let route = Route {
                id: new_id(at, RouteId::from_parts)?,
                environment_id: environment,
                service_id: new.service_id,
                provider: new.provider,
                address: new.address.clone(),
                status: new.status,
                created_at: at,
                updated_at: at,
            };
            sqlx::query(
                "INSERT INTO route (id, environment_id, service_id, provider, address, \
                 status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(route.id.to_string())
            .bind(route.environment_id.to_string())
            .bind(route.service_id.to_string())
            .bind(route.provider.as_str())
            .bind(route.address.as_str())
            .bind(route.status.as_str())
            .bind(at.unix_millis())
            .bind(at.unix_millis())
            .execute(&mut *tx)
            .await
            .map_err(write_err("route"))?;
            out.push(route);
        }
        tx.commit().await.map_err(db)?;
        Ok(out)
    }

    /// Every route of `environment`, oldest first.
    pub async fn list_routes(&self, environment: EnvironmentId) -> Result<Vec<Route>, StoreError> {
        sqlx::query(
            "SELECT id, environment_id, service_id, provider, address, status, created_at, \
             updated_at FROM route WHERE environment_id = ? ORDER BY created_at, rowid",
        )
        .bind(environment.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?
        .iter()
        .map(route_from)
        .collect()
    }
}
