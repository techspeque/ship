//! Desired state, observed service status and operation phase
//! (ADR-0006, ADR-0011).
//!
//! The three are separate facts and are never derived from one another.

use std::fmt;

/// A requested change of state that the state machine does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError<S> {
    pub from: S,
    pub to: S,
}

impl<S: fmt::Display> fmt::Display for TransitionError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal transition from `{}` to `{}`",
            self.from, self.to
        )
    }
}

impl<S: fmt::Display + fmt::Debug> std::error::Error for TransitionError<S> {}

string_enum! {
    /// What the operator asked for (ADR-0006 rule 1). Set only by an operator
    /// action; a failed deployment never changes it.
    pub enum DesiredState {
        Running => "running",
        Stopped => "stopped",
        Removed => "removed",
    }
}

impl DesiredState {
    /// Whether `self → to` is legal: `running ↔ stopped`, either to
    /// `removed`. `removed` is final. A same-state request is not a
    /// transition; callers treat it as a no-op before asking.
    pub fn can_transition_to(self, to: DesiredState) -> bool {
        use DesiredState::*;
        matches!(
            (self, to),
            (Running, Stopped) | (Stopped, Running) | (Running, Removed) | (Stopped, Removed)
        )
    }

    /// Returns `to` if the transition is legal.
    pub fn transition(self, to: DesiredState) -> Result<DesiredState, TransitionError<Self>> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(TransitionError { from: self, to })
        }
    }
}

string_enum! {
    /// What Docker reports for a service now (ADR-0006 rule 2).
    ///
    /// Observed, not commanded: Docker is the authority, so any status may
    /// follow any other. What is constrained is the evidence each status
    /// needs; see [`ServiceStatus::from_observation`].
    pub enum ServiceStatus {
        Healthy => "healthy",
        RunningUnverified => "running_unverified",
        Starting => "starting",
        Unhealthy => "unhealthy",
        Failed => "failed",
        Stopped => "stopped",
        Unknown => "unknown",
    }
}

/// A container's state as Docker reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    /// No labelled container exists for the service.
    Absent,
    /// Created but never started.
    Created,
    Running,
    /// Docker is restarting it after an exit.
    Restarting,
    Paused,
    Exited {
        exit_code: i64,
    },
    Dead,
}

/// The result of the service's health check, if it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthCheck {
    /// No health check is configured.
    None,
    /// Configured, no verdict yet.
    Starting,
    Passing,
    Failing,
}

/// One observation of a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// Docker could not be asked. Nothing is known (ADR-0006 rule 4).
    DockerUnreachable,
    Reachable {
        container: ContainerState,
        health: HealthCheck,
    },
}

impl ServiceStatus {
    /// Derives a status from evidence alone.
    ///
    /// - Docker unreachable → `unknown`, whatever was seen before.
    /// - Running with a passing check → `healthy`; this is the only route to
    ///   `healthy`. Running with no check → `running_unverified`; with a
    ///   check still pending → `starting`; with a failing check → `unhealthy`.
    /// - Restarting, dead, or exited non-zero → `failed`.
    /// - Absent, created, paused, or exited 0 → `stopped`.
    pub fn from_observation(observation: Observation) -> ServiceStatus {
        let (container, health) = match observation {
            Observation::DockerUnreachable => return ServiceStatus::Unknown,
            Observation::Reachable { container, health } => (container, health),
        };
        match container {
            ContainerState::Running => match health {
                HealthCheck::Passing => ServiceStatus::Healthy,
                HealthCheck::None => ServiceStatus::RunningUnverified,
                HealthCheck::Starting => ServiceStatus::Starting,
                HealthCheck::Failing => ServiceStatus::Unhealthy,
            },
            ContainerState::Restarting | ContainerState::Dead => ServiceStatus::Failed,
            ContainerState::Exited { exit_code } if exit_code != 0 => ServiceStatus::Failed,
            ContainerState::Exited { .. }
            | ContainerState::Absent
            | ContainerState::Created
            | ContainerState::Paused => ServiceStatus::Stopped,
        }
    }
}

string_enum! {
    /// Where an operation is (ADR-0011 rule 2). Non-terminal phases are
    /// ordered; an operation moves forward only, skipping phases that do not
    /// apply, and ends in exactly one terminal phase.
    pub enum OperationPhase {
        Queued => "queued",
        Uploading => "uploading",
        Validating => "validating",
        Building => "building",
        Preparing => "preparing",
        Deploying => "deploying",
        Verifying => "verifying",
        Succeeded => "succeeded",
        Failed => "failed",
        Interrupted => "interrupted",
    }
}

impl OperationPhase {
    /// `succeeded`, `failed` and `interrupted` are final.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OperationPhase::Succeeded | OperationPhase::Failed | OperationPhase::Interrupted
        )
    }

    /// Position among the ordered phases; `None` for terminal phases.
    fn rank(self) -> Option<u8> {
        use OperationPhase::*;
        match self {
            Queued => Some(0),
            Uploading => Some(1),
            Validating => Some(2),
            Building => Some(3),
            Preparing => Some(4),
            Deploying => Some(5),
            Verifying => Some(6),
            Succeeded | Failed | Interrupted => None,
        }
    }

    /// Whether `self → to` is legal: from a non-terminal phase, forward to a
    /// later non-terminal phase or to any terminal phase. Nothing leaves a
    /// terminal phase, nothing moves backwards, and a phase does not
    /// transition to itself.
    pub fn can_transition_to(self, to: OperationPhase) -> bool {
        let Some(from) = self.rank() else {
            return false;
        };
        match to.rank() {
            None => true,
            Some(to) => to > from,
        }
    }

    /// Returns `to` if the transition is legal.
    pub fn transition(self, to: OperationPhase) -> Result<OperationPhase, TransitionError<Self>> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(TransitionError { from: self, to })
        }
    }

    /// The coarse status recorded beside the phase.
    pub fn status(self) -> OperationStatus {
        match self {
            OperationPhase::Queued => OperationStatus::Queued,
            OperationPhase::Succeeded => OperationStatus::Succeeded,
            OperationPhase::Failed => OperationStatus::Failed,
            OperationPhase::Interrupted => OperationStatus::Interrupted,
            OperationPhase::Uploading
            | OperationPhase::Validating
            | OperationPhase::Building
            | OperationPhase::Preparing
            | OperationPhase::Deploying
            | OperationPhase::Verifying => OperationStatus::Running,
        }
    }
}

string_enum! {
    /// An operation's coarse status, derived from its phase.
    pub enum OperationStatus {
        Queued => "queued",
        Running => "running",
        Succeeded => "succeeded",
        Failed => "failed",
        Interrupted => "interrupted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_state_all_pairs() {
        use DesiredState::*;
        let legal = [
            (Running, Stopped),
            (Stopped, Running),
            (Running, Removed),
            (Stopped, Removed),
        ];
        for &from in DesiredState::ALL {
            for &to in DesiredState::ALL {
                let expected = legal.contains(&(from, to));
                assert_eq!(from.can_transition_to(to), expected, "{from} -> {to}");
                match from.transition(to) {
                    Ok(s) => {
                        assert!(expected, "{from} -> {to} accepted");
                        assert_eq!(s, to);
                    }
                    Err(e) => {
                        assert!(!expected, "{from} -> {to} rejected");
                        assert_eq!(e, TransitionError { from, to });
                    }
                }
            }
        }
    }

    #[test]
    fn operation_phase_all_pairs() {
        use OperationPhase::*;
        // Written independently of `rank` so the table checks the code.
        let order = [
            Queued, Uploading, Validating, Building, Preparing, Deploying, Verifying,
        ];
        let terminal = [Succeeded, Failed, Interrupted];
        assert_eq!(order.len() + terminal.len(), OperationPhase::ALL.len());

        let mut legal = Vec::new();
        for (i, &from) in order.iter().enumerate() {
            for &to in &order[i + 1..] {
                legal.push((from, to));
            }
            for &to in &terminal {
                legal.push((from, to));
            }
        }

        let mut rejected = 0;
        for &from in OperationPhase::ALL {
            for &to in OperationPhase::ALL {
                let expected = legal.contains(&(from, to));
                assert_eq!(from.can_transition_to(to), expected, "{from} -> {to}");
                assert_eq!(from.transition(to).is_ok(), expected, "{from} -> {to}");
                if !expected {
                    assert_eq!(from.transition(to), Err(TransitionError { from, to }));
                    rejected += 1;
                }
            }
        }
        // 100 pairs: 21 forward + 7 * 3 into a terminal phase = 42 legal.
        assert_eq!(legal.len(), 42);
        assert_eq!(rejected, 58);
    }

    #[test]
    fn operation_phase_spot_checks() {
        use OperationPhase::*;
        assert!(Queued.can_transition_to(Building));
        assert!(Queued.can_transition_to(Succeeded));
        assert!(Verifying.can_transition_to(Interrupted));
        assert!(!Verifying.can_transition_to(Building));
        assert!(!Building.can_transition_to(Building));
        assert!(!Succeeded.can_transition_to(Failed));
        assert!(!Failed.can_transition_to(Queued));
        assert!(!Interrupted.can_transition_to(Deploying));
    }

    #[test]
    fn operation_status_mapping() {
        use OperationPhase::*;
        assert_eq!(Queued.status(), OperationStatus::Queued);
        for p in [
            Uploading, Validating, Building, Preparing, Deploying, Verifying,
        ] {
            assert_eq!(p.status(), OperationStatus::Running);
            assert!(!p.is_terminal());
        }
        assert_eq!(Succeeded.status(), OperationStatus::Succeeded);
        assert_eq!(Failed.status(), OperationStatus::Failed);
        assert_eq!(Interrupted.status(), OperationStatus::Interrupted);
        assert!(Succeeded.is_terminal() && Failed.is_terminal() && Interrupted.is_terminal());
        assert!(!Queued.is_terminal());
    }

    #[test]
    fn service_status_derivation() {
        use ContainerState as C;
        use HealthCheck as H;
        use ServiceStatus as S;

        let containers = [
            C::Absent,
            C::Created,
            C::Running,
            C::Restarting,
            C::Paused,
            C::Exited { exit_code: 0 },
            C::Exited { exit_code: 1 },
            C::Exited { exit_code: -1 },
            C::Dead,
        ];
        let checks = [H::None, H::Starting, H::Passing, H::Failing];

        assert_eq!(
            S::from_observation(Observation::DockerUnreachable),
            S::Unknown
        );

        for &container in &containers {
            for &health in &checks {
                let got = S::from_observation(Observation::Reachable { container, health });
                let expected = match (container, health) {
                    (C::Running, H::Passing) => S::Healthy,
                    (C::Running, H::None) => S::RunningUnverified,
                    (C::Running, H::Starting) => S::Starting,
                    (C::Running, H::Failing) => S::Unhealthy,
                    (C::Restarting | C::Dead, _) => S::Failed,
                    (C::Exited { exit_code: 0 }, _) => S::Stopped,
                    (C::Exited { .. }, _) => S::Failed,
                    (C::Absent | C::Created | C::Paused, _) => S::Stopped,
                };
                assert_eq!(got, expected, "{container:?} {health:?}");
                // `healthy` needs a running container and a passing check.
                if got == S::Healthy {
                    assert_eq!((container, health), (C::Running, H::Passing));
                }
                // Reachable Docker never yields `unknown`.
                assert_ne!(got, S::Unknown);
            }
        }
    }

    #[test]
    fn string_forms_are_stable_and_match_serde() {
        fn check<T>(all: &[T], expected: &[&str])
        where
            T: Copy + fmt::Display + std::str::FromStr + PartialEq + fmt::Debug + serde::Serialize,
            <T as std::str::FromStr>::Err: fmt::Debug,
        {
            let got: Vec<String> = all.iter().map(|v| v.to_string()).collect();
            assert_eq!(got, expected);
            for v in all {
                let s = v.to_string();
                assert_eq!(s.parse::<T>().unwrap(), *v);
                assert_eq!(serde_json::to_string(v).unwrap(), format!("\"{s}\""));
            }
        }
        check(DesiredState::ALL, &["running", "stopped", "removed"]);
        check(
            ServiceStatus::ALL,
            &[
                "healthy",
                "running_unverified",
                "starting",
                "unhealthy",
                "failed",
                "stopped",
                "unknown",
            ],
        );
        check(
            OperationPhase::ALL,
            &[
                "queued",
                "uploading",
                "validating",
                "building",
                "preparing",
                "deploying",
                "verifying",
                "succeeded",
                "failed",
                "interrupted",
            ],
        );
        check(
            OperationStatus::ALL,
            &["queued", "running", "succeeded", "failed", "interrupted"],
        );
        assert!("Running".parse::<DesiredState>().is_err());
        assert!(serde_json::from_str::<DesiredState>("\"paused\"").is_err());
    }
}
