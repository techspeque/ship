---
type: adr
id: 0011
title: Every mutation is a durable operation, serialised per environment and streamed with resumable events
status: proposed
date: 2026-11-24
phase: 1
---

# ADR-0011: Every mutation is a durable operation, serialised per environment and streamed with resumable events

> Phase: 1 | Status: proposed | Date: 2026-11-24
> Decision drivers: builds take minutes, connections drop, hosts reboot; the CLI and UI must show the same truth.

## Context

A deploy outlives the SSH session that started it. Two pushes to one
environment must not interleave. A retried request must not deploy twice.
After a reboot, Ship must know what was in flight. None of this needs a
message broker on a single host.

## Decision

Every state-changing request creates an operation row in SQLite before any
side effect. An in-process queue backed by that table runs operations. The
client gets an operation id and follows it.

### Rules this decision implies:

1. Mutating API calls carry an idempotency key and return `202` with the operation id. A replayed key returns the original operation.
2. Phases, in order, skipping those that do not apply: `queued`, `uploading`, `validating`, `building`, `preparing`, `deploying`, `verifying`, then `succeeded`, `failed` or `interrupted`.
3. One mutation lock per environment. Reads and log streaming are never blocked.
4. One build at a time across the host by default; the limit is configurable.
5. Events have a per-operation sequence number, are persisted, and stream over SSE. A client reconnects with its last sequence and misses nothing.
6. Build and deploy output is written to bounded on-disk logs through the redacting logger.
7. Failures are typed with stable codes and name the phase. Evidence is kept; the previous release is not touched.
8. On daemon start, operations left running are marked `interrupted`. They are never resumed automatically.
9. `--wait` exits non-zero on anything but `succeeded`. `--detach` returns the id at once.
10. Build timeout, queue length and a disk watermark are enforced before work starts.

## Consequences

### Positive
- CLI and UI follow the same record and cannot disagree.
- Dropped connections and retries are harmless.
- A reboot mid-build leaves a clear, inspectable state.

### Negative
- More machinery than calling Docker inline.
- Interrupted operations need a manual retry.

### Neutral
- The queue is in-process; it has no meaning beyond one daemon.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Synchronous request per deploy | Trivial | Dies with the connection | Long builds |
| Redis or NATS queue | Standard | A service to run | Non-goal |
| Auto-resume after restart | Convenient | Re-running half-applied steps is unsafe | Data safety |

## References

- ADR-0005, ADR-0006, ADR-0007
