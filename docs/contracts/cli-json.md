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
| `ship projects -o json` | one object | `projects-empty.json`, `projects.json` | Phase 0; `environments` since Phase 1 |
| `ship status PROJECT -o json` | one object | `status.json`, `status-never-deployed.json` | Phase 1 |
| `ship deployments PROJECT -o json` | one object, newest first | `deployments.json` | Phase 1 |
| `ship logs PROJECT[/SERVICE] -o json` | one object per line | `logs.jsonl` | Phase 1 |
| `ship push --wait -o json` | one object per line, the last with `"type": "result"` | `push-wait-succeeded.jsonl`, `push-wait-failed.jsonl` | Phase 1 |

The four Phase 1 fixtures were written before the code, on 2026-11-27, to
fix the shapes Gaia's adapter is planned against. The Phase 1 slices make
the output match them or change them on purpose.

`projects.json` was added on 2026-11-28 (phase-1-ws-1.11): each project
gains `environments`, each with its `desired_state` and `status`, the
overall status of its services as Docker reports them now (the one most
in need of attention: `unknown`, `failed`, `unhealthy`, `starting`,
`paused`, `stopped`, `running_unverified`, `healthy`), `null` when it has
no service.

`ship status`'s `last_deployment` is the deployment serving now, the
latest that succeeded, so a later failure leaves it visible; before any
success it is the latest deployment, whatever its result. `ship
deployments` lists every deployment, failures included.

On 2026-11-28 (phase-1-ws-1.12) `status.json` and `deployments.json`
gained, on purpose:

- `action` on every deployment and on `last_deployment`: `deploy`, or
  `remove` for `ship remove`, which is recorded in the same history. A
  removal names the revision that was serving, has `release_id: null`,
  `previous_release_id` the release it took down, and result `succeeded`.
  After a removal `last_deployment` is the removal until
  `ship push --recreate` deploys again.
- `desired_state` on every service of `ship status`: `running`, or
  `stopped` after `ship stop PROJECT/SERVICE`. The environment's
  `desired_state` wins when it is `stopped` or `removed`; a service's
  `status` derives from the one that applies, so a service stopped on
  purpose is `stopped`, never `failed`.

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
| 3 | `confirmation_required` | The Compose file needs an operator's confirmation (ADR-0008 rule 2) and none was given: no terminal, no `--confirm`, and no `compose.confirm accept` setting on the host; or the environment was removed and the push has no `--recreate` (the message starts `the environment was removed` and names the volumes that would be reattached). Nothing is deployed | untouched |
| 90 | `daemon_unreachable` | No daemon answered on the target | untouched |
| 91 | `version_mismatch` | Client and daemon API versions are incompatible | untouched |
| 92 | `docker_unavailable` | The host's Docker Engine or Compose is unreachable or unsupported | untouched |
| 93 | `interrupted` | The daemon stopped during the operation; it is never resumed | untouched |
| 94 | `connection_lost` | The client lost the daemon while waiting; the operation continues and can be followed by its id | unknown |
| 99 | `internal` | Any other Ship failure | untouched |

Codes 90 to 99 are Ship's own failures. Code 1 is a failure of what was
pushed. A caller that retries should retry only on 90 and 94.

## Lifecycle commands

`ship start|stop|restart PROJECT[/SERVICE]` and `ship redeploy|remove
PROJECT`, each one operation followed to its end (phase-1-ws-1.12). Their
`-o json` is the operation as it ended (`OperationView`, the shape
`GET /api/v1/operations/{id}` answers); it is not a contract shape yet.
Exit codes: 0 done; 1 the action failed or was refused (a removed
environment, a stopped environment for one service, nothing deployed, an
unknown project or service); 2 usage, including `redeploy` or `remove`
with `/SERVICE`, refused before anything is sent because a release and a
Compose project cover the whole environment; 3 `remove` with no terminal
and no `--confirm`, nothing sent; 90, 92, 93 and 94 as for `ship push`.

- `stop` records the stop first, so nothing in Ship starts it again until
  `start`; a deployment (`push`, `redeploy`) asks for everything running
  and so starts a stopped environment or service.
- `restart` restarts the containers of the release serving now and skips
  services stopped on purpose; `redeploy` deploys the serving revision
  again with the options it was deployed with and the config stored now.
- `remove` runs `compose down`, never with volumes: containers and
  networks go, every named volume, the release files and the history
  stay. A removed environment refuses `start`, `stop`, `restart` and
  `redeploy`, and only `ship push --recreate` brings it back, as the same
  environment with its volumes reattached.
- Docker's restart policy decides what happens after a host reboot: the
  `unless-stopped` policy Ship gives a Dockerfile project keeps a stopped
  container stopped; a Compose file that asks for `restart: always` is
  started again by Docker itself at boot, and `ship status` then shows it
  running under desired `stopped`.
