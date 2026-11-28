//! Ship's Docker labels: how Ship recognises its own objects after a
//! restart (OVERVIEW §6, ADR-0001 rule 5).
//!
//! Every object Ship creates carries the four labels, with immutable ids as
//! values. Every object Ship selects is selected by them.

use std::collections::BTreeMap;
use std::ffi::OsString;

use ship_core::{EnvironmentId, ProjectId, ReleaseId, ServiceId};

pub const PROJECT_LABEL: &str = "dev.ship.project";
pub const ENVIRONMENT_LABEL: &str = "dev.ship.environment";
pub const SERVICE_LABEL: &str = "dev.ship.service";
pub const RELEASE_LABEL: &str = "dev.ship.release";

/// The label Compose puts on everything in a project.
pub const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
/// The label Compose puts on a service's containers.
pub const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

/// The four labels of one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShipLabels {
    pub project: ProjectId,
    pub environment: EnvironmentId,
    /// The service a container or image belongs to. `None` for an object
    /// the environment's services share (a network or a volume); its label
    /// value is then empty.
    pub service: Option<ServiceId>,
    pub release: ReleaseId,
}

impl ShipLabels {
    /// The four `(key, value)` pairs.
    pub fn pairs(&self) -> [(&'static str, String); 4] {
        [
            (PROJECT_LABEL, self.project.to_string()),
            (ENVIRONMENT_LABEL, self.environment.to_string()),
            (
                SERVICE_LABEL,
                self.service.map(|s| s.to_string()).unwrap_or_default(),
            ),
            (RELEASE_LABEL, self.release.to_string()),
        ]
    }

    /// The four labels as a JSON object, for a Compose file.
    pub(crate) fn to_json(self) -> serde_json::Value {
        serde_json::Value::Object(
            self.pairs()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), serde_json::Value::String(v)))
                .collect(),
        )
    }

    /// Reads the four labels from an object's label map. `None` when any is
    /// missing or not a valid id; an empty service label reads as `None`.
    pub fn parse(labels: &BTreeMap<String, String>) -> Option<ShipLabels> {
        let project = labels.get(PROJECT_LABEL)?.parse().ok()?;
        let environment = labels.get(ENVIRONMENT_LABEL)?.parse().ok()?;
        let release = labels.get(RELEASE_LABEL)?.parse().ok()?;
        let service = match labels.get(SERVICE_LABEL)?.as_str() {
            "" => None,
            s => Some(s.parse().ok()?),
        };
        Some(ShipLabels {
            project,
            environment,
            service,
            release,
        })
    }
}

/// Which of Ship's objects a query is about: always one environment,
/// optionally one service of it. There is no selector without Ship's labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selector {
    environment: EnvironmentId,
    service: Option<ServiceId>,
}

impl Selector {
    /// Every object of `environment`.
    pub fn environment(environment: EnvironmentId) -> Self {
        Selector {
            environment,
            service: None,
        }
    }

    /// Narrows to one service.
    pub fn service(mut self, service: ServiceId) -> Self {
        self.service = Some(service);
        self
    }

    pub fn environment_id(&self) -> EnvironmentId {
        self.environment
    }

    pub fn service_id(&self) -> Option<ServiceId> {
        self.service
    }

    /// `--filter label=…` arguments for `docker ps`.
    pub(crate) fn filters(&self) -> Vec<OsString> {
        let mut out = vec![
            "--filter".into(),
            format!("label={ENVIRONMENT_LABEL}={}", self.environment).into(),
        ];
        if let Some(service) = self.service {
            out.push("--filter".into());
            out.push(format!("label={SERVICE_LABEL}={service}").into());
        }
        out
    }

    /// Whether an object with `labels` is one this selector means.
    pub fn matches(&self, labels: &ShipLabels) -> bool {
        labels.environment == self.environment
            && self.service.is_none_or(|s| labels.service == Some(s))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn ids() -> (ProjectId, EnvironmentId, ServiceId, ReleaseId) {
        (
            ProjectId::from_parts(1_700_000_000_000, 1).unwrap(),
            EnvironmentId::from_parts(1_700_000_000_000, 2).unwrap(),
            ServiceId::from_parts(1_700_000_000_000, 3).unwrap(),
            ReleaseId::from_parts(1_700_000_000_000, 4).unwrap(),
        )
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn pairs_round_trip() {
        let (p, e, s, r) = ids();
        for service in [Some(s), None] {
            let labels = ShipLabels {
                project: p,
                environment: e,
                service,
                release: r,
            };
            let parsed: BTreeMap<String, String> = labels
                .pairs()
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect();
            assert_eq!(parsed.len(), 4);
            assert_eq!(ShipLabels::parse(&parsed), Some(labels));
        }
    }

    #[test]
    fn missing_or_malformed_labels_are_not_ours() {
        let (p, e, s, r) = ids();
        let (p, e, s, r) = (p.to_string(), e.to_string(), s.to_string(), r.to_string());
        let full = [
            (PROJECT_LABEL, p.as_str()),
            (ENVIRONMENT_LABEL, e.as_str()),
            (SERVICE_LABEL, s.as_str()),
            (RELEASE_LABEL, r.as_str()),
        ];
        assert!(ShipLabels::parse(&map(&full)).is_some());
        for skip in 0..4 {
            let partial: Vec<_> = full
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, kv)| *kv)
                .collect();
            assert_eq!(ShipLabels::parse(&map(&partial)), None, "without {skip}");
        }
        let mut bad = full;
        bad[1] = (ENVIRONMENT_LABEL, "env_nope");
        assert_eq!(ShipLabels::parse(&map(&bad)), None);
        // An environment id in the project label is not a project id.
        let mut swapped = full;
        swapped[0] = (PROJECT_LABEL, e.as_str());
        assert_eq!(ShipLabels::parse(&map(&swapped)), None);
        assert_eq!(ShipLabels::parse(&BTreeMap::new()), None);
    }

    #[test]
    fn selector_filters_and_matches() {
        let (p, e, s, r) = ids();
        let sel = Selector::environment(e);
        assert_eq!(
            sel.filters(),
            vec![
                OsString::from("--filter"),
                OsString::from(format!("label=dev.ship.environment={e}"))
            ]
        );
        let narrow = sel.service(s);
        assert_eq!(narrow.filters().len(), 4);
        assert_eq!(
            narrow.filters()[3],
            OsString::from(format!("label=dev.ship.service={s}"))
        );

        let ours = ShipLabels {
            project: p,
            environment: e,
            service: Some(s),
            release: r,
        };
        assert!(sel.matches(&ours));
        assert!(narrow.matches(&ours));
        let other_env = ShipLabels {
            environment: EnvironmentId::from_parts(1, 9).unwrap(),
            ..ours
        };
        assert!(!sel.matches(&other_env));
        let shared = ShipLabels {
            service: None,
            ..ours
        };
        assert!(sel.matches(&shared));
        assert!(!narrow.matches(&shared));
    }
}
