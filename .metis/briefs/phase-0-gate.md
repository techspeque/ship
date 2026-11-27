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
- [x] Cross-module integration tests pass — run 2: all eight scenarios evidenced on v0.0.2 (3 and 6 partly through stand-ins, stated below)
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

### Run 2 — 2026-11-27, v0.0.2 — Coder's evidence complete; verdict is the Reviewer's

Same hosts. Gate host B is the clean host: its first Ship unit appears in
the journal at 11:56 GMT, after the sentinel objects (11:52). Gate host A
has run pre-release builds since 2026-11-25 and is a secondary, upgraded
host. Branch `feature/phase-1-push`; `metis verify --pre`: ALL GREEN.
Laptop client: the v0.0.2 release (darwin arm64, installed with
install.sh). Release archive `ship-v0.0.2-linux-amd64.tar.gz` checked
against `SHA256SUMS`; its `ship` is sha256 `110bf099…f26065d`, equal to
`/usr/local/bin/ship` (and the leftover `~/.local/bin/ship`) on both hosts.
Everything below was observed by the agent over ssh as the login user
(groups `docker`, `ship`), without sudo, 12:59–13:06 and 13:25–13:30 GMT, unless marked
"the Human's report". The ssh alias resolves to the hosts' tailnet
addresses; tailscale reports a direct path over the LAN, ping 5 ms.

#### Scenario 1: release install, service install, reboot — PASS (host B)

- **The Human's report:** on both hosts ran install.sh as the login user,
  then the two commands it printed
  (`sudo install -m 0755 ~/.local/bin/ship /usr/local/bin/ship`,
  `sudo /usr/local/bin/ship service install`). After the sudo script below
  (which uninstalled and reinstalled the service), rebooted host B.
- **Observed before the reboot (12:59 GMT):** the first reboot, at 12:35:17,
  came *before* the v0.0.2 install (`~/.local/bin/ship` 12:45:31,
  `/usr/local/bin/ship` 12:45:55; daemon restarted by hand 12:46:21). It
  did not count.
- **Observed after the final reboot, host B:** boot 13:23:44 GMT
  (`uptime -s`, `journalctl --list-boots`), later than the binary
  (12:45:55, sha256 `110bf099…f26065d`, equal to the release archive's)
  and the unit (rewritten by the reinstall at 13:17:29). Unit enabled;
  `ExecStart=/usr/local/bin/ship daemon --state /var/lib/ship --socket /run/ship/ship.sock`,
  `User=ship`. The daemon started in this boot at 13:23:52 (PID 3048,
  `NRestarts=0`, journal `daemon listening` 13:23:52). `/run/ship/ship.sock` is
  `srw-rw---- ship:ship`. `ship host -o json` from the laptop: version
  0.0.2, Docker `ready`. `command -v ship` in a non-interactive ssh session:
  `/usr/local/bin/ship`.
- **Host A** (upgrade, not the clean-host scenario): it ran 0.0.1 after
  the binary was replaced, because nothing restarted it (advisory f-018).
  After the Human's `sudo systemctl restart ship` at 13:23:39 GMT it
  reports version 0.0.2, Docker `ready`, `NRestarts=0`. Not rebooted.

#### Scenario 2: target add, projects — PASS (both hosts)

With a fresh config (`SHIP_CONFIG` in a temp dir):
`ship target add lab ssh://USER@HOST` exit 0, then `ship projects -o json`
exit 0. The output is `{"projects": []}`, identical after `jq -S` to
`crates/ship/tests/fixtures/projects-empty.json`. Text: `No projects.`
Host A answered from its 0.0.1 daemon. API v1 on both sides.

#### Scenario 3: UI ready, Docker stopped shows unreachable — PASS (UI half through a stand-in)

- **Ready:** `ship open --no-browser` against host B printed
  `http://127.0.0.1:N/` after `/healthz`. `GET /` 200 text/html. A headless
  Chrome render on the laptop shows
  `✓ Docker ready Ship 0.0.2 (API v1) … Docker Engine 29.10.1 Docker Compose 5.8.0`.
- **Docker stopped** (`~/ship-gate/run2/`, the sudo script): Docker was
  stopped (`docker.socket` and `docker.service`) 13:15:46.656 → 13:16:57.516Z.
  In all 12 samples, 13:15:56.801Z–13:16:52.457Z, `ship host -o json`,
  run as the login user, exits 0 with `docker.status` `unreachable`,
  `engine_version` null and a `message` naming `systemctl start docker`.
  `systemctl show ship` shows `ActiveState=active`, `MainPID=5162` and
  `NRestarts=0`, the same as the baseline. 11 s after Docker started:
  `ready`, PID 5162, `NRestarts=0`. The daemon kept running throughout.
- **UI while stopped:** the laptop observer was not started, so the real
  UI was not rendered while Docker was down. Stand-in: the release UI
  bundle (`index-C7gdC2Ly.js`, sha256 `4ab41272…a4a416`, fetched from the
  host B daemon) was served on the laptop next to a stub that returns the
  daemon's recorded 13:16:22Z `/api/v1/host` answer. Headless Chrome
  renders `✕ Docker unreachable Docker Engine is not reachable: … Docker Engine unknown Docker Compose 5.8.0 …`.
  The UI polls `/api/v1/host` (`web/src/lib/poller.svelte.ts:32`), the
  same endpoint `ship host` reads.
- **Observation:** while the Engine is down, `compose_version` stays
  `"5.8.0"`, because the Compose CLI plugin still answers. The UI's example
  `web/src/lib/contract/host-unreachable.json` has it null. Both satisfy the
  contract's types. The message contains `running?. Start` (cosmetic).

#### Scenario 4: listeners — PASS (both hosts)

`ss -ltnH` on host B against `~/ship-gate/listeners-before.txt`: one new
TCP listener, `127.0.0.1:7180`. Sockets owned by the `ship` uid in
`/proc/net/{tcp,tcp6,udp,udp6}`: only `0100007F:1C0C` (127.0.0.1:7180)
on both hosts. Unix listener `/run/ship/ship.sock`, mode 0660 `ship:ship`
in `/run/ship` 0750. The daemon does not run with `--tailnet`, so no
tailnet listener. Host A shows the same. Rechecked on host B after the
13:23 reboot: unchanged.

#### Scenario 5: pre-existing Docker objects — PASS (host B)

Compared with `~/ship-gate/*-before.json` after everything: the 12:35
reboot, the Docker stop and start, uninstall, reinstall and the 13:23
reboot. Checked 13:26 GMT.
- Volume `ship-gate-sentinel`: `docker volume inspect` identical;
  `sha256sum /data/seed` (busybox:1.37, read-only mount) identical,
  `484211ae…838c24`.
- Container: differs only in `State.Pid`, `StartedAt`, `FinishedAt`,
  `NetworkSettings.SandboxID`/`SandboxKey`, the default bridge's
  `EndpointID`/`MacAddress`/`NetworkID`, and `HostConfig.Dns` `null` → `[]`.
  `Dns` is not in the plan's list of excluded fields. It changed at the
  first reboot after creation (12:35) and has not changed since, across a
  Docker restart and a second reboot. No `docker events` came from Ship.
  Read as Docker normalising the field when it reloads the container from
  disk. The Reviewer should confirm that reading.
- Networks `bridge`/`host`/`none`: only the default bridge's `Id`,
  `Created` and the sentinel's endpoint changed (excluded by the plan).
- No other containers, volumes or networks exist: Ship created none.

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

#### Scenario 7: service uninstall leaves state and Docker objects — PASS (host B)

From the sudo script's evidence:
- `ship service uninstall` exit 0: `Stopped and removed /etc/systemd/system/ship.service.`,
  `Left in place: the ship user and group, /var/lib/ship, and every container and volume.`
  Afterwards `is-active` inactive, `is-enabled` not-found, no unit file;
  `ship` user and group still present. `ship host` exit 1 with
  `no Ship daemon is listening on /run/ship/ship.sock; start it with sudo systemctl start ship`.
- State directory: `/var/lib/ship` `drwx------ ship:ship` with `ship.db`,
  sha256 `89a9ea26…c92371`. The listing and hash after uninstall are
  identical to those after reinstall.
- Docker: `docker inspect` of the sentinel container and volume,
  identical before and after uninstall.
- Reinstall: `ship service install` exit 0 (`runtime directory: created`,
  `unit: created`, `enabled at boot: created`, the rest `ok`).
  `systemctl start ship` → active, PID 19377, `ship host` `ready`, 0.0.2.

#### Scenario 8: JSON matches fixtures — PASS (Phase 0 commands)

- `ship projects -o json`: identical to `projects-empty.json` (scenario 2).
- `ship host -o json` on both hosts (`ready`) and on host B with Docker
  stopped (`unreachable`): the same key paths as `host.json` (jq `paths`,
  no diff). Types match except where the contract allows null:
  `engine_version` null and `message` a string when unreachable.
- These are the two Phase 0 commands in `docs/contracts/cli-json.md`.

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

#### Sudo steps (done by the Human)

Gate host B: `sudo sh ~/ship-gate/run2-sudo.sh` (sha256 `aef193c7…bcf20f`;
steps.log 13:15:46–13:17:33Z), then `sudo reboot` (boot 13:23:44 GMT).
Gate host A: `sudo systemctl restart ship` (13:23:39 GMT).

#### Run 2 status

Every scenario and both performance checks have evidence on v0.0.2. Two
parts rest on stand-ins, because the real thing cannot be run: the UI's
`unreachable` rendering in scenario 3 (real bundle, recorded daemon answer)
and the older client in scenario 6 (no release speaks another API).
`HostConfig.Dns` in scenario 5 needs the Reviewer's reading. Open
advisory: f-018 (upgrade hint does not say restart). Flipped coded for
review.
