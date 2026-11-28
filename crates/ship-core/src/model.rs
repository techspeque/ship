//! Small domain enums from the data model (OVERVIEW §2.2) that are neither
//! ids nor state machines.

string_enum! {
    /// What a service is (OVERVIEW §2.2).
    pub enum ServiceKind {
        Web => "web",
        Worker => "worker",
        Job => "job",
        Compose => "compose",
    }
}

string_enum! {
    /// How the reconciler treats an environment (ADR-0006 rule 5).
    /// `observe_only` is the default; `repair_safe_drift` arrives in Phase 3.
    #[derive(Default)]
    pub enum RecoveryPolicy {
        #[default]
        ObserveOnly => "observe_only",
        RepairSafeDrift => "repair_safe_drift",
    }
}

string_enum! {
    /// Where a revision's source came from (ADR-0007). `snapshot` is the
    /// only kind in Phase 1; `git` and `image` arrive in Phase 4.
    pub enum RevisionKind {
        Snapshot => "snapshot",
        Git => "git",
        Image => "image",
    }
}

string_enum! {
    /// How a deployment ended (ADR-0007, ADR-0011). A deployment still in
    /// progress has no result.
    pub enum DeploymentResult {
        Succeeded => "succeeded",
        Failed => "failed",
        Interrupted => "interrupted",
    }
}

string_enum! {
    /// What a deployment did to its environment: applied a release
    /// (`deploy`), or removed its containers and networks, keeping its
    /// volumes and history (`remove`, phase-1-ws-1.12), which records no
    /// release.
    pub enum DeploymentAction {
        Deploy => "deploy",
        Remove => "remove",
    }
}

string_enum! {
    /// How a route reaches a service (ADR-0010). Phase 1 has `port` only.
    pub enum RouteProvider {
        Port => "port",
    }
}

string_enum! {
    /// Whether a route's address is known to work. Only a `confirmed`
    /// route's address is ever printed (ADR-0010 rule 2).
    pub enum RouteStatus {
        Pending => "pending",
        Confirmed => "confirmed",
        Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_strings_are_stable_and_round_trip() {
        let providers: Vec<&str> = RouteProvider::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(providers, ["port"]);
        let statuses: Vec<&str> = RouteStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(statuses, ["pending", "confirmed", "failed"]);
        for s in RouteStatus::ALL {
            assert_eq!(s.as_str().parse::<RouteStatus>(), Ok(*s));
        }
    }

    #[test]
    fn revision_kind_strings_are_stable_and_round_trip() {
        let strings: Vec<&str> = RevisionKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(strings, ["snapshot", "git", "image"]);
        for k in RevisionKind::ALL {
            assert_eq!(k.as_str().parse::<RevisionKind>(), Ok(*k));
        }
    }

    #[test]
    fn deployment_result_strings_are_stable_and_round_trip() {
        let strings: Vec<&str> = DeploymentResult::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(strings, ["succeeded", "failed", "interrupted"]);
        for r in DeploymentResult::ALL {
            assert_eq!(r.as_str().parse::<DeploymentResult>(), Ok(*r));
        }
    }

    #[test]
    fn service_kind_strings_are_stable_and_round_trip() {
        let strings: Vec<&str> = ServiceKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(strings, ["web", "worker", "job", "compose"]);
        for k in ServiceKind::ALL {
            assert_eq!(k.as_str().parse::<ServiceKind>(), Ok(*k));
        }
        assert!("Web".parse::<ServiceKind>().is_err());
    }

    #[test]
    fn recovery_policy_strings_are_stable_and_round_trip() {
        let strings: Vec<&str> = RecoveryPolicy::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(strings, ["observe_only", "repair_safe_drift"]);
        for p in RecoveryPolicy::ALL {
            assert_eq!(p.as_str().parse::<RecoveryPolicy>(), Ok(*p));
        }
        assert_eq!(RecoveryPolicy::default(), RecoveryPolicy::ObserveOnly);
    }
}
