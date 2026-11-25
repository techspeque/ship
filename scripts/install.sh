#!/bin/sh
# Install the ship binary from a GitHub release, verified before it is used.
#
# Order: download into a fresh temporary directory, verify the Sigstore
# signature of SHA256SUMS and of the archive (cosign, keyless, pinned to this
# repository's release workflow at the requested tag), check the archive's
# SHA-256 against its single line in SHA256SUMS, extract, install. Nothing
# is installed unless every check passes. The script never runs as root
# itself; only the final copy uses sudo, and only when the prefix is not
# writable.
#
# Usage: sh install.sh [--version vX.Y.Z] [--prefix DIR] [--from DIR]
# Exit codes: 0 installed, 1 refused or failed, 2 usage error.

set -eu

REPO="techspeque/ship"
ISSUER="https://token.actions.githubusercontent.com"

usage() {
	cat <<'EOF'
Usage: sh install.sh [--version vX.Y.Z] [--prefix DIR] [--from DIR]

  --version vX.Y.Z  release to install (default: the latest release)
  --prefix DIR      directory for the ship binary (default: /usr/local/bin
                    when writable or running as root, else ~/.local/bin)
  --from DIR        use release files already in DIR instead of downloading
                    them; needs --version
  -h, --help        show this help

The release signature is checked with cosign, which must be installed.
EOF
}

say() { printf 'install.sh: %s\n' "$*"; }
die() {
	printf 'install.sh: %s\n' "$*" >&2
	printf 'install.sh: nothing was installed\n' >&2
	exit 1
}
usage_error() {
	printf 'install.sh: %s\n' "$*" >&2
	usage >&2
	exit 2
}

version=""
prefix=""
from=""
while [ $# -gt 0 ]; do
	case "$1" in
	--version)
		[ $# -ge 2 ] || usage_error "--version needs a value"
		version=$2
		shift 2
		;;
	--version=*)
		version=${1#*=}
		shift
		;;
	--prefix)
		[ $# -ge 2 ] || usage_error "--prefix needs a value"
		prefix=$2
		shift 2
		;;
	--prefix=*)
		prefix=${1#*=}
		shift
		;;
	--from)
		[ $# -ge 2 ] || usage_error "--from needs a value"
		from=$2
		shift 2
		;;
	--from=*)
		from=${1#*=}
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		usage_error "unknown option: $1"
		;;
	esac
done

# The version ends up in URLs and in the pinned signing identity, so only
# a plain release tag is accepted.
valid_version() {
	printf '%s\n' "$1" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$'
}

if [ -n "$version" ] && ! valid_version "$version"; then
	usage_error "--version must look like v1.2.3 or v1.2.3-rc.1, got: $version"
fi
if [ -n "$from" ] && [ -z "$version" ]; then
	usage_error "--from needs --version"
fi
if [ -n "$from" ] && [ ! -d "$from" ]; then
	usage_error "--from: not a directory: $from"
fi

case "$(uname -s)" in
Linux) os=linux ;;
Darwin) os=darwin ;;
*) die "unsupported operating system: $(uname -s) (releases exist for Linux and macOS)" ;;
esac
case "$(uname -m)" in
x86_64 | amd64) arch=amd64 ;;
aarch64 | arm64) arch=arm64 ;;
*) die "unsupported architecture: $(uname -m) (releases exist for amd64 and arm64)" ;;
esac

# Checked before any download, so a missing verifier costs nothing.
if ! command -v cosign >/dev/null 2>&1; then
	die "cosign is required to verify the release signature and was not found on PATH; install it (https://docs.sigstore.dev/cosign/system_config/installation/) and run this script again"
fi

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{ print $1 }'
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | awk '{ print $1 }'
	else
		die "neither sha256sum nor shasum is available to check the archive"
	fi
}

download() {
	# $1 URL, $2 output file
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q --https-only -O "$2" "$1"
	else
		die "neither curl nor wget is available to download the release"
	fi
}

if [ -z "$version" ]; then
	command -v curl >/dev/null 2>&1 || die "finding the latest release needs curl; pass --version instead"
	latest=$(curl -fsSLI --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' \
		"https://github.com/$REPO/releases/latest") || die "could not reach GitHub to find the latest release"
	version=${latest##*/}
	valid_version "$version" || die "could not determine the latest release (got: $version)"
fi

archive="ship-$version-$os-$arch.tar.gz"
identity="https://github.com/$REPO/.github/workflows/release.yml@refs/tags/$version"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t ship-install)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM
mkdir "$tmp/release" "$tmp/extract"

# Every file is copied or downloaded into the temporary directory first, so
# what is verified is exactly what is installed.
for name in "$archive" "$archive.sigstore.json" SHA256SUMS SHA256SUMS.sigstore.json; do
	if [ -n "$from" ]; then
		[ -f "$from/$name" ] || die "missing release file: $from/$name"
		cp "$from/$name" "$tmp/release/$name"
	else
		say "downloading $name"
		download "https://github.com/$REPO/releases/download/$version/$name" "$tmp/release/$name" ||
			die "download failed: $name (does release $version exist for $os-$arch?)"
	fi
done

verify_signature() {
	# $1 file name inside $tmp/release
	if ! cosign verify-blob \
		--bundle "$tmp/release/$1.sigstore.json" \
		--certificate-identity "$identity" \
		--certificate-oidc-issuer "$ISSUER" \
		"$tmp/release/$1" >"$tmp/cosign.log" 2>&1; then
		cat "$tmp/cosign.log" >&2
		die "signature of $1 does not verify against $identity"
	fi
}

verify_signature SHA256SUMS
verify_signature "$archive"
say "signatures verified ($identity)"

# Exactly one line must name the archive (optionally in binary mode, "*name").
expected=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1 }' "$tmp/release/SHA256SUMS")
count=$(printf '%s' "$expected" | grep -c . || true)
[ "$count" -eq 1 ] || die "$archive is listed $count times in SHA256SUMS, expected once"
printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' || die "SHA256SUMS line for $archive is malformed"
actual=$(sha256 "$tmp/release/$archive")
[ "$actual" = "$expected" ] || die "checksum mismatch for $archive: expected $expected, got $actual"
say "checksum verified ($actual)"

tar -xzf "$tmp/release/$archive" -C "$tmp/extract" || die "could not extract $archive"
bin="$tmp/extract/ship-$version-$os-$arch/ship"
if [ ! -f "$bin" ] || [ -L "$bin" ]; then
	die "$archive does not contain ship-$version-$os-$arch/ship as a regular file"
fi

if [ -z "$prefix" ]; then
	if [ "$(id -u)" -eq 0 ] || [ -w /usr/local/bin ]; then
		prefix=/usr/local/bin
	else
		prefix="$HOME/.local/bin"
	fi
fi

[ -d "$prefix" ] || mkdir -p "$prefix" 2>/dev/null || true
if [ -d "$prefix" ] && [ -w "$prefix" ]; then
	install -m 0755 "$bin" "$prefix/ship"
else
	command -v sudo >/dev/null 2>&1 || die "$prefix is not writable and sudo is not available; pass --prefix"
	say "$prefix is not writable; using sudo for the final copy only"
	sudo mkdir -p "$prefix"
	sudo install -m 0755 "$bin" "$prefix/ship"
fi

say "installed $prefix/ship"
"$prefix/ship" --version || true
case ":$PATH:" in
*":$prefix:"*) ;;
*) say "note: $prefix is not on PATH" ;;
esac
if [ "$os" = linux ]; then
	say "to run the daemon under systemd: sudo $prefix/ship service install"
fi
