---
type: adr
id: 0007
title: Revision, release and deployment are separate immutable records; rollback is honest about what it restores
status: proposed
date: 2026-11-24
phase: 1
---

# ADR-0007: Revision, release and deployment are separate immutable records; rollback is honest about what it restores

> Phase: 1 | Status: proposed | Date: 2026-11-24
> Decision drivers: a deployment history that can answer "what exactly is running, and what was running before".

## Context

"Deploy" hides three things: the source that came in, the artifact built
from it, and the attempt to run that artifact somewhere. Rebuilding old
source is not the same as running the old image, because base images and
dependencies move. Ship has no registry, so old images exist only while the
host keeps them.

## Decision

Three records. A **revision** is captured source (or a resolved commit or
image digest). A **release** is the image ids built or pulled from a revision
plus the resolved non-secret runtime definition. A **deployment** is one
attempt to apply a release to an environment.

### Rules this decision implies:

1. Revisions and releases are never edited. A new push is a new revision even with identical content elsewhere.
2. Several deployments may reference one release. A retry is a new deployment, not a rewritten one.
3. A release records image ids or digests and a fingerprint of the effective configuration. No secret value is stored in either.
4. Phase 1 offers "redeploy an earlier revision", labelled as a rebuild whose result may differ.
5. Phase 3 offers true rollback: start a retained image with its stored definition, no build. Ship checks the image exists first and says so plainly when it was garbage-collected.
6. Rollback never touches volumes, databases or external systems, and warns when the environment has stateful services.
7. Retention keeps at least three revisions and a configurable number of releases. The active release is never collected. Volumes are never collected.
8. Every successful deployment links revision, release, environment, actor, times and health result.

## Consequences

### Positive
- History answers what ran when, and from what.
- Rollback claims are exact.
- Failed attempts leave earlier records intact.

### Negative
- More tables and ids than a single "deploys" list.
- Retained images cost disk; retention must be managed.

### Neutral
- Mutable image tags are resolved to digests at deploy time, so a tag moving later changes nothing running.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| One "deployment" record | Simple | Cannot tell rebuild from rollback | Honesty about rollback |
| Bundled registry for old images | Durable rollback | A service to run and secure | Non-goal |
| Git as the history | Familiar | Pushes need no commit | Goal 1 |

## References

- ADR-0005, ADR-0006
