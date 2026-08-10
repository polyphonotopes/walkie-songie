## 1. Room v4 core and native runtime

- [x] 1.1 Define independent music/extension stores, lane identities, composed
  reads, and two-phase local commits.
- [x] 1.2 Define strict native and IndexedDB v4 lane journals.
- [x] 1.3 Define v4 topics, tickets, rendezvous, mDNS, presence, repair, and
  courier identities.
- [x] 1.4 Hard-cut the native endpoint and Tauri room runtime.
- [x] 1.5 Prove native two-lane repair, v3 refusal, dropped-gossip recovery,
  lane isolation, convergence, and journal reopen.

## 2. Browser runtime hard cut

- [x] 2.1 Recover `Room` from the v4 IndexedDB journal and fail corruption
  loudly without reading the v3 key.
- [x] 2.2 Map browser commands to `LocalRoomOp`, persist-before-ingest, compose
  `RoomView`, and broadcast exact lane bytes.
- [x] 2.3 Route browser gossip to music, extension, or v4 presence and use the
  common durable lane admission seam.
- [x] 2.4 Register exactly the five v4 endpoint ALPNs and use v4 tickets,
  rendezvous hellos, and topic identity.
- [x] 2.5 Drive both advertised/negotiated repair lanes concurrently on
  separate connections and dispatch lane-specific courier connections.
- [x] 2.6 Delete every live browser-v3 room path and compatibility fallback.

## 3. Automated release evidence

- [x] 3.1 Prove browser reload and complete-record corruption refusal using the
  real storage adapter.
- [x] 3.2 Prove browser/browser and browser/native two-lane convergence,
  dropped-gossip recovery, and zero cross-lane bytes.
- [x] 3.3 Prove a bare `tutti-music` peer joins only the shared music lane.
- [x] 3.4 Run native, desktop, wasm, production Trunk, formatting, and source
  audits with no live v3 paths.
- [x] 3.5 Update the protocol design and task evidence, then commit each
  validated milestone without pushing or merging.
