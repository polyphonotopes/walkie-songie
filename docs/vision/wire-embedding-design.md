# Room v4: letting a bare tutti-music peer join a walkie room

Design notes for the WalkieOp ⊇ MusicOp goal, after a codex (gpt-5.6-sol) review.
The short version: the naive `WalkieOp::Music(MusicOp)` variant does **not** get us
what we want, and the real thing is a room protocol generation, not a schema bump.

## Why the naive embedding fails

Wrapping the music op-language in a walkie variant —

    WalkieOp::Music(MusicOp) | Piece(..) | Config(..)

— makes a walkie op, not a music op. A bare ESP32 peer running only `MusicLang`
signs a `VersionedOpG<MusicLang>` carrying a `MusicOp`; walkie expects a
`VersionedOpG<WalkieLang>` carrying the wrapper. Different entry/wire magics
(walkie `src/room/ops.rs:96` vs tutti-music `lang.rs:51`), an extra CBOR
discriminator, different signed bytes → different `OpId`/`EntryHash`. Shared fold
combinators don't help: shared folds ≠ shared signed identity.

Worse, the store stamps the **whole** store frontier into every new op
(`tutti-core/store.rs:401,417`). In one combined DAG, a walkie music op would
observe `Piece` ops as predecessors; a music-only peer can't decode those, and
strict deferral (`store.rs:332`) then blocks the music op from lifting. One DAG
silently strands the music-only peer.

## The design: two causal lanes in a v4 room

- **Music lane** — literally `Store<MusicLang>`: MusicLang bytes, its own head,
  frontier, framing, and the 64 KiB payload cap (`lang.rs:62`). An ESP32 joins
  *only* this lane and is a first-class peer in it.
- **Walkie extension lane** — pieces + config only.
- Walkie composes both views; the ESP32 sees just the music view.

Separate frontiers are the load-bearing part: a music op must never backlink or
observe a walkie-extension op, so a MusicLang-only peer never meets a predecessor
it can't decode.

Alternative if one DAG is ever mandatory: a shared carrier with opaque-extension
support (verify/hash/retain/relay unknown ops without decoding). That's a bigger
build and is no longer "an ESP32 running today's MusicLang."

## Migration: hard cut, new protocol generation

~0 rooms are deployed, so no live translating shim. Treat v4 as a whole protocol
suite, not just `OP_SCHEMA_VERSION = 4`:

- repair ALPN `walkie/rbsr/2` → `/3` (`src/net/iroh_common.rs:46`) + strategy gen.
- version the gossip topic derivation, or v3/v4 peers share a topic
  (`iroh_common.rs:22,75`).
- new room-ticket format carrying the protocol generation (`iroh_common.rs:24,308`).
- journal magic `/3` → `/4` (`src/room/journal.rs:13`).
- fresh author heads; never backlink v4 → v3.

Mixed v3/v4 in one room fails loudly and permanently (mutual verifier rejects,
RBSR roots never converge, parked ops). At most, ship an offline "open projected
v3 state as a new v4 room" importer — a reset, not interop.

## Golden re-baseline

Old entry golden `9e2179…3568` is pinned in `src/room/store.rs:1123` and
`tests/l0_convergence.rs:14`; `ops_root`/`state_root` in `store.rs:1221`.

1. Keep the old values as named v3 fixtures.
2. Pin separately: CBOR payload, signed header/`OpId`, wire frame, predecessor
   hashes, lifted `EntryHash`.
3. Add the decisive vector: one fixed `MusicLang` `SignedOp` verifies and lifts
   through both the bare music store and walkie's music lane to the **same**
   `OpId` and `EntryHash`.
4. `ops_root` changes when signed bytes change; `state_root` changes only if the
   canonical projected state changes (adding envelopes to committed state is an
   explicit state-root schema change — walkie's `RoomView` has no envelopes today,
   `store.rs:263`).

## Identity rules (do not violate)

- `TunedDegree` = `TuningId + ScaleDegree` is the whole key (`tuning/mod.rs:240`).
  Never translate through a bare degree index or the receiver's current tuning.
- Preserve inactive-tuning ops (MusicLang hides them under another tuning and
  resurrects on switch-back, `lang.rs:13`).
- **Keep `b"walkie-songie/tuning\0"` pinned forever** (`tuning/mod.rs:21`) —
  changing it silently rekeys every tuning-scoped degree. (So: do *not* do the
  "rename the tuning magic" follow-up. It was a mistake to list it.)
- A received ESP32 op is stored and relayed as its original bytes — never
  reserialized or re-wrapped after signature verification.
- Enforce the music lane's 64 KiB cap, not walkie's larger allowance.
