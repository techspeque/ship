# phase-0-gate — Phase 0 gate: composed-system validation

- **Type:** gate | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27

## Phase being validated

Phase 0: Foundation. Run 1: v0.0.1 (tag v0.0.1, commit 30104f4b5e20).
Run 2: v0.0.2 (tag v0.0.2, commit 16d5d72b5846), carrying fix-0006.

## Composition scenarios

From `.metis/plans/phase-0-plan.md`, Phase Gate:

1. Install a release build with `install.sh`, `ship service install`, start; daemon up after a reboot
2. `ship target add lab ssh://user@host`, then `ship projects -o json` returns the empty list fixture
3. `ship open` shows Docker `ready`; stopping Docker shows `unreachable` in the UI and `ship host`; daemon keeps running
4. Listening sockets: Ship only on its unix socket and 127.0.0.1
5. Docker objects that existed before Ship are unchanged after the whole scenario
6. A client one API version behind fails with the upgrade instruction
7. `ship service uninstall` leaves the state directory and Docker objects
8. Every `-o json` output matches its fixture

Performance: idle RSS and CPU recorded in the README; `ship projects` over ssh under 2 s on a LAN, handshake reported separately.

## Evidence criteria

- [x] All phase slices coded and reviewed (ws-0.1..0.9, fix-0001..0006, feat-0001 archived)
- [ ] Cross-module integration tests pass — run 2: scenarios 2, 4, 6, 8 evidenced; 1, 3, 5, 7 wait for the Human's sudo steps
- [x] No interface mismatches at seam points — run 2: release client 0.0.2 against daemons 0.0.2 and 0.0.1 over ssh; JSON matches fixtures; version refusal both ways
- [x] Performance/resource usage acceptable — run 2: figures below, README updated

## Report

### Run 1 — 2026-11-27, v0.0.1 — verdict FAIL

Hosts: gate host A (no GPU) and gate host B (GPU), both Ubuntu 26.04 x86_64,
systemd 259, Docker Engine 29.10.1. Names are in `.local/hosts.md` only.
`metis verify --pre` on dev: ALL GREEN.

#### Scenario 1: release install, service install, reboot — FAIL

- **Setup:** login user, no root shell. `/usr/local/bin` not writable by the user.
- **Action (the Human, as documented):**
  `curl -fsSL .../v0.0.1/install.sh | sh -s -- --version v0.0.1`;
  then `sudo ship service install`, `sudo systemctl start ship`,
  `sudo usermod -aG ship USER`, reboot.
- **Expected:** daemon running after the reboot, `/run/ship/ship.sock` present.
- **Actual, both hosts:** install.sh chose `$HOME/.local/bin`
  (`scripts/install.sh:202-208`) and printed
  `to run the daemon under systemd: sudo $prefix/ship service install`
  (`scripts/install.sh:227`). The unit was written with
  `ExecStart=/home/USER/.local/bin/ship daemon --state /var/lib/ship --socket /run/ship/ship.sock`.
  The home directory is mode 0750, so the `ship` user cannot reach the
  binary. Journal: `Failed to execute /home/USER/.local/bin/ship: Permission denied`,
  `status=203/EXEC`; `Restart=always` loops (NRestarts 131 on A, 147 on B
  when checked). `/run/ship` does not exist. `ship service install`
  detected the condition (`crates/ship/src/service.rs:310`) but only
  warned, after every change was made.
- **Also:** on host B `ship` is not on the PATH of a non-interactive ssh
  session, so the laptop's ssh target fails with "`ship` was not found";
  on host A a pre-release `/usr/local/bin/ship` (2026-11-25) is found
  instead of v0.0.1.
- **Evidence:** `systemctl status ship`, `journalctl -u ship -n 5`,
  `systemctl show ship -p NRestarts`, `ls -la ~/.local/bin/ship /usr/local/bin/ship`,
  `ls -ld $HOME /run/ship` over ssh on both hosts, read-only, 2026-11-27 12:02 GMT.
- **Why release CI missed it:** no CI job composes the two steps.
  `release.yml` runs install.sh only with an explicit `--prefix` under
  `$RUNNER_TEMP` and never runs `service install`; the ci.yml `service` job
  (`scripts/service-smoke.sh:44`) always copies the binary to
  `/usr/local/bin` with sudo before `service install`; `install-selftest.sh`
  always passes `--prefix`. The default per-user prefix branch and the hint
  it prints were never exercised against systemd.

#### Scenarios 2–8 and performance — NOT EVIDENCED

Every remaining scenario needs a running daemon from the release build.
None is marked passed. They wait for fix-0006 and for the Human to repair
the hosts with sudo.

#### Re-run notes

- Repair (the Human, sudo): put the binary at `/usr/local/bin/ship`
  (`sudo install -m 0755 ~/.local/bin/ship /usr/local/bin/ship`), remove the
  per-user copy, replace the stale pre-release binary on host A, re-run
  `sudo /usr/local/bin/ship service install`, reboot. Re-run from scenario 1
  with the release that carries fix-0006, using the default documented path.
- Scenario 5 baseline already exists on gate host B: container and volume
  `ship-gate-sentinel`, with the before-state in `~/ship-gate/` on that host
  (`container-before.json`, `volume-before.json`, `volume-before.sha256`,
  `networks-before.json`, `listeners-before.txt`). Leave them in place and
  compare against them at the end of the re-run; `listeners-before.txt`
  is also the baseline for scenario 4.

### Findings

| Finding | Severity | Offending slice | Filed as |
|---|---|---|---|
| Non-root documented install yields a unit systemd cannot exec; install warns instead of refusing; install.sh recommends service install from a per-user prefix; no CI path covers it | P1 (slice priority p0) | phase-0-ws-0.8, phase-0-ws-0.9 (archived) | fix-0006 |

### Verdict

**FAIL.** Scenario 1 fails on v0.0.1 through the path the installer
recommends. fix-0006 is inserted before this gate; the gate is re-run after
it is reviewed and the hosts are repaired.

### Run 2 — 2026-11-27, v0.0.2 — verdict PENDING (sudo steps outstanding)

Same hosts. Gate host B is the clean host: its first Ship unit appears in
the journal at 11:56 GMT, after the sentinel objects (11:52). Gate host A
has run pre-release builds since 2026-11-25 and is a secondary, upgraded
host. Branch `feature/phase-1-push`; `metis verify --pre`: ALL GREEN.
Laptop client: the v0.0.2 release (darwin arm64, installed with
install.sh). Release archive `ship-v0.0.2-linux-amd64.tar.gz` checked
against `SHA256SUMS`; its `ship` is sha256 `110bf099…f26065d`, equal to
`/usr/local/bin/ship` (and the leftover `~/.local/bin/ship`) on both hosts.
Everything below was observed by the agent over ssh as the login user
(groups `docker`, `ship`), without sudo, 12:59–13:06 GMT, unless marked
"the Human's report". The ssh alias resolves to the hosts' tailnet
addresses; tailscale reports a direct path over the LAN, ping 5 ms.

#### Scenario 1: release install, service install, reboot — NOT YET EVIDENCED on v0.0.2

- **The Human's report:** on both hosts ran install.sh as the login user,
  then the two commands it printed
  (`sudo install -m 0755 ~/.local/bin/ship /usr/local/bin/ship`,
  `sudo /usr/local/bin/ship service install`), then rebooted host B.
- **Observed, host B:** unit `/etc/systemd/system/ship.service` with
  `ExecStart=/usr/local/bin/ship daemon --state /var/lib/ship --socket /run/ship/ship.sock`,
  `User=ship`; `is-enabled` enabled, `is-active` active, `NRestarts=0`;
  `/run/ship/ship.sock` `srw-rw---- ship:ship`; `ship host` reports
  version 0.0.2; `command -v ship` in a non-interactive ssh session is
  `/usr/local/bin/ship` (run 1's PATH problem is gone).
- **Observed, timeline on host B (GMT):** release published 12:30:47;
  boot 12:35:17 (`uptime -s`, `journalctl --list-boots`); daemon started
  at boot 12:35:26; `~/.local/bin/ship` written 12:45:31,
  `/usr/local/bin/ship` 12:45:55; daemon SIGTERMed and restarted 12:46:21
  (PID 5162, `ActiveEnterTimestamp`). The unit file dates from 12:17:18
  (run-1 repair) and was not rewritten: same content, so install reported
  it unchanged. **The reboot came before the v0.0.2 install, not after
  it.** What came up at 12:35 was the run-1-repaired build at the same path.
  The v0.0.2 daemon has never been started by a boot.
- **Observed, host A:** no reboot since 11:46:45. The daemon (PID 10032)
  has been running since 12:16:23. `/usr/local/bin/ship` was replaced at
  12:47:41 and nothing restarted it. `ship host` reports **version 0.0.1**,
  while `ship --version` on the same host prints 0.0.2. Recorded as
  advisory f-018 (P2): on an in-place upgrade the printed hints never say
  to restart. Host A is an upgrade, not the clean-host scenario, so this
  does not fail scenario 1.
- **Needs:** the Human reboots host B after the sudo script below; then
  the agent checks that the boot time is later than the install, the daemon
  started in that boot, and that `ship host` reports 0.0.2.

#### Scenario 2: target add, projects — PASS (both hosts)

With a fresh config (`SHIP_CONFIG` in a temp dir):
`ship target add lab ssh://USER@HOST` exit 0, then `ship projects -o json`
exit 0. The output is `{"projects": []}`, identical after `jq -S` to
`crates/ship/tests/fixtures/projects-empty.json`. Text: `No projects.`
Host A answered from its 0.0.1 daemon. API v1 on both sides.

#### Scenario 3: UI ready, Docker stopped shows unreachable — PART (ready only)

- `ship open --no-browser` against host B printed `http://127.0.0.1:N/`
  after `/healthz`; `GET /` 200 text/html; `GET /api/v1/host` through the
  forward returns docker `ready` with headers `ship-api-version: v1`,
  `ship-version: 0.0.2`.
- Headless Chrome (laptop) rendering that URL shows the text
  `✓ Docker ready Ship 0.0.2 (API v1) … Docker Engine 29.10.1 Docker Compose 5.8.0`.
- `ship host` (text and json): `ready` on both hosts.
- **Needs sudo:** stopping Docker. The script below covers it.

#### Scenario 4: listeners — PASS (both hosts)

`ss -ltnH` on host B against `~/ship-gate/listeners-before.txt`: one new
TCP listener, `127.0.0.1:7180`. Sockets owned by the `ship` uid in
`/proc/net/{tcp,tcp6,udp,udp6}`: only `0100007F:1C0C` (127.0.0.1:7180)
on both hosts. Unix listener `/run/ship/ship.sock`, mode 0660 `ship:ship`
in `/run/ship` 0750. The daemon does not run with `--tailnet`, so no
tailnet listener. Host A shows the same.

#### Scenario 5: pre-existing Docker objects — INTERIM PASS (final check after the sudo steps)

Host B, against `~/ship-gate/*-before.json`, after one reboot and with
Ship running:
- Volume `ship-gate-sentinel`: `docker volume inspect` identical;
  `sha256sum /data/seed` (busybox:1.37, read-only mount) identical.
- Container: differs only in `State.Pid`, `StartedAt`, `FinishedAt`,
  `NetworkSettings.SandboxID`/`SandboxKey`, the default bridge's
  `EndpointID`/`MacAddress`/`NetworkID`, and `HostConfig.Dns` `null` → `[]`.
  `Dns` is not in the plan's list of excluded fields. It changed across
  the 12:35 reboot, which reloads the container from disk.
  `docker events` since that boot show only the sentinel's start at boot
  and the agent's own busybox check. No event comes from Ship. Read as a
  Docker restart rewrite; the Reviewer should confirm that reading.
- Networks `bridge`/`host`/`none`: only the default bridge's `Id`,
  `Created` and the sentinel's endpoint changed (excluded by the plan).
- **Needs:** the same comparison after the Docker restart, uninstall,
  reinstall and reboot.

#### Scenario 6: client one API version behind — PASS (with a stand-in)

No Ship release speaks any API version but v1, so no real older-API
client exists.
- Real daemon, host B: `GET /api/v1/projects` on the socket with
  `Ship-Api-Version: v0` → `426`, `{"code":"unsupported_api_version",
  "message":"this daemon speaks API v1, the client speaks v0",
  "details":{"client_api_version":"v0","daemon_api_version":"v1","daemon_version":"0.0.2"}}`.
- Real release client 0.0.2, over ssh, against a stand-in socket on host B
  that answers as an API v2 daemon (426, `ship-api-version: v2`,
  `ship-version: 0.1.0`; `~/ship-gate/stub-api-v2.py`, target
  `--socket /tmp/…`): `ship projects -o json` and `ship host` both exit 1
  with `this client (ship 0.0.2) speaks API v1, but the daemon speaks v2.
  Upgrade this client to ship 0.1.0 or later; `ship --version` on the
  daemon's host shows its release.`

#### Scenario 7: service uninstall leaves state and Docker objects — NEEDS SUDO

Covered by the script below. Not marked passed.

#### Scenario 8: JSON matches fixtures — PASS (Phase 0 commands)

`ship projects -o json`: identical to `projects-empty.json` (scenario 2).
`ship host -o json` on both hosts: the same key paths and JSON types as
`host.json` (jq `paths` × `type`, no diff). Values differ as expected.
These are the two Phase 0 commands in `docs/contracts/cli-json.md`. The
`unreachable` shape (`engine_version`/`compose_version` null, `message` a
string) is captured by the sudo script and checked afterwards.

#### Performance — PASS

- Idle, host B, v0.0.2 under systemd, 16 min after start, no projects:
  VmRSS 6448 kB (VmHWM 6508 kB), 18 threads, 0 CPU ticks over 60 s,
  0.02 s CPU since start (`/proc/PID/stat`). The unit's cgroup reports
  `CPUUsageNSec` 0.47 s and `MemoryCurrent` 59 MB. Both cover the whole
  cgroup, including Docker CLI probe children and page cache. Recorded in
  README "Footprint". It replaces the 2026-11-25 pre-release figure.
- `ship projects -o json` over ssh, 5 runs to host B: 0.11–0.14 s, each
  run a new ssh connection (no ControlMaster). The handshake is reported
  separately: `ssh HOST true` 0.12 s every run. Ship's own share is
  ≤ 0.02 s, well under 2 s. Host A: 0.11–0.12 s.

#### Sudo steps for the Human (one trip per host)

Gate host B: `sudo sh ~/ship-gate/run2-sudo.sh` (sha256 `aef193c7…bcf20f`,
about 2 minutes), then `sudo reboot`. The script stops Docker (socket and
service) for 60 s and records `systemctl show ship` and `ship host -o json`
(as the login user) every 5 s. It starts Docker again, then records the
sentinel's inspect output before and after `ship service uninstall`, the
state directory listing and hashes, and the unit, user and group. It
reinstalls with `ship service install` and runs `systemctl start ship`.
Evidence goes to `~/ship-gate/run2/`. It ends with Docker and Ship running.
Optional: start the laptop observer (`.local/gate-run2-observe.sh`,
gitignored) first to capture the UI over ssh while Docker is down.
Gate host A: `sudo systemctl restart ship`, so its daemon runs 0.0.2.

#### Run 2 status

Not flipped. Scenarios 1, 3, 5 and 7 wait for the steps above.
