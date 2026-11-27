# Security policy

## Reporting a vulnerability

Report vulnerabilities **privately** through GitHub's private vulnerability
reporting: open the repository's **Security** tab and choose **Report a
vulnerability**. Do not open a public issue, pull request or discussion for a
suspected vulnerability.

Include what you can of:

- the affected version (`ship --version`) and platform;
- the steps to reproduce, and what an attacker gains;
- any proof of concept, with secrets and real hostnames removed.

You will get an acknowledgement, and a fix or a decision is agreed with you
before anything is disclosed publicly.

## Supported versions

Ship is pre-alpha. Only the latest commit on `main` receives fixes.

## Scope

Ship's trust model is described in OVERVIEW.md §5. In short: Ship is not an
isolation boundary; membership of the `ship` group is equivalent to root on
the host; Ship adds no transport security of its own and relies on the unix
socket, SSH or the tailnet. A report that a deployed workload can do what its
own Dockerfile or Compose file allows is expected behaviour, not a
vulnerability. A way to reach the daemon past those boundaries, to read a
secret value through Ship, or to make Ship act on Docker objects it does not
own, is in scope.
