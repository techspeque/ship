# CLI JSON contract

What scripts and Gaia may depend on (ADR-0004 rule 2). Status: **provisional
until Phase 5**; every change before then is a deliberate edit to a fixture
in `crates/ship/tests/fixtures/`. After v1.0 a shape changes only with a
major version.

Nothing outside this list is for machines: text output, the UI and the HTTP
API may change at any time.

## Commands and fixtures

| Command | Output | Fixture | Since |
|---|---|---|---|
| `ship host -o json` | one object | `host.json` | Phase 0 |
| `ship projects -o json` | one object | `projects-empty.json` | Phase 0 |
| `ship status PROJECT -o json` | one object | `status.json`, `status-never-deployed.json` | Phase 1 |
| `ship deployments PROJECT -o json` | one object, newest first | `deployments.json` | Phase 1 |
| `ship logs PROJECT[/SERVICE] -o json` | one object per line | `logs.jsonl` | Phase 1 |
| `ship push --wait -o json` | one object per line, the last with `"type": "result"` | `push-wait-succeeded.jsonl`, `push-wait-failed.jsonl` | Phase 1 |

The four Phase 1 fixtures were written before the code, on 2026-11-27, to
fix the shapes Gaia's adapter is planned against. The Phase 1 slices make
the output match them or change them on purpose.

## Rules for every shape

- Ids are strings with a type prefix. Timestamps are ISO 8601, UTC, with
  milliseconds.
- A field with no value is `null`. Fields are never left out.
- A reader must ignore fields it does not know and must not fail on an enum
  value it does not know.
- `status.json` keeps three things apart (ADR-0006): `environment.desired_state`
  is what was asked for, `last_deployment.result` is what the last deployment
  did, and `services[].status` is what Docker reports now.
- `endpoints` lists only addresses that exist. With no route it is `[]`.
- No secret value appears in any output.

## `ship push --wait`

Progress lines have `"type": "event"` and an increasing `sequence`. Exactly
one `"type": "result"` line follows, and it is the last line on stdout. With
`-o text` the same information is printed for people and the exit codes are
the same.

### Exit codes

| Code | Name | Meaning | Previous release |
|---|---|---|---|
| 0 | `succeeded` | Deployed and met its verification criteria | replaced |
| 1 | `failed` | The upload, build, deployment or verification failed; `failure` names the phase and a stable code | untouched |
| 2 | `usage` | Bad flags or arguments | untouched |
| 3 | `confirmation_required` | The Compose file needs an operator's confirmation (ADR-0008 rule 2) and none was given | untouched |
| 90 | `daemon_unreachable` | No daemon answered on the target | untouched |
| 91 | `version_mismatch` | Client and daemon API versions are incompatible | untouched |
| 92 | `docker_unavailable` | The host's Docker Engine or Compose is unreachable or unsupported | untouched |
| 93 | `interrupted` | The daemon stopped during the operation; it is never resumed | untouched |
| 94 | `connection_lost` | The client lost the daemon while waiting; the operation continues and can be followed by its id | unknown |
| 99 | `internal` | Any other Ship failure | untouched |

Codes 90 to 99 are Ship's own failures. Code 1 is a failure of what was
pushed. A caller that retries should retry only on 90 and 94.
