# phase-0-gate — Phase 0 gate: composed-system validation

- **Type:** gate | **Risk:** high | **Priority:** p2
- **Coder:** claude-code/opus-5-5 | **Date:** 2026-11-27

## Phase being validated

Phase 0: Foundation, as released in v0.0.1 (tag v0.0.1, commit 30104f4b5e20).

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

- [x] All phase slices coded and reviewed (ws-0.1..0.9, fix-0001..0005, feat-0001 archived)
- [ ] Cross-module integration tests pass — scenario 1 fails on the release build
- [ ] No interface mismatches at seam points — not evidenced
- [ ] Performance/resource usage acceptable — not evidenced

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
