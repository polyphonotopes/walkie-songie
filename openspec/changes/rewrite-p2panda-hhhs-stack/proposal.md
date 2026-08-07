# Change: Rewrite transport + state layer on iroh + p2panda + HHHS

> **Status (2026-07-30): partially superseded by
> `pivot-to-tauri-iroh`.** Retain the completed signed p2panda operation log,
> HHHS RoomStore, reconciliation tests, and shared identity. The proposed
> browser-owned raw-Iroh transport is replaced by a Tauri-owned native Iroh
> runtime; browser shells become optional clients of that runtime.

## Why

walkie-songie's real-time layer runs on a personal git fork of `rust-libp2p`
(`elijahhampton/rust-libp2p`, a WebRTC feature branch) for transport and `yrs`
(Yjs) for shared state. The fork is a supply-chain and maintenance risk, and the
`yrs` state-CRDT gives no signed provenance for who contributed which note. The
sibling project **potluck** (`../dweb-camp-2026/potluck`) has proven a published,
cohesive stack — **iroh 1.0 + p2panda 0.7 + HHHS** — providing signed per-author
op-logs, deterministic causal reads, and browser/native transport parity.
Adopting it removes the fork, aligns the two projects, and gives every musical
contribution a verifiable author.

## What Changes

- **BREAKING (wire + storage):** Replace libp2p transport with **iroh 1.0**
  **everywhere** (browser + native) + **iroh-gossip 0.101**; NO p2panda-net (Topology A,
  resolved in `transport-design.md`: p2panda-net is native-only and its overlay can't admit a
  browser peer). Old peers cannot talk to new
  peers; the on-disk state format changes. No backward compatibility.
- **BREAKING (data model):** Replace the `yrs` document with **p2panda signed
  operation logs** (`p2panda-core` `Header` + CBOR body), one append-only log per
  author. Room identity becomes a p2panda `Topic` derived from the room name.
- Add an **HHHS causal read-mirror** (vendored `hhhs-core` + `hhhs-reactive`,
  SHA-pinned like potluck) as the materialization layer. The **dominator** UI
  binds directly to its `Signal`/`SignalVec` read streams — `hhhs-reactive` is
  built on `futures-signals`, which is dominator's own reactive engine, so no
  adapter is required.
- **Per-author set semantics:** each author owns their own pitch-class set; the
  displayed shared set is the **union** across authors. A peer can no longer
  toggle off another peer's pitch (intentional change to the collaboration
  model). Voice slots stay per-author; pieces become author-owned.
- **Identity:** one Ed25519 signing key per participant serves as both the
  p2panda author key and the iroh transport identity (replaces the random peer
  UUID).
- Migrate the native VST3/CLAP plugin from the `robbert-vdh/nih-plug` git fork to
  **`nice-plug`** (crates.io), and rewire its networking onto the new
  iroh/p2panda node.
- **Keep unchanged:** SwiftF0/ONNX pitch detection, voice conditioner, Web MIDI
  I/O, tuning/Scala model, solfège, the `<all-around-keyboard>` component,
  CSS/PWA shell.
- Retire the standalone libp2p `relay-server/`; peer discovery/relay moves to
  iroh relays (+ optional rendezvous, per potluck).

## Impact

- **Affected specs:** supersedes the archived `p2p-channels`; adds `p2panda-net`,
  `op-log-state`, `hhhs-reads`; modifies `plugin-peer` (currently a pending
  change).
- **Affected code:**
  - `src/room/**` — rewrite (`yrs_state.rs`, `events.rs`, `streams.rs` → signed
    ops + HHHS mirror)
  - `src/web/libp2p_sync.rs` — delete → new `src/net/`
  - `src/web/app.rs` — rewire local edits → ops, reads ← HHHS streams
  - `src/web/storage.rs` — persist signed source bytes + signing key + log heads
  - `src/web/keyboard.rs`, `src/web/components.rs` — bind to HHHS reads
  - `src/plugin/**` — nice-plug + new networking
  - `Cargo.toml` — swap dependency stack
  - `vendor/hhhs-rs/` — new vendored, SHA-pinned HHHS crates
  - `relay-server/` — removed
- **Dependencies:** *remove* libp2p fork, `yrs`, nih-plug fork. *Add* `iroh 1.0.3`,
  `p2panda-core/net/store/sync 0.7`, vendored `hhhs-core`/`hhhs-reactive 0.1`,
  `nice-plug` stack. Enforce a single `iroh` in the tree (mirror potluck's pins).

## Approval

This is a breaking architecture shift; per `openspec/AGENTS.md` do not begin
implementation until this proposal is reviewed and approved. Note: the `openspec`
CLI is not installed on this machine, so `openspec validate --strict` could not be
run locally — the delta files were authored by hand against the format in
`AGENTS.md`.
