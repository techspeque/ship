---
type: adr
id: 0004
title: Ship stands alone; Gaia and Ship meet only at the ship CLI's JSON output
status: proposed
date: 2026-11-24
phase: 0
---

# ADR-0004: Ship stands alone; Gaia and Ship meet only at the ship CLI's JSON output

> Phase: 0 | Status: proposed | Date: 2026-11-24
> Decision drivers: the family order is Metis, Gaia, Ship, but Ship is built first because it is needed to host personal projects now.

## Context

In the finished picture an agent session running under Gaia builds
something, and Ship runs it. But Gaia does not exist yet and Ship is needed
today. Ship therefore cannot depend on anything Gaia provides: not its
tailnet, not its credential store, not its machine registry. It still has to
fit Gaia when Gaia arrives, without a rewrite.

Gaia and Metis already solved the same problem between themselves (Gaia
ADR-0001): two binaries that meet only at each other's CLI.

## Decision

Ship is its own repository and binary, complete without Gaia or Metis. The
seam for later is a small, versioned set of `ship` commands with `-o json`,
plus the network model in ADR-0003.

### Rules this decision implies:

1. Ship never imports, calls or detects Gaia or Metis at runtime.
2. The contract is `ship push --wait`, `ship projects`, `ship status`, `ship deployments` and `ship logs`, each with `-o json`. Anything else is not for machines to depend on.
3. `ship push --wait` exits 0 only when the deployment met its verification criteria, exits with a documented code otherwise, and uses a reserved range for Ship's own failures.
4. JSON shapes are provisional until Phase 5 and covered by fixture tests from Phase 1, so every change is deliberate. After v1 they change only with a major version.
5. Flags and conventions follow the family: `-o json`, `/api/v1`, one state directory, `install.sh` with checksum verification.
6. Ship keeps its own secret store behind a backend interface (ADR-0009). Gaia's store is not a dependency and not a backend in v1.
7. A Gaia session is an ordinary local client. It can use Ship exactly when the OS user running the Gaia node is in the `ship` group; Gaia does not sandbox sessions, so that membership is the only gate and it is the operator's to grant.

## Consequences

### Positive
- Ship is useful on day one with nothing else installed.
- Gaia can add Ship support later by shelling out, with no change to Ship.
- The contract is tested from the first release, not invented at integration time.

### Negative
- Two secret stores and two machine lists once both tools exist.
- Conventions are kept in step by hand across Go and Rust.
- Ship's contract is designed before its consumer exists and may need a revision.

### Neutral
- Ship setting conventions first means Gaia's spec may need small updates to match.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Deployment as a Gaia node capability | One daemon per machine | Blocks Ship on Gaia; breaks Gaia's non-goals | Ship is needed first |
| Ship uses Gaia for transport and secrets | Less duplicated | Hard dependency | Must stand alone |
| No seam now, integrate later | Less to decide | Contract invented under pressure | Cheap to fix now |

## References

- ADR-0002, ADR-0003, ADR-0009
- Gaia ADR-0001, ADR-0009, ADR-0013
