//! Desired state, observed service status and operation phase
//! (ADR-0006, ADR-0011).
//!
//! The three are separate facts and are stored apart. Service status reads
//! desired state only to tell a requested stop from a crash (ADR-0006
//! rule 2); it never changes it.

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

    /// What is asked of one service: the environment's desired state when
    /// it is `stopped` or `removed`, which wins, else the service's own
    /// (`ship stop P/S` records `stopped` for one service).
    pub fn effective(environment: DesiredState, service: DesiredState) -> DesiredState {
        match environment {
            DesiredState::Running => service,
            halted => halted,
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
        Paused => "paused",
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
        /// Docker's `State.OOMKilled`: the kernel killed the container's
        /// process for exceeding its memory limit.
        oom_killed: bool,
    },
}

impl ServiceStatus {
    /// Derives a status from the observation and the environment's desired
    /// state (ADR-0006 rule 2).
    ///
    /// - Docker unreachable → `unknown`, whatever was seen before and
    ///   whatever was asked for.
    /// - Running with a passing check → `healthy`; this is the only route to
    ///   `healthy`. Running with no check → `running_unverified`; with a
    ///   check still pending → `starting`; with a failing check → `unhealthy`.
    /// - Paused → `paused`.
    /// - Exited under desired `stopped` or `removed` → `stopped`, whatever
    ///   the exit code: stopping a container ends it with SIGTERM or SIGKILL
    ///   (143, 137), which is not a failure.
    /// - Exited non-zero under desired `running`, restarting, or dead →
    ///   `failed`.
    /// - Absent, created, or exited 0 → `stopped`.
    pub fn from_observation(observation: Observation, desired: DesiredState) -> ServiceStatus {
        let (container, health) = match observation {
            Observation::DockerUnreachable => return ServiceStatus::Unknown,
            Observation::Reachable {
                container, health, ..
            } => (container, health),
        };
        let halted = matches!(desired, DesiredState::Stopped | DesiredState::Removed);
        match container {
            ContainerState::Running => match health {
                HealthCheck::Passing => ServiceStatus::Healthy,
                HealthCheck::None => ServiceStatus::RunningUnverified,
                HealthCheck::Starting => ServiceStatus::Starting,
                HealthCheck::Failing => ServiceStatus::Unhealthy,
            },
            ContainerState::Paused => ServiceStatus::Paused,
            ContainerState::Restarting | ContainerState::Dead => ServiceStatus::Failed,
            ContainerState::Exited { exit_code } if exit_code != 0 && !halted => {
                ServiceStatus::Failed
            }
            ContainerState::Exited { .. } | ContainerState::Absent | ContainerState::Created => {
                ServiceStatus::Stopped
            }
        }
    }

    /// How much a status needs the operator's attention; `0` is the most.
    /// Order: `unknown`, `failed`, `unhealthy`, `starting`, `paused`,
    /// `stopped`, `running_unverified`, `healthy`.
    pub fn severity(self) -> u8 {
        match self {
            ServiceStatus::Unknown => 0,
            ServiceStatus::Failed => 1,
            ServiceStatus::Unhealthy => 2,
            ServiceStatus::Starting => 3,
            ServiceStatus::Paused => 4,
            ServiceStatus::Stopped => 5,
            ServiceStatus::RunningUnverified => 6,
            ServiceStatus::Healthy => 7,
        }
    }

    /// The overall status of several (services of an environment, or
    /// containers of a service): the one of least [`severity`](Self::severity),
    /// so it is `healthy` only when every one is. `None` for none.
    pub fn overall(statuses: impl IntoIterator<Item = ServiceStatus>) -> Option<ServiceStatus> {
        statuses.into_iter().min_by_key(|s| s.severity())
    }
}

/// What Docker reported for a service that is `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// Exited non-zero while desired `running`.
    Exited,
    /// Docker is restarting it after an exit.
    Restarting,
    /// Docker could not stop or remove it.
    Dead,
}

/// Why a service is `failed`, as far as the observation says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureDetail {
    pub reason: FailureReason,
    /// The exit code, when the container exited.
    pub exit_code: Option<i64>,
    /// The kernel killed it for exceeding its memory limit.
    pub oom_killed: bool,
}

impl FailureDetail {
    /// The detail for an observation; present exactly when
    /// [`ServiceStatus::from_observation`] gives `failed`.
    pub fn from_observation(
        observation: Observation,
        desired: DesiredState,
    ) -> Option<FailureDetail> {
        if ServiceStatus::from_observation(observation, desired) != ServiceStatus::Failed {
            return None;
        }
        let Observation::Reachable {
            container,
            oom_killed,
            ..
        } = observation
        else {
            return None;
        };
        let (reason, exit_code) = match container {
            ContainerState::Exited { exit_code } => (FailureReason::Exited, Some(exit_code)),
            ContainerState::Restarting => (FailureReason::Restarting, None),
            ContainerState::Dead => (FailureReason::Dead, None),
            // Never `failed`; excluded by the status check above.
            ContainerState::Absent
            | ContainerState::Created
            | ContainerState::Running
            | ContainerState::Paused => return None,
        };
        Some(FailureDetail {
            reason,
            exit_code,
            oom_killed,
        })
    }
}

impl fmt::Display for FailureDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.reason, self.exit_code) {
            (FailureReason::Exited, Some(code)) => write!(f, "exited with code {code}")?,
            (FailureReason::Exited, None) => f.write_str("exited")?,
            (FailureReason::Restarting, _) => f.write_str("restarting after an exit")?,
            (FailureReason::Dead, _) => f.write_str("dead")?,
        }
        if self.oom_killed {
            f.write_str("; killed for exceeding its memory limit (OOM)")?;
        }
        Ok(())
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
    fn the_environment_wins_unless_it_is_running() {
        use DesiredState::*;
        for &service in DesiredState::ALL {
            assert_eq!(DesiredState::effective(Running, service), service);
            assert_eq!(DesiredState::effective(Stopped, service), Stopped);
            assert_eq!(DesiredState::effective(Removed, service), Removed);
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

    const CONTAINERS: [ContainerState; 11] = [
        ContainerState::Absent,
        ContainerState::Created,
        ContainerState::Running,
        ContainerState::Restarting,
        ContainerState::Paused,
        ContainerState::Exited { exit_code: 0 },
        ContainerState::Exited { exit_code: 1 },
        ContainerState::Exited { exit_code: 137 },
        ContainerState::Exited { exit_code: 143 },
        ContainerState::Exited { exit_code: -1 },
        ContainerState::Dead,
    ];
    const CHECKS: [HealthCheck; 4] = [
        HealthCheck::None,
        HealthCheck::Starting,
        HealthCheck::Passing,
        HealthCheck::Failing,
    ];

    fn reachable(container: ContainerState, health: HealthCheck, oom_killed: bool) -> Observation {
        Observation::Reachable {
            container,
            health,
            oom_killed,
        }
    }

    /// Every reachable observation in the tables above, plus Docker
    /// unreachable.
    fn all_observations() -> Vec<Observation> {
        let mut out = vec![Observation::DockerUnreachable];
        for &container in &CONTAINERS {
            for &health in &CHECKS {
                for oom in [false, true] {
                    out.push(reachable(container, health, oom));
                }
            }
        }
        out
    }

    #[test]
    fn service_status_derivation() {
        use ContainerState as C;
        use DesiredState as D;
        use HealthCheck as H;
        use ServiceStatus as S;

        for &desired in DesiredState::ALL {
            for obs in all_observations() {
                let got = S::from_observation(obs, desired);
                let Observation::Reachable {
                    container, health, ..
                } = obs
                else {
                    assert_eq!(got, S::Unknown, "{desired}");
                    continue;
                };
                let halted = matches!(desired, D::Stopped | D::Removed);
                // Written independently of the code so the table checks it.
                let expected = match (container, health) {
                    (C::Running, H::Passing) => S::Healthy,
                    (C::Running, H::None) => S::RunningUnverified,
                    (C::Running, H::Starting) => S::Starting,
                    (C::Running, H::Failing) => S::Unhealthy,
                    (C::Paused, _) => S::Paused,
                    (C::Restarting | C::Dead, _) => S::Failed,
                    (C::Exited { exit_code: 0 }, _) => S::Stopped,
                    (C::Exited { .. }, _) if halted => S::Stopped,
                    (C::Exited { .. }, _) => S::Failed,
                    (C::Absent | C::Created, _) => S::Stopped,
                };
                assert_eq!(got, expected, "{obs:?} {desired}");
                // `healthy` needs a running container and a passing check.
                if got == S::Healthy {
                    assert_eq!((container, health), (C::Running, H::Passing));
                }
                // Reachable Docker never yields `unknown`.
                assert_ne!(got, S::Unknown, "{obs:?} {desired}");
            }
        }
    }

    #[test]
    fn severity_orders_every_status_once() {
        use ServiceStatus as S;
        let order = [
            S::Unknown,
            S::Failed,
            S::Unhealthy,
            S::Starting,
            S::Paused,
            S::Stopped,
            S::RunningUnverified,
            S::Healthy,
        ];
        assert_eq!(order.len(), S::ALL.len());
        for (i, s) in order.iter().enumerate() {
            assert_eq!(usize::from(s.severity()), i, "{s}");
        }
    }

    #[test]
    fn overall_is_the_worst() {
        use ServiceStatus as S;
        assert_eq!(S::overall([]), None);
        assert_eq!(S::overall([S::Healthy, S::Healthy]), Some(S::Healthy));
        assert_eq!(
            S::overall([S::Healthy, S::RunningUnverified]),
            Some(S::RunningUnverified)
        );
        assert_eq!(
            S::overall([S::Healthy, S::Stopped, S::Failed]),
            Some(S::Failed)
        );
        for &s in S::ALL {
            assert_eq!(S::overall([s, S::Unknown]), Some(S::Unknown), "{s}");
            assert_eq!(S::overall([S::Healthy, s]), Some(s), "{s}");
        }
    }

    #[test]
    fn docker_unreachable_is_unknown_under_every_desired_state() {
        for &desired in DesiredState::ALL {
            assert_eq!(
                ServiceStatus::from_observation(Observation::DockerUnreachable, desired),
                ServiceStatus::Unknown,
                "{desired}"
            );
        }
    }

    #[test]
    fn stop_signals_are_stopped_only_when_stop_was_asked() {
        use ServiceStatus as S;
        for code in [137, 143] {
            for oom in [false, true] {
                let obs = reachable(
                    ContainerState::Exited { exit_code: code },
                    HealthCheck::None,
                    oom,
                );
                assert_eq!(
                    S::from_observation(obs, DesiredState::Stopped),
                    S::Stopped,
                    "{code}"
                );
                assert_eq!(
                    S::from_observation(obs, DesiredState::Removed),
                    S::Stopped,
                    "{code}"
                );
                assert_eq!(
                    S::from_observation(obs, DesiredState::Running),
                    S::Failed,
                    "{code}"
                );
            }
        }
    }

    #[test]
    fn paused_is_never_stopped() {
        for &desired in DesiredState::ALL {
            for &health in &CHECKS {
                for oom in [false, true] {
                    let obs = reachable(ContainerState::Paused, health, oom);
                    assert_eq!(
                        ServiceStatus::from_observation(obs, desired),
                        ServiceStatus::Paused,
                        "{obs:?} {desired}"
                    );
                }
            }
        }
    }

    #[test]
    fn failure_detail_exactly_when_failed() {
        for &desired in DesiredState::ALL {
            for obs in all_observations() {
                let failed = ServiceStatus::from_observation(obs, desired) == ServiceStatus::Failed;
                let detail = FailureDetail::from_observation(obs, desired);
                assert_eq!(detail.is_some(), failed, "{obs:?} {desired}");
                if let (Some(d), Observation::Reachable { oom_killed, .. }) = (detail, obs) {
                    assert_eq!(d.oom_killed, oom_killed, "{obs:?} {desired}");
                }
            }
        }
    }

    #[test]
    fn failure_detail_text() {
        let detail = |container, oom| {
            FailureDetail::from_observation(
                reachable(container, HealthCheck::None, oom),
                DesiredState::Running,
            )
            .unwrap()
        };
        let d = detail(ContainerState::Exited { exit_code: 137 }, true);
        assert_eq!(
            d,
            FailureDetail {
                reason: FailureReason::Exited,
                exit_code: Some(137),
                oom_killed: true,
            }
        );
        assert_eq!(
            d.to_string(),
            "exited with code 137; killed for exceeding its memory limit (OOM)"
        );
        assert_eq!(
            detail(ContainerState::Exited { exit_code: 1 }, false).to_string(),
            "exited with code 1"
        );
        assert_eq!(
            detail(ContainerState::Restarting, true).to_string(),
            "restarting after an exit; killed for exceeding its memory limit (OOM)"
        );
        assert_eq!(detail(ContainerState::Dead, false).to_string(), "dead");
        // OOM-killed after the operator stopped it: stopped, so no failure.
        let obs = reachable(
            ContainerState::Exited { exit_code: 137 },
            HealthCheck::None,
            true,
        );
        assert_eq!(
            FailureDetail::from_observation(obs, DesiredState::Stopped),
            None
        );
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
                "paused",
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
