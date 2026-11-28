#!/bin/sh
# Self-test for scripts/install.sh, with no network and no real signatures.
#
# Builds a fake release for the host platform and a stub `cosign` that
# accepts a bundle only when it holds the SHA-256 of the signed file and the
# identity is the one install.sh must pin for the tag. Then checks that a
# good release installs and that every tampered or incomplete one is refused
# with exit 1 and leaves the prefix without a ship binary.
#
# Usage: sh scripts/install-selftest.sh

set -eu

here=$(cd "$(dirname "$0")" && pwd)
installer="$here/install.sh"
version="v0.0.0-selftest"
identity="https://github.com/techspeque/ship/.github/workflows/release.yml@refs/tags/$version"

case "$(uname -s)" in
Linux) os=linux ;;
Darwin) os=darwin ;;
*) echo "selftest: unsupported OS" >&2; exit 1 ;;
esac
case "$(uname -m)" in
x86_64 | amd64) arch=amd64 ;;
aarch64 | arm64) arch=arm64 ;;
*) echo "selftest: unsupported arch" >&2; exit 1 ;;
esac
name="ship-$version-$os-$arch"
archive="$name.tar.gz"

work=$(mktemp -d 2>/dev/null || mktemp -d -t ship-selftest)
trap 'rm -rf "$work"' EXIT

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{ print $1 }'
	else
		shasum -a 256 "$1" | awk '{ print $1 }'
	fi
}

# Stub signer and verifier: a "bundle" is the file's SHA-256.
sign() { sha256 "$1" >"$1.sigstore.json"; }

mkdir -p "$work/stub"
cat >"$work/stub/cosign" <<EOF
#!/bin/sh
set -eu
[ "\$1" = verify-blob ] || { echo "stub cosign: unexpected command \$1" >&2; exit 3; }
shift
bundle="" ident="" issuer="" file=""
while [ \$# -gt 0 ]; do
	case "\$1" in
	--bundle) bundle=\$2; shift 2 ;;
	--certificate-identity) ident=\$2; shift 2 ;;
	--certificate-oidc-issuer) issuer=\$2; shift 2 ;;
	-*) echo "stub cosign: unexpected flag \$1" >&2; exit 3 ;;
	*) file=\$1; shift ;;
	esac
done
[ "\$ident" = "$identity" ] || { echo "stub cosign: wrong identity \$ident" >&2; exit 1; }
[ "\$issuer" = "https://token.actions.githubusercontent.com" ] || { echo "stub cosign: wrong issuer" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
	want=\$(sha256sum "\$file" | awk '{ print \$1 }')
else
	want=\$(shasum -a 256 "\$file" | awk '{ print \$1 }')
fi
[ "\$(cat "\$bundle")" = "\$want" ] || { echo "stub cosign: signature mismatch for \$file" >&2; exit 1; }
EOF
chmod 0755 "$work/stub/cosign"

# A PATH with the tools install.sh needs and no cosign at all.
mkdir -p "$work/nocosign"
for tool in sh awk grep cat cp mkdir mktemp rm tar gzip install id uname sha256sum shasum perl dirname chmod; do
	path=$(command -v "$tool" 2>/dev/null || true)
	case "$path" in
	/*) ln -s "$path" "$work/nocosign/$tool" ;;
	esac
done

# The good release.
rel="$work/release"
mkdir -p "$rel" "$work/build/$name"
# Answers --version like ship, and `host -o json` with $FAKE_DAEMON_VERSION
# as the local daemon's version (fails when it is empty), but only when
# SHIP_CONFIG names no file: install.sh must ask the local daemon, never the
# current target.
cat >"$work/build/$name/ship" <<'EOF'
#!/bin/sh
case "$1" in
--version) echo "ship 0.0.0-selftest (commit selftest)" ;;
host)
	if [ -z "${SHIP_CONFIG:-}" ] || [ -e "$SHIP_CONFIG" ]; then
		echo "selftest ship: host asked without an empty SHIP_CONFIG" >&2
		exit 9
	fi
	[ -n "${FAKE_DAEMON_VERSION:-}" ] || { echo "selftest ship: no daemon" >&2; exit 1; }
	printf '{\n  "api_version": "v1",\n  "version": "%s",\n  "os": "linux"\n}\n' "$FAKE_DAEMON_VERSION"
	;;
*) echo "ship selftest binary $*" ;;
esac
EOF
chmod 0755 "$work/build/$name/ship"
printf 'MIT\n' >"$work/build/$name/LICENSE"
tar -czf "$rel/$archive" -C "$work/build" "$name"
printf 'sbom\n' >"$rel/ship-$version.spdx.json"
(cd "$rel" && for f in ship-*; do printf '%s  %s\n' "$(sha256 "$f")" "$f"; done) >"$rel/SHA256SUMS"
sign "$rel/$archive"
sign "$rel/ship-$version.spdx.json"
sign "$rel/SHA256SUMS"

failures=0
n=0
pass() { n=$((n + 1)); echo "ok $n - $1"; }
fail() { n=$((n + 1)); failures=$((failures + 1)); echo "not ok $n - $1"; }

# run CASE PATH EXPECTED_EXIT EXPECTED_STDERR_FRAGMENT DIR [extra args]
run() {
	case_name=$1 run_path=$2 want=$3 fragment=$4 dir=$5
	shift 5
	prefix="$work/prefix-$n"
	mkdir -p "$prefix"
	set +e
	PATH="$run_path" sh "$installer" --from "$dir" --version "$version" --prefix "$prefix" "$@" \
		>"$work/out" 2>"$work/err"
	got=$?
	set -e
	if [ "$got" -ne "$want" ]; then
		fail "$case_name: exit $got, want $want"
		sed 's/^/#   /' "$work/err"
		return
	fi
	if [ -n "$fragment" ] && ! grep -qF -- "$fragment" "$work/err"; then
		fail "$case_name: stderr lacks '$fragment'"
		sed 's/^/#   /' "$work/err"
		return
	fi
	if [ "$want" -ne 0 ] && [ -e "$prefix/ship" ]; then
		fail "$case_name: refused but $prefix/ship exists"
		return
	fi
	pass "$case_name"
}

copy() { rm -rf "${work:?}/${1:?}" &&cp -R "$rel" "$work/$1" && echo "$work/$1"; }

stubbed="$work/stub:$PATH"

# 1. A good release installs a working, executable binary.
run "good release installs" "$stubbed" 0 "" "$rel"
last="$work/prefix-$((n - 1))/ship"
if [ -x "$last" ] && "$last" --version | grep -q "ship 0.0.0-selftest"; then
	pass "installed binary is executable and runs"
else
	fail "installed binary is executable and runs"
fi

# The prefix is inside a 0700 mktemp directory, which the ship service user
# cannot traverse: on Linux the installer must point at a system path and
# must not suggest service install from the prefix.
if [ "$os" = linux ]; then
	dir=$(cd "$(dirname "$last")" && pwd -P)
	if grep -qF "sudo install -m 0755 $dir/ship /usr/local/bin/ship" "$work/out" &&
		grep -qF "sudo /usr/local/bin/ship service install" "$work/out" &&
		! grep -qF "sudo $dir/ship service install" "$work/out"; then
		pass "per-user prefix points service install at a system path"
	else
		fail "per-user prefix points service install at a system path"
		sed 's/^/#   /' "$work/out"
	fi
fi

# 2. Archive changed and re-signed: only the checksum can catch it.
d=$(copy tampered)
printf 'x' >>"$d/$archive"
sign "$d/$archive"
run "tampered archive refused by checksum" "$stubbed" 1 "checksum mismatch" "$d"

# 3. Archive changed, signature not updated.
d=$(copy badsig)
printf 'x' >>"$d/$archive"
run "archive with a bad signature refused" "$stubbed" 1 "signature of $archive does not verify" "$d"

# 4. SHA256SUMS changed and its signature not updated.
d=$(copy badsums)
zeros=0000000000000000000000000000000000000000000000000000000000000000
printf '%s  %s\n' "$zeros" other.tar.gz >>"$d/SHA256SUMS"
run "SHA256SUMS with a bad signature refused" "$stubbed" 1 "signature of SHA256SUMS does not verify" "$d"

# 5. SHA256SUMS without the archive's line, re-signed.
d=$(copy noline)
grep -vF "$archive" "$rel/SHA256SUMS" >"$d/SHA256SUMS"
sign "$d/SHA256SUMS"
run "archive missing from SHA256SUMS refused" "$stubbed" 1 "listed 0 times" "$d"

# 6. The archive listed twice, re-signed.
d=$(copy twice)
grep -F "$archive" "$rel/SHA256SUMS" >>"$d/SHA256SUMS"
sign "$d/SHA256SUMS"
run "archive listed twice refused" "$stubbed" 1 "listed 2 times" "$d"

# 7. Another file's bundle in place of SHA256SUMS's.
d=$(copy swapped)
cp "$d/ship-$version.spdx.json.sigstore.json" "$d/SHA256SUMS.sigstore.json"
run "swapped signature bundle refused" "$stubbed" 1 "signature of SHA256SUMS does not verify" "$d"

# 8. A missing signature bundle.
d=$(copy nobundle)
rm "$d/$archive.sigstore.json"
run "missing signature bundle refused" "$stubbed" 1 "missing release file" "$d"

# 9. No cosign at all.
run "no cosign refused" "$work/nocosign" 1 "cosign is required" "$rel"

# 10–12. Usage errors exit 2.
run "unknown option is a usage error" "$stubbed" 2 "unknown option" "$rel" --bogus
run "malformed version is a usage error" "$stubbed" 2 "--version must look like" "$rel" --version 'v1;rm'

set +e
PATH="$stubbed" sh "$installer" --from "$rel" >/dev/null 2>"$work/err"
got=$?
set -e
if [ "$got" -eq 2 ] && grep -qF -- "--from needs --version" "$work/err"; then
	pass "--from without --version is a usage error"
else
	fail "--from without --version is a usage error (exit $got)"
fi

# 13–16 (Linux). An upgrade over a running daemon (f-018): with a system-path
# prefix and ship.service active, install.sh names a restart unless the
# daemon reports the installed version. A stub systemctl answers is-active
# from $FAKE_ACTIVE.
if [ "$os" = linux ]; then
	sysroot=$(mktemp -d 2>/dev/null || mktemp -d -t ship-selftest-sys)
	trap 'rm -rf "$work" "$sysroot"' EXIT
	chmod 0755 "$sysroot"
	mkdir -p "$work/systemd"
	cat >"$work/systemd/systemctl" <<'EOF'
#!/bin/sh
[ "$*" = "is-active --quiet ship.service" ] || { echo "stub systemctl: unexpected $*" >&2; exit 64; }
[ "${FAKE_ACTIVE:-0}" = 1 ]
EOF
	chmod 0755 "$work/systemd/systemctl"

	# hint CASE ACTIVE DAEMON_VERSION EXPECTED_FRAGMENT (empty: no restart hint)
	hint() {
		prefix="$sysroot/bin-$n"
		mkdir -p "$prefix"
		chmod 0755 "$prefix"
		set +e
		FAKE_ACTIVE=$2 FAKE_DAEMON_VERSION=$3 PATH="$work/systemd:$stubbed" \
			sh "$installer" --from "$rel" --version "$version" --prefix "$prefix" \
			>"$work/out" 2>"$work/err"
		got=$?
		set -e
		if [ "$got" -ne 0 ]; then
			fail "$1: exit $got"
			sed 's/^/#   /' "$work/err"
			return
		fi
		dir=$(cd "$prefix" && pwd -P)
		if ! grep -qF "sudo $dir/ship service install" "$work/out"; then
			pass "$1 # SKIP $dir is not a system path on this host"
			return
		fi
		ok=no
		if [ -n "$4" ]; then
			if grep -qF -- "$4" "$work/out" && grep -qF "sudo systemctl restart ship" "$work/out"; then
				ok=yes
			fi
		elif ! grep -qF "systemctl restart" "$work/out"; then
			ok=yes
		fi
		if [ "$ok" = yes ]; then
			pass "$1"
		else
			fail "$1"
			sed 's/^/#   /' "$work/out"
		fi
	}
	hint "running daemon on another version is told to restart" 1 0.0.0-old \
		"the running daemon is ship 0.0.0-old, not ship 0.0.0-selftest"
	hint "running daemon on the installed version needs nothing" 1 0.0.0-selftest ""
	hint "running daemon with no version is told to restart" 1 "" "did not report its version"
	hint "no running daemon: no restart hint" 0 0.0.0-old ""
fi

echo "1..$n"
if [ "$failures" -ne 0 ]; then
	echo "selftest: $failures of $n failed" >&2
	exit 1
fi
echo "selftest: all $n passed"
