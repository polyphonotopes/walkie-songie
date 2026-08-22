## 1. Capability-native room core

- [x] 1.1 Define Room v5 namespaces, `ProtocolSupport`, strict command codecs,
  command-derived areas, and proof-bound admission policies.
- [x] 1.2 Define capability bootstrap, delegation, revocation, invitation, and
  local authoring APIs for both lanes.
- [x] 1.3 Define music/extension materializers and composed causal view deltas.
- [x] 1.4 Prove receiver binding, area/right binding, revocation barriers,
  non-retroactivity, lane isolation, and convergence under concurrency.

## 2. Storage and repair

- [x] 2.1 Construct each native lane over `JournalStorage` and prove restart of
  history, local secrets, and validated projection checkpoints.
- [ ] 2.2 Implement the IndexedDB async durable adapter using validated
  prepare → persist → finalize transactions, plus corruption/crash recovery
  gates. IndexedDB must not masquerade as the synchronous `ReplicaStorage`
  boundary.
- [x] 2.3 Adapt walkie's carrier streams to `hhhs_sync::FrameStream` and use
  `ReplicaRepairHost` with the upstream repair driver.
- [ ] 2.4 Prove dropped-broadcast, offline/rejoin, refusal, root mismatch, and
  two-lane loopback repair without cross-lane bytes.

## 3. Live host migration

- [ ] 3.1 Hard-cut ticket, rendezvous, discovery, presence, repair, and storage
  identities to Room v5; rename lane capability bits to protocol support.
- [ ] 3.2 Move the Tauri host to Room v5 author/admit/materialize/subscription
  APIs while retaining app ownership of iroh and task lifecycle.
- [ ] 3.3 Move the browser host to the same APIs while retaining browser-owned
  iroh/WebRTC and IndexedDB scheduling.
- [ ] 3.4 Move the bare music peer to the shared music Replica contract and prove
  it interoperates without linking walkie extensions.

## 4. Remove old abstractions and release

- [ ] 4.1 Delete live p2panda signed-op, `Store<L>`, lane journal,
  application-owned HHHS pump, and courier compatibility paths.
- [ ] 4.2 Remove obsolete dependencies and public exports; keep only bounded
  v4 refusal fixtures, with no runtime fallback.
- [ ] 4.3 Update architecture, user flow, ticket/reset, and contributor docs.
- [ ] 4.4 Pass workspace tests, Clippy, native/desktop/WASM builds,
  browser/browser, browser/native, bare-peer, storage-reopen, and source audits.
