---
type: adr
id: 0012
title: MIT licensed, self-hosted only, signed releases, no telemetry; scaffolding grows with the product
status: proposed
date: 2026-11-24
phase: 0
---

# ADR-0012: MIT licensed, self-hosted only, signed releases, no telemetry; scaffolding grows with the product

> Phase: 0 | Status: proposed | Date: 2026-11-24
> Decision drivers: Ship is public from the start and sits beside Metis and Gaia; the v0.4 draft front-loaded governance ahead of any working code.

## Context

Gaia is MIT with no hosted service and no telemetry (Gaia ADR-0012), and
Metis has an install script and release pipeline to copy. The v0.4 draft
asked for a code of conduct, SBOM, downgrade policy, compatibility matrix
and contributor acceptance test before a single deploy worked. Most of that
describes a product that does not exist yet.

## Decision

Ship is MIT licensed, self-hosted, with no service run by the maintainer
and no telemetry. Release hygiene that protects users is in from Phase 0.
Community scaffolding is added when there is something for a community to
use.

### Rules this decision implies:

1. Phase 0: LICENSE (MIT), README with honest status, SECURITY.md with a private reporting route, CONTRIBUTING.md with build and test commands, CI.
2. Releases from the first tag: checksums, signatures, an SBOM, and an `install.sh` that verifies them. No `curl | sudo sh` without verification.
3. No phone-home, update check, activation or account. Any future opt-in integration documents exactly what it sends.
4. The repository contains no real hostnames, addresses, names, credentials or screenshots with secrets. Examples use documentation domains.
5. Supported platforms are those built and tested in CI, listed in the README. Nothing else is claimed.
6. Docker and Compose version ranges are those CI validates.
7. Before v1.0 the CLI, API and storage formats may change; the README says so. Semantic versioning applies from v1.0.
8. Code of conduct, issue templates, changelog policy, operator runbook and the threat model document arrive by Phase 5, not before.
9. The name "Ship" is checked for conflicts before any public announcement; package and domain names are not fixed until then.

## Consequences

### Positive
- Same licence and release shape across the family.
- Users can verify what they install from the first release.
- Early effort goes to the product.

### Negative
- Early visitors find a thin repository.
- The name may change after code exists.

### Neutral
- Release tooling is copied from Metis, not shared.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Apache-2.0 | Patent grant | Differs from Gaia and Metis | Family consistency |
| Full governance in Phase 0 (v0.4) | Looks complete | Documentation for nothing | Order of work |
| Private until v1.0 | No early scrutiny | No early feedback | Public from the start |

## References

- ADR-0004
- Gaia ADR-0012
