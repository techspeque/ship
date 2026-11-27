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

#[cfg(test)]
mod tests {
    use super::*;

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
