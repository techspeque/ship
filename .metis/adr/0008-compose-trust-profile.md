---
type: adr
id: 0008
title: Compose files run under a trust profile that stops accidents and asks before anything host-reaching
status: proposed
date: 2026-11-24
phase: 1
---

# ADR-0008: Compose files run under a trust profile that stops accidents and asks before anything host-reaching

> Phase: 1 | Status: proposed | Date: 2026-11-24
> Decision drivers: real Compose files must work; the operator is trusted, their copy-pasted YAML is not.

## Context

The v0.4 draft rejected every host bind mount. But mounting a config file
from the project (`./nginx.conf:/etc/nginx/nginx.conf`) is in a large share
of real Compose files, and the operator is a single trusted person. Blanket
rejection protects no one and blocks the main use.

On the other side, `privileged: true`, the Docker socket and host
namespaces hand a container the machine. Those are rarely intended.

Environments add a second problem: a file publishing `8080:80` cannot run
twice on one host.

## Decision

Ship evaluates the canonical output of `docker compose config` against a
profile with three outcomes: **allow**, **confirm** (shown to the operator,
recorded in the audit log) and **reject**.

### Rules this decision implies:

1. Allow: named volumes, networks, health checks, `depends_on`, bind mounts whose source is inside the revision's snapshot (mounted read-only by default).
2. Confirm: bind mounts to other host paths, published ports, added capabilities, GPU reservations through a container runtime (`deploy.resources.reservations.devices`, `gpus`), `restart` policies that differ from Ship's default. The operator may set the host, or one project, to accept these without asking (`compose.confirm accept`); the setting is held by the daemon, cannot come from the pushed files, and every item accepted under it is recorded in the audit log.
3. Reject: `privileged`, host PID/IPC/network namespaces, raw device passthrough (`devices:`), mounts of the Docker socket. An audited override may come later; it is off by default.
4. Ship adds only labels, the environment's project name, managed config and secrets, and resource limits. It shows the resulting diff before the first deployment.
5. Ship never silently changes a port, volume, network or privilege the file declares.
6. `container_name` and fixed host ports make a file single-instance. That is fine in Phase 1, which has one environment per project. From Phase 2, creating a second environment from such a file requires the operator to remap ports or use routes (ADR-0010); Ship says which lines are the problem.
7. `docker compose config` output is treated as sensitive, since interpolation can expand credentials, and is never logged.
8. The profile is described as accident prevention. It is never described as isolation.

## Consequences

### Positive
- Ordinary Compose files deploy without edits.
- Dangerous directives are seen before they run.
- The environment and port conflict is explained instead of failing obscurely.

### Negative
- A confirmation step on first deploy of many files.
- The profile must track new Compose features.
- Does nothing against a malicious Dockerfile; build steps run regardless.

### Neutral
- The Compose file Ship generates for a Dockerfile source passes through the same profile, so there is one deploy path.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Reject all bind mounts (v0.4) | Simple rule | Breaks common files | Usability |
| Allow everything | No friction | One pasted line gives away the host | Accidents |
| Rewrite ports automatically | Environments just work | Silent change of declared behaviour | Rule 5 |

## References

- ADR-0001, ADR-0009, ADR-0010
