.PHONY: verify web fmt clippy test

# `web` runs first, so every cargo step below embeds the fresh UI build.
verify: web fmt clippy test

# The UI: install exactly the locked dependencies, lint, type-check,
# unit-test (including the contract test against the Rust fixtures) and
# build web/dist, which crates/ship-daemon/build.rs embeds.
web:
	cd web && npm ci --no-audit --no-fund
	cd web && npm run lint
	cd web && npm run check
	cd web && npm test
	cd web && npm run build

fmt:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace
