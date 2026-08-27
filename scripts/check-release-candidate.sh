#!/usr/bin/env bash

# Opt-in release-candidate gate. Nothing in CI invokes this automatically.
# It deliberately tests `relay/` separately because the root workspace excludes
# that operational crate.
set -euo pipefail

REPOSITORY="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_DIST="${WALKIE_RELEASE_DIST:-$REPOSITORY/target/release-web}"

cd "$REPOSITORY"

scripts/check-architecture-records.sh

nix develop --command bash -euo pipefail -c '
  cargo fmt --all -- --check
  cargo fmt --manifest-path relay/Cargo.toml -- --check
  cargo test --workspace --all-targets --locked
  cargo test --manifest-path relay/Cargo.toml --locked
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo check --locked --target wasm32-unknown-unknown --features web-ui
  NO_COLOR=false trunk build --release --locked --dist "$1"
' bash "$RELEASE_DIST"

pnpm install --frozen-lockfile
nix develop --command bash -euo pipefail -c '
  WALKIE_BROWSER_EXECUTABLE="$(command -v chromium)" \
    WALKIE_RELEASE_DIST="$1" \
    pnpm browser:acceptance
' bash "$RELEASE_DIST"
