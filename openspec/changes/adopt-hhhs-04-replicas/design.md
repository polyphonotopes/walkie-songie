## Context

Room v4 has the correct product boundary—independent music and extension causal
lanes—but the wrong local composition boundary. Its `Store<L>` owns signed-op
verification and causal lifting, its journals persist those source records, and
walkie implements a parallel HHHS reconciliation host. This duplicates the work
now centralized by HHHS 0.4 and conflates transport feature negotiation with
authority by calling both concepts capabilities.

## Goals / Non-Goals

- Goals: capability-native admission, independent durable lane replicas,
  rebuildable views, bare music participation, app-owned carriers, native/WASM
  storage, offline repair, and an intentionally small high-level room API.
- Non-goals: v4 byte/hash compatibility, online v4 translation, HHHS-owned
  endpoint or mesh actors, peer membership ACLs, hidden authority discovery, or
  using protocol negotiation as authorization.

## Decisions

### Room and lane identity

Room v5 derives one room object plus disjoint music and extension namespaces.
Each namespace owns a `Replica`, capability root, storage instance, repair ALPN,
and projection checkpoint. The room host composes their views; neither Replica
can observe the other lane's frontier.

`ProtocolSupport` is a non-authoritative bitset used only to decide which ALPNs
to attempt. An authenticated ALPN scopes a repair session to one lane. A grant
and verified presentation, not a ticket, hello, peer id, or protocol bit,
authorizes an entry.

### Commands and authority

Each command envelope includes the Room v5 generation, lane namespace, actor
receiver, exact presented grant IDs, and typed command. Its strict canonical codec is the HHHS entry
payload. An admission policy decodes the payload after structural staging and
requires:

1. the command namespace equals the Replica namespace;
2. the command actor equals the verified presentation receiver;
3. the presented authorization has `Invoke` on the exact area derived from the
   command; and
4. command-specific ownership, bounds, and causal rules hold.

Admission evaluates capability liveness at the action's own causal vantage, so
the same historical fact is accepted independently of concurrent arrival order.
Materialization re-evaluates the payload-recorded grant path from the action
vantage through the current frontier: a writer-selected concurrent barrier can
therefore retract an action's effect while every peer retains identical history.

Capability grants and revocations are ordinary lane history. Bootstrap uses an
explicit trusted top-level grant. Delegations and revocations use the same
proof-bound admission path as application commands. No peer/member/organizer
list is an authority oracle.

### Storage and materialization

The native host opens one `JournalStorage` per lane. IndexedDB is asynchronous
and its handles are browser-task-local, so the browser does not pretend it can
implement the synchronous, `Send + Sync` `ReplicaStorage` contract. It owns one
validated portable transaction log per lane and serializes prepare → IndexedDB
persist → Replica finalize. The transaction still spans canonical entries,
local evidence, local secrets, and projection checkpoints. Nothing becomes
visible before IndexedDB's transaction-complete event. The browser host is the
exclusive writer for each lane, so a successfully persisted preparation cannot
be made stale by a concurrent in-memory commit.

Inbound repair crosses the same durability boundary. Its async `RepairHost`
apply step validates incoming Replica records, persists the resulting prepared
transactions, and only then reports them admitted to the sync session. Browser
startup strictly decodes and replays the per-lane logs after reconstructing the
deterministic trusted roots; checksum failure, truncation, sequence mismatch,
or a causal hole refuses room entry rather than silently resetting history.

Music and extension materializers fold immutable snapshots into versioned
checkpoints. Checkpoints are validated against their exact history root and can
always be discarded and rebuilt. The composed `RoomView` is app state, not HHHS
state and not a cross-lane canonical fact.

### Networking and repair

`ReplicaRepairHost` adapts a lane Replica to `hhhs_sync::RepairHost`.
`hhhs_sync::{drive_initiator, drive_responder}` drives any walkie-provided
`FrameStream`; iroh QUIC, browser WebRTC custom transport, loopback, threads,
pipes, and IPC are carrier choices below that boundary. Broadcast delivery is
an optional low-latency hint and carries opaque Replica repair records or a
bounded one-entry admission frame; convergence relies on repair.

HHHS owns no endpoint, discovery, rendezvous, relay selection, peer lifecycle,
transport path, room task, or UI subscription.

### Bare music peer

The music command codec and admission policy live in a transport-free music
protocol crate. Walkie and the bare peer instantiate it with their own storage
and carrier. The bare peer advertises only music support and never links the
walkie extension protocol. Functional interop means equal admitted command
history and music view, not v4 source bytes or identities.

## Risks / Trade-offs

- A capability grant must reach a participant before their commands can be
  admitted. Tickets therefore carry bootstrap location and invitation material,
  but never turn connectivity metadata into ambient authority.
- Two independent lane commits are not atomic together. Commands target one
  lane; room composition tolerates either lane advancing first.
- Full snapshot recapture in the initial `ReplicaRepairHost` may be expensive.
  Correctness lands first; incremental recapture is measured and optimized in
  HHHS without changing the app/carrier seam.
- Room v5 deliberately strands v4 rooms. There is no deployed history requiring
  an online migration, and a clean generation boundary is safer than dual
  admission.

## Migration Plan

1. Add Room v5 command codecs, capability policies, materializers, and an
   in-memory two-Replica host with restart and repair tests.
2. Adapt walkie's generic frame streams to the HHHS driver and prove loopback
   two-lane repair plus music-only isolation.
3. Add native `ReplicaStorage` construction and the IndexedDB async durability
   owner, then move live hosts to v5 tickets, ALPNs, gossip admission,
   subscriptions, and composed views.
4. Move the bare music peer and cross-runtime gates to v5.
5. Delete live `Store<L>`, p2panda source-log, old sync/courier, and v4 journal
   paths; retain only explicit refusal fixtures.
6. Run native, desktop, WASM, browser/native, browser/browser, formatting,
   Clippy, dependency, and source-audit release gates.
