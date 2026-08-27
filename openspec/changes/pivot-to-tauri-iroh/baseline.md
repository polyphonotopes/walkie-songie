# Interrupted-work baseline

Captured on 2026-07-30 before applying `pivot-to-tauri-iroh`.

## Verified baseline

- `cargo test --no-default-features`: 57 unit tests and 16 L0 integration
  tests passed (73 total).
- `cargo check --target wasm32-unknown-unknown --no-default-features`: failed
  because `pitch/swiftf0.rs` imported `crate::web::onnx_bridge` while the
  interrupted transport cutover had disabled the entire `web` module.
- The recovered lockfile resolved Iroh 1.0.2 before the deliberate 1.0.3
  toolchain update.

## Recovered work that must be retained

- `src/net/identity.rs`: one-seed p2panda/Iroh Ed25519 identity.
- `src/room/ops.rs`: signed operation schema and verification.
- `src/room/store.rs`: append-only RoomStore and HHHS projection.
- `src/room/test_support.rs` and `tests/`: independent convergence/oracle
  harness.
- `src/room/mod.rs` and `src/lib.rs`: exports needed by that implementation.

These files are incomplete relative to the new operation schema and native
runtime, but they are the tested base for those changes and are not disposable
transport scaffolding.

## Dirty tree at capture

Tracked changes existed in `.cargo/config.toml`, `Cargo.lock`, `Cargo.toml`,
`assets/onnx-bridge.js`, `assets/sw.js`, `src/lib.rs`, `src/room/mod.rs`, and
`src/web/app.rs`; `dist/index.html` was deleted. Generated wasm assets,
`src/net/`, the RoomStore files, integration tests, and the prior
now-archived `rewrite-p2panda-hhhs-stack` change were untracked. All are treated as recovered
user work. Removal remains deferred until the corresponding Tauri replacement
passes its acceptance tests.
