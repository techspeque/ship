---
type: adr
id: 0003
title: Ship secures no transport of its own; local socket, SSH and the host's tailnet carry it
status: proposed
date: 2026-11-24
phase: 0
---

# ADR-0003: Ship secures no transport of its own; local socket, SSH and the host's tailnet carry it

> Phase: 0 | Status: proposed | Date: 2026-11-24
> Decision drivers: writing transport auth and TLS is where small tools get breached; Ship must match Gaia's network model without embedding tsnet.

## Context

The v0.4 draft had Ship implement HTTPS, pairing codes, certificate pinning
and bearer tokens. That is the largest and riskiest part of a first release
and none of it is what Ship is for.

Gaia (ADR-0004) lets only daemons join a tailnet and has the CLI talk to a
local socket. Ship cannot copy that exactly: it is Rust, so there is no
embedded `tsnet`, and `ship push` runs on a laptop that has no Ship daemon.
But every machine Ship targets already has SSH, and a machine on a tailnet
already has `tailscaled`.

## Decision

The daemon listens only on a unix socket, 127.0.0.1 and, when enabled, the
host's tailnet address. Ship implements no TLS, pairing or tokens. A client
reaches the daemon over one of three transports:

- **local:** the unix socket directly.
- **ssh:** the client runs `ssh HOST ship dial-stdio`, which bridges stdin and stdout to the socket.
- **tailnet:** plain HTTP to the daemon's tailnet address; WireGuard encrypts it.

### Rules this decision implies:

1. No listener ever binds 0.0.0.0 or a non-tailnet interface. A test enumerates listeners and fails otherwise.
2. The unix socket is mode 0660, group `ship`. Socket access is the whole authorisation for local and ssh transports.
3. The tailnet listener is off by default. When on, every request is authorised by asking `tailscaled`'s local API who the peer is, then checking a host-side allowlist. No identity, no access.
4. Ship never embeds a tailnet client and never manages tailnet keys for the host. Tailscale and Headscale are treated the same.
5. Phase 0–1 ship the local and ssh transports only. The tailnet listener arrives in Phase 2.
6. Browser requests that mutate state require a same-origin check on every transport.
7. The UI is reached over the tailnet listener, or through `ship open`, which forwards a local port over ssh.
8. A later Gaia node runs the `ship` CLI against the unix socket like any other local client; Ship needs no change for it, and Gaia's tailnet carries no Ship traffic.

## Consequences

### Positive
- No certificates, pairing flow, token store or revocation to build or get wrong.
- Phase 1 works with nothing but SSH.
- Same shape as Gaia: loopback, local socket, tailnet. One mental model across both tools.

### Negative
- No remote access without SSH or a tailnet.
- The ssh transport needs `ship` installed on the host's PATH for the login user, and that user in the `ship` group.
- Tailnet mode depends on a system `tailscaled`, which Gaia avoids.

### Neutral
- Public exposure of the UI is out of scope; Gaia's public link (Gaia ADR-0011) is the family's answer to that.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Own HTTPS + pairing + pinning (v0.4) | No dependencies | Most security code in the project | Too much to own |
| Embedded tailnet via libtailscale FFI | Matches Gaia exactly | Go toolchain in a Rust build, immature bindings | Build complexity |
| `tailscale serve` in front of loopback | Real HTTPS cert | Peer identity arrives as spoofable headers on loopback | Weaker authorisation |
| Tailnet only | One path | Unusable without an account | Must stand alone |

## References

- ADR-0004, ADR-0010
- Gaia ADR-0004, ADR-0011
