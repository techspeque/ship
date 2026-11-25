# Releases

`.github/workflows/release.yml` builds, signs and publishes a release when
a `v*` tag is pushed (ADR-0012 rule 2). Nothing is signed with a key of our
own: every file is signed by Sigstore cosign, keyless, with the release
workflow's GitHub OIDC identity at that tag.

## Cutting a release

1. Set `version` in `[workspace.package]` of `Cargo.toml` (and
   `web/package.json`), commit, and merge to `main` through the usual
   review.
2. Tag that commit `v<version>` and push the tag. A tag that differs from
   the workspace version fails the first job.
3. The workflow:
   - builds `web/dist` once and embeds it in every binary
     (`SHIP_REQUIRE_UI=1`);
   - builds `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`
     natively on amd64 and arm64 runners (static binaries), and
     `x86_64-apple-darwin` and `aarch64-apple-darwin` on a macOS runner;
   - checks each Linux binary: statically linked, `ship --version` inside a
     `busybox` container with no network, and
     `scripts/no-network-smoke.sh` (every root-free Phase 0 command under
     `strace`, failing on any connection to an IP address);
   - writes `ship-<tag>.spdx.json` (Syft, from `Cargo.lock` and
     `web/package-lock.json`) and `SHA256SUMS`;
   - signs each archive, the SBOM and `SHA256SUMS` with cosign;
   - runs `scripts/install.sh` against the signed files with real cosign
     (must install) and against two tampered copies (must refuse);
   - publishes a GitHub pre-release with all of it and `install.sh`.

`workflow_dispatch` (Actions → release → Run workflow) is a dry run: it
builds, checks and writes the SBOM and checksums, and stops before signing
and publishing.

## Release files

| File | What |
|---|---|
| `ship-<tag>-linux-amd64.tar.gz` | static binary, `LICENSE`, `README.md` |
| `ship-<tag>-linux-arm64.tar.gz` | static binary, `LICENSE`, `README.md` |
| `ship-<tag>-darwin-amd64.tar.gz` | client binary, `LICENSE`, `README.md` |
| `ship-<tag>-darwin-arm64.tar.gz` | client binary, `LICENSE`, `README.md` |
| `ship-<tag>.spdx.json` | SBOM (SPDX JSON) |
| `SHA256SUMS` | SHA-256 of the four archives and the SBOM |
| `<file>.sigstore.json` | cosign bundle for each archive, the SBOM and `SHA256SUMS` |
| `install.sh` | the verifying installer |

## Verifying by hand

With cosign 2.4 or later, for tag `v0.0.1` on linux amd64:

```sh
tag=v0.0.1
cosign verify-blob \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-identity "https://github.com/techspeque/ship/.github/workflows/release.yml@refs/tags/$tag" \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS     # macOS: shasum -a 256 --ignore-missing -c SHA256SUMS
tar -xzf "ship-$tag-linux-amd64.tar.gz"
```

The identity pins both the workflow file and the tag: a signature made by
any other workflow, branch or repository does not verify.

## install.sh

`sh install.sh [--version vX.Y.Z] [--prefix DIR] [--from DIR]`

- Refuses (exit 1, nothing installed) when cosign is missing, when a
  signature does not verify, when the archive is not listed exactly once in
  `SHA256SUMS`, or when its SHA-256 differs.
- Works on copies in a fresh temporary directory, so what it verifies is
  what it installs.
- Runs as the invoking user; `sudo` is used for the final copy only, and
  only when the prefix is not writable.
- `--from DIR` installs from release files already on disk (for hosts that
  cannot reach GitHub); it needs `--version`.

`scripts/install-selftest.sh` checks these refusals in CI against a fake
release and a stub cosign.

## Dependency audit

The CI job `audit` (see `CONTRIBUTING.md`) runs on every push and pull
request, not only at release time. A release tag is cut from a commit whose
CI, including `audit`, is green.
