# Change: Adopt capability-native HHHS 0.4 replicas

## Why

Room v4 proved the two-lane product shape, but its implementation exposes a
p2panda-flavoured signed source log, application-owned causal lifting, and a
second repair driver. HHHS 0.4 now provides the intended seam: a storage-aware
`Replica` which owns admission and materialization while leaving every endpoint,
peer set, mesh, and carrier in the application.

## What Changes

- **BREAKING:** introduce Room v5 with new command, entry, repair, ticket,
  discovery, and storage generations. Functional behaviour is preserved; v4
  signed bytes, p2panda headers, hashes, and source-log APIs are not.
- Replace both live `tutti_core::Store<L>` lanes with independent
  capability-configured `hhhs_replica::Replica`s and app-defined admission
  policies.
- Bind every durable command's actor, lane area, and `Invoke` right to a
  verified presentation. Derive authority only from explicitly presented,
  causally live grant paths; protocol-support bits never authorize an action.
- Replace the bespoke RBSR host and pump with `ReplicaRepairHost` and the
  transport-neutral `hhhs-sync` driver. Iroh, WebRTC, IPC, loopback, and future
  carriers remain walkie-owned adapters.
- Persist canonical history, local proof evidence, secrets, and rebuildable
  projection checkpoints through `ReplicaStorage`; compose music and extension
  materializations only in the walkie room host.
- Update the bare music peer to the same music command/policy contract without
  depending on walkie or participating in the extension lane.
- Delete live v4/source-log compatibility shims after native and browser hosts
  move. Retain only bounded refusal fixtures where they test a generation cut.

## Impact

- Affected specs: adds `room-v5-replicas`; supersedes live parts of
  `room-v4-protocol`.
- Affected code: `src/room/**`, `src/net/**`, `src/web/browser_host.rs`,
  `src/web/storage.rs`, `src-tauri/src/lib.rs`, `tests/bare-music-peer/**`, and
  Room v5 integration tests.
- Dependencies: immutable HHHS `v0.4.1` and Tutti `v0.4.2` release tags;
  `p2panda-core` and the old HHHS facade have left walkie's live state path.

## Approval

The active development thread explicitly approved the capability-native Replica
direction, independent per-lane replicas, application-owned iroh/WebRTC/IPC
carriers, a comprehensive HHHS 0.4 migration, and functional rather than byte
compatibility. It also explicitly rejected preserving old abstractions through
conservative compatibility layers. This proposal records that approved scope.

The `openspec` CLI is not installed in this environment. These files are checked
manually against `openspec/AGENTS.md` and must be CLI-validated when available.
