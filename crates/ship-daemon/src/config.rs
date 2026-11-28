//! Config values and write-only secrets (ADR-0009), over the API and at
//! deploy.
//!
//! The API: `GET|POST /api/v1/projects/{project}/vars`, `POST …/vars/unset`
//! and the same for `secrets`. The scope is the project, an environment
//! (`env`) or one of its services (`service`; `env` then defaults to
//! `development`). The project, environment, service, name and value are
//! checked before any operation; the change is then a `config` operation
//! on the project's `development` environment (ADR-0011), serialised with
//! its deployments, which appends an audit event naming the scope and the
//! name, never a value. A secret's value is registered with the
//! [`Redactor`] as its request is read, is written to the
//! [`SecretStore`] and is never answered back: lists give names and
//! timestamps.
//!
//! At deploy ([`prepare`]), each service's entries are resolved, lowest
//! first, from the manifest defaults (none in Phase 1: Ship reads no
//! manifest of its own yet), the project, the environment and the service
//! (rule 1), and written to `<release>/env/<service>.env` (0600 in a 0700
//! directory), outside the snapshot. `<release>/config.yaml`, the last of
//! the release's Compose files, names those env files and holds no value;
//! the build, the labels override and the trust profile run before it is
//! added, so nothing Compose is asked to build or Ship evaluates holds a
//! value (rule 5). `<release>/config.json` records a stamp per service, a
//! SHA-256 of the names, the config values and the secrets' backend
//! references, so [`stale_services`] can tell which services run with
//! values older than the stored ones (rule 6) without keeping a value.
//! [`prune`] removes the env files of releases that will not run again.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use ship_core::api::{SecretList, SecretView, VarList, VarView};
use ship_core::config::{self as rules, ConfigError};
use ship_core::{
    ApiError, DeploymentResult, EnvironmentId, ErrorCode, OperationPhase, ReleaseId, SecretValue,
    Slug,
};
use ship_docker::ComposeFiles;
use ship_store::{
    ConfigTarget, DEVELOPMENT, Environment, NewAuditEvent, Project, SecretRecord, Service, Store,
    StoreError,
};

use crate::deploy::{
    Deployer, internal_docker, io_failure, literal, path_text, release_dir, write_file,
};
use crate::queue::{Context, JobFailure};
use crate::secrets::SecretStore;
use crate::settings::OPERATOR;

/// The release's override naming the services' env files.
pub const CONFIG_FILE: &str = "config.yaml";
/// The release's per-service stamps.
pub const STAMPS_FILE: &str = "config.json";
/// The release's directory of env files.
pub const ENV_DIR: &str = "env";
/// The key of `config.json` naming, per service, which entries of its env
/// file are secrets (names only), so a restarted daemon can redact the
/// values a retained release still runs with ([`crate::secrets::register_all`]).
pub const SECRET_NAMES: &str = "secrets";

// ---- scopes ----

/// A scope named by a request, found in the store.
#[derive(Debug, Clone)]
pub struct Scope {
    pub project: Project,
    /// The project's `development` environment: the one whose operations a
    /// change is serialised with.
    pub lock: Environment,
    /// The environment named, if any.
    pub environment: Option<Environment>,
    /// The service named, if any.
    pub service: Option<Service>,
}

impl Scope {
    /// Where values set in this scope are stored.
    pub fn target(&self) -> ConfigTarget {
        match (&self.service, &self.environment) {
            (Some(s), _) => ConfigTarget::Service(s.id),
            (None, Some(e)) => ConfigTarget::Environment(e.id),
            (None, None) => ConfigTarget::Project(self.project.id),
        }
    }

    /// "project my-app", "environment development of my-app", …
    pub fn describe(&self) -> String {
        let p = &self.project.slug;
        match (&self.service, &self.environment) {
            (Some(s), Some(e)) => format!("service {} in {} of {p}", s.name, e.name),
            (None, Some(e)) => format!("environment {} of {p}", e.name),
            _ => format!("project {p}"),
        }
    }
}

fn not_found(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::NotFound, message)
}

fn database(e: &StoreError) -> ApiError {
    tracing::error!(error = %e, "reading the database failed");
    ApiError::new(
        ErrorCode::Internal,
        "the daemon could not read its database",
    )
}

/// Finds the scope `project`, `env` and `service` name: `400` for a bad
/// slug, `404` for a project, environment or service that does not exist.
pub async fn find_scope(
    store: &Store,
    project: &str,
    env: Option<&str>,
    service: Option<&str>,
) -> Result<Scope, ApiError> {
    let slug = Slug::parse(project)?;
    let project = store
        .list_projects()
        .await
        .map_err(|e| database(&e))?
        .into_iter()
        .find(|p| p.slug == slug)
        .ok_or_else(|| not_found("no such project"))?;
    let environments = store
        .list_environments(project.id)
        .await
        .map_err(|e| database(&e))?;
    let lock = environments
        .iter()
        .find(|e| e.name.as_str() == DEVELOPMENT)
        .cloned()
        .ok_or_else(|| not_found("the project has no development environment"))?;
    let environment = match (env, service) {
        (None, None) => None,
        (name, _) => {
            let name = name.unwrap_or(DEVELOPMENT);
            Some(
                environments
                    .iter()
                    .find(|e| e.name.as_str() == name)
                    .cloned()
                    .ok_or_else(|| {
                        not_found(format!(
                            "no such environment; the project's environments are {}",
                            environments
                                .iter()
                                .map(|e| e.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?,
            )
        }
    };
    let service = match (service, &environment) {
        (Some(name), Some(env)) => Some(
            store
                .list_services(env.id)
                .await
                .map_err(|e| database(&e))?
                .into_iter()
                .find(|s| s.name == name)
                .ok_or_else(|| {
                    not_found(
                        "no such service in that environment; a service is known from its \
                         first deployment",
                    )
                })?,
        ),
        _ => None,
    };
    Ok(Scope {
        project,
        lock,
        environment,
        service,
    })
}

/// `env` and `service` from a list's query string.
pub fn parse_query(query: Option<&str>) -> Result<(Option<String>, Option<String>), ApiError> {
    let invalid = |why: &str| ApiError::new(ErrorCode::InvalidRequest, why.to_owned());
    let (mut env, mut service) = (None, None);
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| invalid("a query parameter has no value"))?;
        let v = ship_core::api::percent_decode(v)
            .ok_or_else(|| invalid("a query parameter is not percent-encoded UTF-8"))?;
        match k {
            "env" if env.is_none() => env = Some(v),
            "service" if service.is_none() => service = Some(v),
            _ => return Err(invalid("the query takes env and service, once each")),
        }
    }
    Ok((env, service))
}

/// `400 invalid_request` for a name or value outside the rules.
pub fn rule_error(what: &str, e: ConfigError) -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest, format!("{what}: {e}"))
}

// ---- lists ----

/// The config values of `scope`, with the stale services.
pub async fn var_list(store: &Store, state_dir: &Path, scope: &Scope) -> Result<VarList, ApiError> {
    let vars = store
        .list_config_values(scope.target())
        .await
        .map_err(|e| database(&e))?
        .into_iter()
        .map(|v| VarView {
            name: v.key,
            value: v.value,
            updated_at: v.updated_at,
        })
        .collect();
    let stale_services = stale_services(store, state_dir, &scope.lock)
        .await
        .map_err(|e| database(&e))?;
    Ok(VarList {
        scope: scope.target().scope(),
        project: scope.project.slug.clone(),
        environment: scope.environment.as_ref().map(|e| e.name.to_string()),
        service: scope.service.as_ref().map(|s| s.name.clone()),
        vars,
        stale_services,
    })
}

/// The secrets of `scope`, names and timestamps only, with the stale
/// services.
pub async fn secret_list(
    store: &Store,
    state_dir: &Path,
    scope: &Scope,
) -> Result<SecretList, ApiError> {
    let secrets = store
        .list_secrets(scope.target())
        .await
        .map_err(|e| database(&e))?
        .into_iter()
        .map(|s| SecretView {
            name: s.name,
            updated_at: s.updated_at,
        })
        .collect();
    let stale_services = stale_services(store, state_dir, &scope.lock)
        .await
        .map_err(|e| database(&e))?;
    Ok(SecretList {
        scope: scope.target().scope(),
        project: scope.project.slug.clone(),
        environment: scope.environment.as_ref().map(|e| e.name.to_string()),
        service: scope.service.as_ref().map(|s| s.name.clone()),
        secrets,
        stale_services,
    })
}

// ---- changes (the `config` operation's jobs) ----

/// What a `config` operation changes.
#[derive(Clone)]
pub enum Change {
    SetVar { name: String, value: String },
    UnsetVar { name: String },
    SetSecret { name: String, value: SecretValue },
    UnsetSecret { name: String },
}

impl std::fmt::Debug for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the values: a config value is not secret, but there is no
        // need for it here either.
        let (action, name) = self.parts();
        write!(f, "{action} {name}")
    }
}

impl Change {
    fn parts(&self) -> (&'static str, &str) {
        match self {
            Change::SetVar { name, .. } => ("vars set", name),
            Change::UnsetVar { name } => ("vars unset", name),
            Change::SetSecret { name, .. } => ("secrets set", name),
            Change::UnsetSecret { name } => ("secrets unset", name),
        }
    }
}

fn conflict(name: &str, other: &str) -> JobFailure {
    JobFailure::new(
        ErrorCode::Conflict,
        format!("{name} is a {other} in this scope; unset it there first"),
    )
}

/// The `config` job: applies `change` to `scope` and records it in the
/// audit log. Nothing running changes until a redeploy (rule 6).
pub async fn apply(
    ctx: &Context,
    store: &Store,
    secrets: &SecretStore,
    scope: &Scope,
    change: Change,
) -> Result<(), JobFailure> {
    let target = scope.target();
    let (action, name) = change.parts();
    let name = name.to_owned();
    let place = scope.describe();
    ctx.enter(
        OperationPhase::Validating,
        &format!("{action} {name} for the {place}"),
    )
    .await?;
    let line = match change {
        Change::SetVar { name, value } => {
            match store.set_config_value(target, &name, &value).await {
                Err(StoreError::Conflict { .. }) => return Err(conflict(&name, "secret")),
                r => r?,
            };
            format!("{name} is set for the {place}")
        }
        Change::UnsetVar { name } => {
            if store.unset_config_value(target, &name).await? {
                format!("{name} is unset for the {place}")
            } else {
                format!("{name} was not set for the {place}; nothing changed")
            }
        }
        Change::SetSecret { name, value } => {
            let backend_ref = store.new_secret_ref()?;
            secrets
                .write(&backend_ref, &value)
                .map_err(|e| io_failure(&e, "the daemon could not write to its secret store"))?;
            let (_, replaced) = match store.set_secret(target, &name, &backend_ref).await {
                Ok(done) => done,
                Err(e) => {
                    let _ = secrets.remove(&backend_ref);
                    return Err(match e {
                        StoreError::Conflict { .. } => conflict(&name, "config value"),
                        e => e.into(),
                    });
                }
            };
            if let Some(old) = replaced
                && let Err(e) = secrets.remove(&old)
            {
                tracing::warn!(secret = %name, error = %e, "a replaced secret value was not removed");
            }
            format!("secret {name} is set for the {place}")
        }
        Change::UnsetSecret { name } => match store.unset_secret(target, &name).await? {
            Some(old) => {
                if let Err(e) = secrets.remove(&old) {
                    tracing::warn!(secret = %name, error = %e, "an unset secret value was not removed");
                }
                format!("secret {name} is unset for the {place}")
            }
            None => format!("secret {name} was not set for the {place}; nothing changed"),
        },
    };
    let target_id = match target {
        ConfigTarget::Project(p) => p.to_string(),
        ConfigTarget::Environment(e) => e.to_string(),
        ConfigTarget::Service(s) => s.to_string(),
    };
    let detail = json!({
        "scope": target.scope().as_str(),
        "name": name,
        "project_id": scope.project.id.to_string(),
    })
    .to_string();
    store
        .append_audit_event(&NewAuditEvent {
            actor: OPERATOR,
            action,
            target: &target_id,
            detail: Some(&detail),
        })
        .await?;
    ctx.log(&line).await?;
    ctx.log("running services keep their values until the next deployment")
        .await?;
    Ok(())
}

// ---- deploy ----

/// One resolved entry of a service.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Var(String),
    Secret(SecretRecord),
}

/// The config of a project's environment as stored now: its layers.
struct Layers {
    project: BTreeMap<String, Entry>,
    environment: BTreeMap<String, Entry>,
    services: BTreeMap<String, BTreeMap<String, Entry>>,
}

async fn layer(store: &Store, target: ConfigTarget) -> Result<BTreeMap<String, Entry>, StoreError> {
    let mut out = BTreeMap::new();
    for v in store.list_config_values(target).await? {
        out.insert(v.key, Entry::Var(v.value));
    }
    for s in store.list_secrets(target).await? {
        out.insert(s.name.clone(), Entry::Secret(s));
    }
    Ok(out)
}

async fn layers(
    store: &Store,
    environment: &Environment,
    services: &[Service],
) -> Result<Layers, StoreError> {
    let mut by_service = BTreeMap::new();
    for s in services {
        by_service.insert(
            s.name.clone(),
            layer(store, ConfigTarget::Service(s.id)).await?,
        );
    }
    Ok(Layers {
        project: layer(store, ConfigTarget::Project(environment.project_id)).await?,
        environment: layer(store, ConfigTarget::Environment(environment.id)).await?,
        services: by_service,
    })
}

impl Layers {
    /// `service`'s entries (ADR-0009 rule 1).
    fn resolve(&self, service: &str) -> BTreeMap<String, Entry> {
        let defaults = BTreeMap::new();
        let none = BTreeMap::new();
        let own = self.services.get(service).unwrap_or(&none);
        rules::resolve(&[&defaults, &self.project, &self.environment, own])
    }
}

/// `sha256:<hex>` of `entries`: names, config values and secrets' backend
/// references, so any change of a value, a set or an unset changes it.
fn stamp(entries: &BTreeMap<String, Entry>) -> String {
    let list: Vec<Value> = entries
        .iter()
        .map(|(name, e)| match e {
            Entry::Var(v) => json!(["var", name, v]),
            Entry::Secret(s) => json!(["secret", name, s.backend_ref]),
        })
        .collect();
    let digest = Sha256::digest(Value::Array(list).to_string().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// The non-secret part of `entries`, for the release fingerprint: config
/// values with their values, secrets by name.
fn fingerprint_part(entries: &BTreeMap<String, Entry>) -> Value {
    let list: Vec<Value> = entries
        .iter()
        .map(|(name, e)| match e {
            Entry::Var(v) => json!(["var", name, v]),
            Entry::Secret(_) => json!(["secret", name]),
        })
        .collect();
    Value::Array(list)
}

/// What [`prepare`] wrote for a release.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// `config.yaml`, when some service has an entry.
    override_file: Option<PathBuf>,
    /// The services' non-secret config, for the release fingerprint.
    pub fingerprint: Value,
    /// Each service's entry names.
    pub names: BTreeMap<String, Vec<String>>,
}

impl Prepared {
    /// `files` with `config.yaml` added last, when there is one.
    pub fn files(&self, files: ComposeFiles) -> Result<ComposeFiles, JobFailure> {
        match &self.override_file {
            None => Ok(files),
            Some(path) => {
                let mut list: Vec<PathBuf> = files.files().to_vec();
                list.push(path.clone());
                ComposeFiles::new(files.directory(), list).map_err(internal_docker)
            }
        }
    }
}

/// Writes the env files, `config.yaml` and `config.json` of the release in
/// `dir` for `services` of `job`'s environment, and logs which names each
/// service gets (never a value).
pub async fn prepare(
    ctx: &Context,
    d: &Deployer,
    environment: &Environment,
    services: &[Service],
    dir: &Path,
) -> Result<Prepared, JobFailure> {
    let store = d.store();
    let secrets = SecretStore::new(d.state_dir());
    let layers = layers(store, environment, services).await?;
    let env_dir = dir.join(ENV_DIR);
    let mut stamps = serde_json::Map::new();
    let mut secret_names_of = serde_json::Map::new();
    let mut fingerprint = serde_json::Map::new();
    let mut names = BTreeMap::new();
    let mut overrides = serde_json::Map::new();
    for service in services {
        let entries = layers.resolve(&service.name);
        stamps.insert(service.name.clone(), json!(stamp(&entries)));
        fingerprint.insert(service.name.clone(), fingerprint_part(&entries));
        names.insert(service.name.clone(), entries.keys().cloned().collect());
        if entries.is_empty() {
            continue;
        }
        let mut text = String::new();
        for (name, entry) in &entries {
            match entry {
                Entry::Var(value) => text.push_str(&rules::env_file_line(name, value)),
                Entry::Secret(record) => {
                    let value = secrets.read(&record.backend_ref).map_err(|e| {
                        tracing::error!(secret = %name, error = %e, "a secret's value cannot be read");
                        JobFailure::new(
                            ErrorCode::Internal,
                            format!("the value of secret {name} cannot be read; set it again"),
                        )
                    })?;
                    text.push_str(&rules::env_file_line(name, value.expose()));
                }
            }
        }
        create_private_dir(&env_dir)?;
        let path = env_dir.join(format!("{}.env", service.name));
        write_file(&path, text.as_bytes())?;
        overrides.insert(
            service.name.clone(),
            json!({ "env_file": [literal(&path_text(&path)?)] }),
        );
        let (vars, secret_names): (Vec<_>, Vec<_>) = entries
            .iter()
            .partition(|(_, e)| matches!(e, Entry::Var(_)));
        if !secret_names.is_empty() {
            let list: Vec<&str> = secret_names.iter().map(|(n, _)| n.as_str()).collect();
            secret_names_of.insert(service.name.clone(), json!(list));
        }
        let list = |v: Vec<(&String, &Entry)>| {
            v.into_iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut line = format!("{} gets", service.name);
        if !vars.is_empty() {
            line.push_str(&format!(" config {}", list(vars)));
            if !secret_names.is_empty() {
                line.push(';');
            }
        }
        if !secret_names.is_empty() {
            line.push_str(&format!(" secrets {}", list(secret_names)));
        }
        ctx.log(&line).await?;
    }
    write_file(
        &dir.join(STAMPS_FILE),
        json!({ "services": stamps, SECRET_NAMES: secret_names_of })
            .to_string()
            .as_bytes(),
    )?;
    let override_file = if overrides.is_empty() {
        None
    } else {
        let path = dir.join(CONFIG_FILE);
        let doc = json!({ "services": overrides });
        let text = serde_json::to_string_pretty(&doc).map_err(|_| {
            JobFailure::new(ErrorCode::Internal, "could not write the config override")
        })?;
        write_file(&path, text.as_bytes())?;
        Some(path)
    };
    Ok(Prepared {
        override_file,
        fingerprint: Value::Object(fingerprint),
        names,
    })
}

fn create_private_dir(dir: &Path) -> Result<(), JobFailure> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(io_failure(
            &e,
            "the daemon could not create the release's env directory",
        )),
        _ => Ok(()),
    }
}

/// The services of `environment`'s serving release whose stored config
/// differs from what that release was deployed with, by name. None when
/// nothing has been deployed. A release from before config existed was
/// deployed with none.
pub async fn stale_services(
    store: &Store,
    state_dir: &Path,
    environment: &Environment,
) -> Result<Vec<String>, StoreError> {
    let serving = store
        .list_deployments(environment.id)
        .await?
        .into_iter()
        .find(|x| x.result == Some(DeploymentResult::Succeeded))
        .and_then(|x| x.release_id);
    let Some(release) = serving else {
        return Ok(Vec::new());
    };
    let services = store.list_services(environment.id).await?;
    let applied: Option<BTreeMap<String, String>> =
        fs::read(release_dir(state_dir, release).join(STAMPS_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| v["services"].as_object().cloned())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_owned())))
                    .collect()
            });
    let empty = stamp(&BTreeMap::new());
    let layers = layers(store, environment, &services).await?;
    let mut out = Vec::new();
    for s in &services {
        let was = match &applied {
            Some(map) => match map.get(&s.name) {
                Some(v) => v.clone(),
                // Not run by the serving release.
                None => continue,
            },
            None => empty.clone(),
        };
        if stamp(&layers.resolve(&s.name)) != was {
            out.push(s.name.clone());
        }
    }
    Ok(out)
}

/// Removes the env files of `environment`'s releases other than `keep`:
/// a release that will not be brought up again needs no values on disk.
/// `attempted` is the release the deployment just ended prepared, which no
/// deployment names if it failed before the release was recorded.
/// Problems are logged; nothing fails.
pub async fn prune(
    d: &Deployer,
    environment: EnvironmentId,
    attempted: Option<ReleaseId>,
    keep: &[ReleaseId],
) {
    let deployments = match d.store().list_deployments(environment).await {
        Ok(list) => list,
        Err(e) => {
            tracing::error!(error = %e, "listing deployments to prune env files failed");
            Vec::new()
        }
    };
    let releases = deployments.iter().filter_map(|x| x.release_id);
    for release in attempted.into_iter().chain(releases) {
        if keep.contains(&release) {
            continue;
        }
        let dir = release_dir(d.state_dir(), release).join(ENV_DIR);
        match fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                tracing::error!(release = %release, error = %e, "an old release's env files were not removed");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ship_core::{ProjectId, Timestamp};

    fn secret(r: &str) -> Entry {
        Entry::Secret(SecretRecord {
            target: ConfigTarget::Project(ProjectId::from_parts(1_767_225_600_000, 1).unwrap()),
            name: "S".to_owned(),
            backend_ref: r.to_owned(),
            updated_at: Timestamp::from_unix_millis(1_767_225_600_000).unwrap(),
        })
    }

    #[test]
    fn stamps_follow_names_values_and_references() {
        let mut a = BTreeMap::new();
        a.insert("A".to_owned(), Entry::Var("1".to_owned()));
        a.insert("S".to_owned(), secret("aa"));
        let mut b = a.clone();
        assert_eq!(stamp(&a), stamp(&b));
        b.insert("S".to_owned(), secret("bb"));
        assert_ne!(stamp(&a), stamp(&b));
        let mut c = a.clone();
        c.insert("A".to_owned(), Entry::Var("2".to_owned()));
        assert_ne!(stamp(&a), stamp(&c));
        let mut d = a.clone();
        d.remove("A");
        assert_ne!(stamp(&a), stamp(&d));
        assert!(stamp(&BTreeMap::new()).starts_with("sha256:"));
        // The fingerprint names a secret, never its reference.
        let part = fingerprint_part(&a).to_string();
        assert!(!part.contains("aa"), "{part}");
        assert!(part.contains("\"S\""), "{part}");
    }

    #[test]
    fn queries() {
        assert_eq!(parse_query(None).unwrap(), (None, None));
        assert_eq!(
            parse_query(Some("env=development&service=web")).unwrap(),
            (Some("development".to_owned()), Some("web".to_owned()))
        );
        assert!(parse_query(Some("env=a&env=b")).is_err());
        assert!(parse_query(Some("other=1")).is_err());
        assert!(parse_query(Some("env")).is_err());
    }
}
