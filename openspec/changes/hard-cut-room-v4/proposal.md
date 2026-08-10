# Change: Hard-cut every room runtime to Room v4

## Why

A bare `tutti-music` peer cannot participate in the v3 `WalkieLang` DAG without
changing its signed bytes or encountering extension predecessors it cannot
decode. Room v4 solves that by giving music and walkie extensions independent
causal lanes while retaining one composed room view.

## What Changes

- **BREAKING:** replace every live native and browser v3 room path with the
  Room v4 identity suite; there is no translating shim or fallback.
- Persist and reconcile the music and extension lanes independently while
  gossiping their exact signed wire bytes on one shared v4 topic.
- Advertise lane capabilities in v4 tickets and rendezvous hellos; treat the
  negotiated ALPN as authoritative.
- Keep voice presence outside both durable lanes under its own v4 signed codec.
- Prove browser/browser, browser/native, and bare-music interoperability with
  automated reload, corruption, dropped-gossip, convergence, and lane-isolation
  gates.

## Impact

- Affected specs: new `room-v4-protocol` capability.
- Affected code: `src/room/v4.rs`, `src/room/lane_journal.rs`, `src/net/**`,
  `src/web/browser_host.rs`, `src/web/storage.rs`, and `src-tauri/src/lib.rs`.
- Existing v3 rooms and tickets are rejected. With no deployed room history,
  migration is a clean reset rather than online interoperation.

## Approval

The hard-cut design and the native/browser completion goal were explicitly
approved in the active development thread. Physical-hardware testing,
deployment, pushing, and merging are outside this change.

The `openspec` CLI is not installed in this environment, so strict CLI
validation cannot be run locally; these files follow `openspec/AGENTS.md` and
must be validated when the tool is available.
