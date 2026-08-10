## Context

`WalkieOp::Music(MusicOp)` would change music signatures and entry identities.
A combined DAG would also let a music op observe an extension predecessor that
a bare music peer cannot decode. The detailed rationale and exact identity
matrix live in `docs/vision/wire-embedding-design.md`.

## Goals / Non-Goals

- Goals: one shared v4 room topic, two independent causal stores, exact
  `MusicLang` interoperability, strict durable recovery, and identical behavior
  in native and browser runtimes.
- Non-goals: v3/v4 interoperation, an online history translator, physical-device
  testing, deployment, pushing, or merging.

## Decisions

- A full walkie peer owns `Store<MusicLang>` and `Store<ExtensionLang>` and
  composes them only at read time.
- Music and extension repair/courier traffic use distinct ALPNs and distinct
  QUIC connections. No lane byte rides inside RBSR or courier frames.
- Gossip carries both lane frame magics plus v4 presence on one shared topic.
  Ingress routes by exact magic, verifies under that lane language and topic,
  persists the original bytes, then ingests.
- Native files and IndexedDB use disjoint v4 journal markers and lane-tagged
  records. A complete corrupt record fails loudly; only a torn final record may
  truncate to its complete prefix.
- Local commits are two phase: prepare exact lane wire, durably append, ingest,
  then broadcast. Persistence failure cannot make an op visible.
- Tickets and rendezvous advertise `LaneSet`; ALPN negotiation remains the
  authoritative capability check. A full peer requires both advertised lanes
  before reporting synchronization.
- Browser and native use the same protocol constants, sync drivers, admission
  path, and identity codecs. Browser-specific code supplies only scheduling and
  IndexedDB/endpoint adapters.

## Risks / Trade-offs

- Concurrent lane sessions share one composed room behind a lock/borrow seam.
  Capture and apply operations are short; no network await may hold that seam.
- Browser IndexedDB stores one journal blob, so each accepted append rewrites
  the bounded blob. Correctness and atomic replacement take priority; storage
  chunking is a later performance change if measurement earns it.
- A dishonest capability advertisement can cause failed dials but cannot cross
  a lane boundary because QUIC ALPN negotiation and lane verification fail
  closed.

## Migration Plan

1. Land and validate the Room v4 core and native runtime.
2. Move browser persistence, commands, gossip, presence, and recovery to v4.
3. Move the browser endpoint and discovery suite to the exact v4 identities.
4. Add cross-runtime automated gates and remove every live v3 call site.
5. Run native, desktop, wasm, and production bundle gates.

There is no rollback path inside a room. Reverting the application returns to a
separate v3 generation and cannot open v4 artifacts.
