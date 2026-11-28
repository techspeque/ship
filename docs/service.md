# Running the daemon as a service

`ship daemon` runs on Linux under systemd as a dedicated `ship` user.
`ship service install` sets that up; `ship service uninstall` takes the unit
away again. Both need root and are safe to run more than once.

## Install

Put the binary somewhere every user can run it, then install the service:

```sh
sudo install -m 0755 ship /usr/local/bin/ship
sudo ship service install
sudo systemctl start ship
```

The daemon runs as the `ship` user, so install refuses, before changing
anything, a binary that user cannot run: one under a home directory
(`/home`, `/root`, or the home of the user running `sudo`), or one where the
binary or a directory above it is not executable by other users. systemd
would otherwise fail to start it (`status=203/EXEC`) and retry forever. The
refusal prints the two commands that fix it:

```sh
sudo install -m 0755 ~/.local/bin/ship /usr/local/bin/ship
sudo /usr/local/bin/ship service install
```

`/usr/local/bin` is also on the PATH of a non-interactive ssh session,
which ssh targets need; `~/.local/bin` usually is not.

Install needs Docker Engine already on the host (it looks for the `docker`
group). It does the following, skipping anything already in place:

| What | Detail |
|---|---|
| `ship` group | `groupadd --system ship` |
| `ship` user | system user, primary group `ship`, home `/var/lib/ship`, no login shell, no password (the account is locked) |
| Docker access | `ship` is added to the `docker` group |
| State directory | `/var/lib/ship`, owner `ship:ship`, mode 0700 |
| Socket directory | `/run/ship`, owner `ship:ship`, mode 0750; systemd recreates it on every start |
| Unit | `/etc/systemd/system/ship.service`, enabled at boot, `Restart=always` |

The daemon's socket is `/run/ship/ship.sock`, group `ship`, mode 0660. It
also listens on 127.0.0.1:7180 and nowhere else.

Install creates no password, token or key. A second run prints `ok` for
every step and `Nothing changed.`

A running daemon keeps the binary it started with. After upgrading the
binary in place, restart the daemon with `sudo systemctl restart ship`.
Install (run it again if the binary's path changed) and `install.sh` both
check: when `ship.service` is active and its daemon reports a version other
than the installed binary's, or reports none, they print that restart
command. `ship host` shows a `Client:` line when the client's version is
not the daemon's.

**Docker access is root.** The `ship` user can start any container, which
is root on the host. Anyone who can use the daemon's socket can make the
daemon do the same, so membership of the `ship` group is root-equivalent
too. Ship is not an isolation boundary.

## Letting a login user use Ship

The local target and the ssh target both talk to the daemon through its
socket, so the login user on the host needs to be in the `ship` group:

```sh
sudo usermod -aG ship USER
```

The user must log in again (or start a new ssh session) for the group to
apply. Then, from a laptop:

```sh
ship target add lab ssh://USER@HOST
ship projects
```

Without the group, the client fails with a permission error that names the
`ship` group. To revoke access: `sudo gpasswd -d USER ship`.

## When the daemon stops mid-operation

If the daemon is killed, crashes or the host loses power while an
operation runs, systemd starts it again (`Restart=always`) and the daemon
does this before it serves anything, without a Docker command that changes
anything:

- every operation left queued or running becomes `interrupted`; none is
  resumed, and `ship push --wait` exits 93 for it;
- the deployment it was running ends `interrupted`, and the last
  deployment that succeeded stays the one Ship reports serving;
- what that operation staged is removed, and nothing else: a half-written
  upload, a release directory it never recorded, and the env files (the
  values) of a release it prepared but never brought up;
- what Docker runs is observed again from Ship's labelled containers.

What that leaves depends on where the deployment was:

| Interrupted while | Docker | What to do |
|---|---|---|
| uploading, validating, building or preparing | unchanged; the previous release still serves | nothing; push again when ready |
| deploying or verifying | the new containers may already run in place of the old, in part or in full | run the command `ship status` names |

In the second case `ship status` prints

```
Interrupted: deployment dep_… bringing up release rel_… was interrupted while deploying; the services show what Docker runs
Recover:     ship redeploy PROJECT
```

and `-o json` carries the same as `interruption`. `ship redeploy` deploys
the last good revision again and brings the environment back to a known
release. If nothing had ever succeeded, the command is `ship push`; if the
environment was being recreated after `ship remove`, it is `ship push
--recreate`. Ship does not do this by itself: it never reconciles at start
(ADR-0006 rule 11).

## Uninstall

```sh
sudo ship service uninstall
```

This stops and disables the daemon and removes
`/etc/systemd/system/ship.service`. It leaves the `ship` user and group,
`/var/lib/ship` and every container and volume where they are; apps keep
running under Docker. Remove those yourself if you mean to.

## A unit Ship did not write

Install and uninstall only touch a unit whose first line is

```
# Written by ship service install. ship service uninstall removes it.
```

Any other file at `/etc/systemd/system/ship.service` is refused and left
as it is. Move it aside first if Ship should manage the service.
