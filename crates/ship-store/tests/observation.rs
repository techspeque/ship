//! Observations (phase-1-ws-1.11) against a real SQLite file: reachable and
//! unreachable observations read back as recorded, a later one replaces
//! the earlier, and each environment lists only its own services'.

use ship_core::{ContainerState, HealthCheck, Observation, ServiceId, ServiceKind, Slug};
use ship_store::{NewObservation, NewService, Store, StoreError};

async fn service(store: &Store, env: ship_core::EnvironmentId, name: &str) -> ServiceId {
    store
        .create_service(
            env,
            &NewService {
                name: name.to_owned(),
                kind: ServiceKind::Web,
                port: None,
            },
        )
        .await
        .unwrap()
        .id
}

fn reachable(container: ContainerState, health: HealthCheck, oom_killed: bool) -> Observation {
    Observation::Reachable {
        container,
        health,
        oom_killed,
    }
}

#[tokio::test]
async fn observations_round_trip_and_replace() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let a = store
        .get_or_create_project(&Slug::parse("a").unwrap())
        .await
        .unwrap();
    let b = store
        .get_or_create_project(&Slug::parse("b").unwrap())
        .await
        .unwrap();
    let web = service(&store, a.environment.id, "web").await;
    let worker = service(&store, a.environment.id, "worker").await;
    let other = service(&store, b.environment.id, "web").await;
    assert!(
        store
            .list_observations(a.environment.id)
            .await
            .unwrap()
            .is_empty()
    );

    let every = [
        reachable(ContainerState::Absent, HealthCheck::None, false),
        reachable(ContainerState::Created, HealthCheck::None, false),
        reachable(ContainerState::Running, HealthCheck::Passing, false),
        reachable(ContainerState::Running, HealthCheck::Starting, false),
        reachable(ContainerState::Running, HealthCheck::Failing, false),
        reachable(ContainerState::Restarting, HealthCheck::None, false),
        reachable(ContainerState::Paused, HealthCheck::None, false),
        reachable(
            ContainerState::Exited { exit_code: 137 },
            HealthCheck::None,
            true,
        ),
        reachable(ContainerState::Dead, HealthCheck::None, false),
        Observation::DockerUnreachable,
    ];
    for (i, &observation) in every.iter().enumerate() {
        let restarts = u64::try_from(i).unwrap();
        let recorded = store
            .record_observations(&[
                NewObservation {
                    service_id: web,
                    observation,
                    restarts,
                },
                NewObservation {
                    service_id: worker,
                    observation: Observation::DockerUnreachable,
                    restarts: 0,
                },
            ])
            .await
            .unwrap();
        assert_eq!(recorded.len(), 2);
        let listed = store.list_observations(a.environment.id).await.unwrap();
        assert_eq!(listed, recorded, "{observation:?}");
        assert_eq!(listed[0].service_id, web);
        assert_eq!(listed[0].observation, observation);
        assert_eq!(listed[0].restarts, restarts);
        assert_eq!(listed[1].observation, Observation::DockerUnreachable);
    }

    store
        .record_observations(&[NewObservation {
            service_id: other,
            observation: reachable(ContainerState::Running, HealthCheck::None, false),
            restarts: 4,
        }])
        .await
        .unwrap();
    let b_list = store.list_observations(b.environment.id).await.unwrap();
    assert_eq!(b_list.len(), 1);
    assert_eq!(b_list[0].service_id, other);
    assert_eq!(
        store
            .list_observations(a.environment.id)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn an_unknown_service_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let missing = ServiceId::from_parts(1, 1).unwrap();
    let err = store
        .record_observations(&[NewObservation {
            service_id: missing,
            observation: Observation::DockerUnreachable,
            restarts: 0,
        }])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }), "{err:?}");
}
