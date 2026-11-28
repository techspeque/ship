//! Config values and secret metadata (phase-1-ws-1.10) against a real
//! SQLite file.

use ship_core::{ConfigScope, ProjectId, ServiceKind, Slug};
use ship_store::{ConfigTarget, NewService, Store, StoreError};

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    project: ConfigTarget,
    environment: ConfigTarget,
    service: ConfigTarget,
    other_service: ConfigTarget,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    let ensured = store
        .get_or_create_project(&Slug::parse("app").unwrap())
        .await
        .unwrap();
    let service = |name: &str| NewService {
        name: name.to_owned(),
        kind: ServiceKind::Web,
        port: None,
    };
    let web = store
        .ensure_service(ensured.environment.id, &service("web"))
        .await
        .unwrap();
    let worker = store
        .ensure_service(ensured.environment.id, &service("worker"))
        .await
        .unwrap();
    Fixture {
        _dir: dir,
        project: ConfigTarget::Project(ensured.project.id),
        environment: ConfigTarget::Environment(ensured.environment.id),
        service: ConfigTarget::Service(web.id),
        other_service: ConfigTarget::Service(worker.id),
        store,
    }
}

#[tokio::test]
async fn config_values_are_set_replaced_listed_and_unset_per_scope() {
    let f = fixture().await;
    let s = &f.store;
    for (target, value) in [(f.project, "p"), (f.environment, "e"), (f.service, "s")] {
        let set = s.set_config_value(target, "LEVEL", value).await.unwrap();
        assert_eq!((set.target, set.key.as_str()), (target, "LEVEL"));
        assert_eq!(set.target.scope(), target.scope());
    }
    s.set_config_value(f.project, "B_FIRST", "1").await.unwrap();
    let replaced = s.set_config_value(f.project, "LEVEL", "p2").await.unwrap();
    assert_eq!(replaced.value, "p2");

    let project = s.list_config_values(f.project).await.unwrap();
    let keys: Vec<(&str, &str)> = project
        .iter()
        .map(|v| (v.key.as_str(), v.value.as_str()))
        .collect();
    assert_eq!(keys, [("B_FIRST", "1"), ("LEVEL", "p2")]);
    assert_eq!(
        s.list_config_values(f.environment).await.unwrap()[0].value,
        "e"
    );
    assert_eq!(s.list_config_values(f.service).await.unwrap()[0].value, "s");
    assert!(
        s.list_config_values(f.other_service)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.service.scope(), ConfigScope::Service);

    assert!(s.unset_config_value(f.service, "LEVEL").await.unwrap());
    assert!(!s.unset_config_value(f.service, "LEVEL").await.unwrap());
    assert!(s.list_config_values(f.service).await.unwrap().is_empty());
    // Other scopes keep theirs.
    assert_eq!(s.list_config_values(f.environment).await.unwrap().len(), 1);
}

#[tokio::test]
async fn secrets_record_references_only_and_hand_back_the_replaced_one() {
    let f = fixture().await;
    let s = &f.store;
    let r1 = s.new_secret_ref().unwrap();
    let r2 = s.new_secret_ref().unwrap();
    assert_ne!(r1, r2);
    assert_eq!(r1.len(), 32);
    assert!(r1.bytes().all(|b| b.is_ascii_hexdigit()));

    let (first, replaced) = s.set_secret(f.project, "TOKEN", &r1).await.unwrap();
    assert_eq!(replaced, None);
    assert_eq!(first.backend_ref, r1);
    let (second, replaced) = s.set_secret(f.project, "TOKEN", &r2).await.unwrap();
    assert_eq!(replaced.as_deref(), Some(r1.as_str()));
    assert_eq!(second.backend_ref, r2);
    s.set_secret(f.service, "TOKEN", &s.new_secret_ref().unwrap())
        .await
        .unwrap();

    let listed = s.list_secrets(f.project).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0], second);
    assert_eq!(s.all_secrets().await.unwrap().len(), 2);

    assert_eq!(
        s.unset_secret(f.project, "TOKEN").await.unwrap().as_deref(),
        Some(r2.as_str())
    );
    assert_eq!(s.unset_secret(f.project, "TOKEN").await.unwrap(), None);
    assert!(s.list_secrets(f.project).await.unwrap().is_empty());
    assert_eq!(s.all_secrets().await.unwrap().len(), 1);

    for bad in ["", &"r".repeat(257)] {
        assert!(matches!(
            s.set_secret(f.project, "TOKEN", bad).await,
            Err(StoreError::Invalid { .. })
        ));
    }
}

#[tokio::test]
async fn a_value_and_a_secret_cannot_share_a_name_in_one_scope() {
    let f = fixture().await;
    let s = &f.store;
    s.set_config_value(f.project, "SHARED", "v").await.unwrap();
    let r = s.new_secret_ref().unwrap();
    assert!(matches!(
        s.set_secret(f.project, "SHARED", &r).await,
        Err(StoreError::Conflict { .. })
    ));
    // Another scope may: the higher one wins at deploy.
    s.set_secret(f.environment, "SHARED", &r).await.unwrap();
    assert!(matches!(
        s.set_config_value(f.environment, "SHARED", "v").await,
        Err(StoreError::Conflict { .. })
    ));
}

#[tokio::test]
async fn unknown_targets_and_overlong_names_are_refused() {
    let f = fixture().await;
    let s = &f.store;
    let ghost = ConfigTarget::Project(ProjectId::from_parts(1_767_225_600_000, 9).unwrap());
    assert!(matches!(
        s.set_config_value(ghost, "A", "v").await,
        Err(StoreError::Conflict { .. })
    ));
    assert!(matches!(
        s.set_secret(ghost, "A", "ref").await,
        Err(StoreError::Conflict { .. })
    ));
    assert!(matches!(
        s.set_config_value(f.project, &"N".repeat(257), "v").await,
        Err(StoreError::Invalid { .. })
    ));
}
