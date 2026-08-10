# Room v4: letting a bare tutti-music peer join a walkie room

Design notes for the WalkieOp ⊇ MusicOp goal. The short version: the naive
`WalkieOp::Music(MusicOp)` variant does **not** get us what we want, and the real
thing is a room protocol generation, not a schema bump.

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

~0 rooms are deployed, so there is no live translating shim. v4 is a complete,
mutually incompatible protocol suite, not an `OP_SCHEMA_VERSION` bump:

| Surface | Room v4 identity |
|---|---|
| Shared room topic | `blake3::derive_key("walkie-songie room topic v4", ascii_lowercase(room_name))` |
| Endpoint ALPNs | gossip, music repair, extension repair, music courier, extension courier |
| Music repair | `tutti/music/rbsr/3` |
| Extension repair | `walkie/extension/rbsr/3` |
| Music courier | `tutti/music/courier/1` |
| Extension courier | `walkie/extension/courier/1` |
| Ticket | kind `walkieroom4`; format 2, generation 4, lane bits, topic, endpoint |
| Rendezvous | v4 channel and `HelloV4` with required lane bits |
| mDNS | room-scoped service name ending in `-v4` |
| Native journal | `walkie-songie/op-journal/4\n`; every record carries its lane byte |
| IndexedDB journal | `walkie-songie/idb-op-journal/4\0`; every record carries its lane byte |
| Presence | signed presence generation 4 on the shared v4 topic |

The native endpoint registers exactly the five v4 ALPNs above and refuses the
v3 repair ALPN. Each repair or courier connection is scoped to one lane by its
authenticated ALPN; no lane tag is added to an RBSR frame. Walkie advertises
both lane bits (`0x03`); a bare music peer advertises only music (`0x01`). Fresh
lane stores mean fresh author heads: no v4 op ever backlinks to a v3 entry.

The browser host temporarily retains the v3 runtime until its live transport and
IndexedDB cutover. That is a separate implementation boundary, not v3/v4 room
interop.

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
