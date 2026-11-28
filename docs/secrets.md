# Config values and secrets

A pushed directory never carries `.env` (ADR-0005), so the values an app
needs to start are set on the daemon instead (ADR-0009): config values,
which are not secret, and secrets, which are write-only.

```sh
ship vars set LOG_LEVEL=info --project my-app
ship vars set LOG_LEVEL=debug --project my-app --env development
ship vars set WORKERS=4 --project my-app --service worker
ship secrets set DATABASE_URL --prompt --project my-app
printf '%s' "$TOKEN" | ship secrets set API_TOKEN --stdin --project my-app
ship vars list --project my-app
ship secrets list --project my-app -o json
ship secrets unset API_TOKEN --project my-app
```

Every service gets each value as an environment variable. A value is set
for a project, for one of its environments (`--env`; Phase 1 has
`development` only) or for one service of an environment (`--service`,
known from the service's first deployment). When the same name is set at
several of these, the most specific wins: service over environment over
project. Ship has no manifest of its own yet, so nothing sits below the
project. One scope cannot hold a config value and a secret of the same
name.

For a Compose project, a name the service also sets under its own
`environment:` keeps the file's value: Compose puts `environment:` above the
env file Ship gives it, and Ship changes nothing the file declares
(ADR-0008). The deployment's log names each such name.

## Nothing running changes

Setting or unsetting a value changes no running container. It applies at
the next deployment. Until then `ship vars set`, `ship secrets set` and the
lists name the services running with older values (`stale_services`).

## Secrets are write-only

A secret's value is never an argument: `ship secrets set NAME=VALUE`, or a
value after the name, is refused before anything is sent, so it stays out
of shell history and the process list. `--prompt` reads the value from the
terminal with echo off; `--stdin` reads all of stdin, less one final
newline. The daemon answers names and the time each was set, never a
value: not in a list, an operation, an event, a log or an error.

The daemon's logs and every operation's log and events redact each secret
value wherever it appears, including in build output and in what a
service prints. The values stored before a restart are redacted from the
daemon's first log line on.

## How a secret is protected in Phase 1

**By file permissions, not encryption.** Each secret value is a file under
`<state>/secrets/` (by default `/var/lib/ship/secrets/`), mode 0600, in a
directory of mode 0700, owned by the `ship` user the daemon runs as. Phase
2 encrypts this store with a key kept outside the database (ADR-0009 rule
4); stored values will then be migrated.

At each deployment the values a service gets are written, in the same
plain form, to `<state>/releases/<release>/env/<service>.env` (0600, in a
0700 directory), outside the snapshot, and handed to Compose as an
`env_file`. They are never in an image, a build argument or a Compose file
Ship writes: the override that names the env file holds only its path. The
env files are kept for the release running now and the one before it,
which a failed deployment brings back, and removed for older releases.

So, plainly:

- Anyone who can read the `ship` user's files, root included, can read
  every secret.
- A service can read every secret it is given; that is the point of giving
  it.
- Anyone with Docker access on the host (the `ship` group, the `docker`
  group, root) can read every secret injected into a container, for
  instance with `docker inspect`. Docker access is root on the host anyway.
- A backup of the state directory holds every secret in plain text. Keep
  it as safe as the host.

## Limits

- A name is an environment variable name: a letter or `_`, then letters,
  digits or `_`, at most 256 bytes.
- A value is one line, at most 64 KiB: no newline, carriage return or NUL,
  and it cannot end with a backslash, which Compose's env-file format cannot
  hold. Anything else, `$`, quotes and `#` included, arrives literally.
- A secret is at least 4 bytes. The logs redact every occurrence of a
  secret, so a shorter one would mangle them; a value that short is not a
  secret worth the name: set it with `ship vars`.
