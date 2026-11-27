---
type: adr
id: 0006
title: Desired, deployed and observed state are separate; the reconciler observes first and repairs only when told
status: proposed
date: 2026-11-24
phase: 1
---

# ADR-0006: Desired, deployed and observed state are separate; the reconciler observes first and repairs only when told

> Phase: 1 | Status: proposed | Date: 2026-11-24
> Decision drivers: goal 2; tools in this space routinely report "deployed" for an app that is crash-looping.

## Context

Three different facts get collapsed into one status badge: what the operator
asked for, what the last deployment attempt achieved, and what Docker
reports this second. When they are one field, a successful `compose up`
reads as a healthy app, a manual `docker stop` reads as a failure, and an
unreachable Docker reads as everything being down.

A reconciler that acts on a collapsed state fights the operator and, at
worst, cleans up things it should not.

## Decision

Store the three separately and show disagreement as disagreement. SQLite is
the authority on intent; Docker is the authority on what is running. The
reconciler's default is to observe and report.

### Rules this decision implies:

1. Desired state is `running`, `stopped` or `removed`, set only by an operator action. A failed deployment does not change it.
2. Service status is one of `healthy`, `running_unverified`, `starting`, `unhealthy`, `failed`, `stopped`, `paused`, `unknown`. `healthy` requires a passing health check. An exited container in an environment whose desired state is `stopped` is `stopped` whatever its exit code; `paused` was added and this was stated on 2026-11-27 (Phase 1 plan).
3. A Compose subprocess exiting 0 proves nothing about health.
4. Docker unreachable sets `unknown` and blocks all corrective action.
5. Recovery policy is per environment: `observe_only` (default) or `repair_safe_drift` (Phase 3, opt-in).
6. An environment stopped through Ship is never restarted. Simple process restarts are left to Docker's restart policy.
7. Repair retries are capped with backoff and a cooldown; a crash loop cannot become a rebuild loop.
8. The reconciler takes the environment's mutation lock and never races a deployment.
9. It never deletes or recreates a volume, never touches unlabelled objects, and flags anything ambiguous for the operator.
10. `restart`, `redeploy`, `update` and `rollback` are distinct: only the last two change the pinned release.
11. On daemon start, in-flight operations are marked `interrupted` and state is rebuilt from labelled Docker objects.

## Consequences

### Positive
- The UI and CLI never claim more than Ship knows.
- Manual Docker use alongside Ship is safe and visible.
- The destructive failure modes are ruled out by construction.

### Negative
- More states to model, test and explain.
- With `observe_only`, a drifted app stays drifted until someone acts.

### Neutral
- Phase 1 needs only observation and restart recovery; the repair loop is later.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Single status field | Simple | Lies under every failure above | The point of the product |
| Always-repair reconciler | Self-healing | Fights the operator; risk to data | Safety |
| No reconciler | Least code | State goes stale after reboot | Needed for restart recovery |

## References

- ADR-0001, ADR-0007, ADR-0011
