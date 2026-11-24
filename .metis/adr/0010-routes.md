---
type: adr
id: 0010
title: Routes are explicit records with three providers: published port, tailnet, Caddy
status: proposed
date: 2026-11-24
phase: 2
---

# ADR-0010: Routes are explicit records with three providers: published port, tailnet, Caddy

> Phase: 2 | Status: proposed | Date: 2026-11-24
> Decision drivers: a pushed app should come back with an address that works, without the operator running DNS.

## Context

The v0.4 draft offered hostnames only "when an operator-configured DNS
strategy is available", which leaves the best moment of the product to a
hosts file. On a tailnet, MagicDNS already resolves the host's name on every
device, and a container that joins the tailnet as its own device gets its
own name, address and HTTPS certificate.

A tailnet node has one name; there are no wildcard subdomains beneath it.
So per-app names on a tailnet mean one tailnet device per environment.

Separately, two environments cannot both publish the same host port
(ADR-0008).

## Decision

A route is a stored record tying a service and port to an address through a
provider. Nothing is reachable without one.

- **port** (Phase 1): a published host port. Address is `host:port`, using the host's tailnet name when it has one.
- **tailnet** (Phase 2): a Tailscale sidecar container per environment joins the tailnet and serves the environment's web service over HTTPS on its own name.
- **caddy** (Phase 2): one Caddy instance routes operator-supplied hostnames.

### Rules this decision implies:

1. No route, no exposure. `EXPOSE` in a Dockerfile is a hint for the port prompt and nothing more.
2. The CLI prints only addresses of routes whose status is confirmed, otherwise `Endpoint: not configured` and how to add one.
3. The `port` provider detects conflicts before deploy and never picks a different port silently.
4. The `tailnet` provider needs a tailnet auth key or OAuth client, stored as a Ship secret. Sidecars are tagged and removed with the environment.
5. Tailnet Funnel is never enabled by Ship.
6. The `caddy` provider validates a new configuration before applying it and restores the previous one on failure. Ship does not claim to issue certificates for names it cannot prove.
7. Hostname routing only; no path-prefix routes.
8. Application routes and the management API never share a listener.
9. Services in one environment reach each other by Compose service name. Cross-project networking is explicit, never a shared default network.

## Consequences

### Positive
- With a tailnet, a push returns a working HTTPS address and no DNS work.
- Each environment has its own address, which removes the port collision.
- Without a tailnet, host:port still works.

### Negative
- A sidecar and a tailnet device per environment; device limits apply.
- A tailnet credential lives in Ship's secret store.
- Three providers to test.

### Neutral
- Tailscale's own service-naming features may later replace sidecars; the route record does not change.

## Alternatives Considered

| Option | Pros | Cons | Why rejected |
|---|---|---|---|
| Caddy + operator DNS only (v0.4) | No third party | Hosts-file setup before first URL | First-run experience |
| Distinct ports on the host's tailnet name | No sidecars | Port numbers as app names | Poor addresses |
| Wildcard DNS services (sslip-style) | Zero config | Blocked by rebinding protection; no private TLS | Unreliable |

## References

- ADR-0003, ADR-0008, ADR-0009
