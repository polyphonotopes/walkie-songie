## Context

The current browser host combines an authoritative data plane with a
window-only network session. A dedicated worker can use IndexedDB and HHHS, but
the supported browser carrier uses window-owned WebRTC objects.

## Goals / Non-Goals

- Goals: remove durable Replica work from the UI thread; retain exact
  snapshot/revision/reset continuity; retain persistence, live delivery, and
  full repair; measure the result in a production build.
- Non-goals: move WebRTC into a worker, add a second authoritative replica,
  change Room-v5 wire identity, or introduce speculative realtime authority.

## Decisions

- One worker owns both independent lanes. Per-replica workers remain a
  placement option, not a different protocol.
- The window owns session and carrier effects. The worker emits/accepts opaque
  public records and advances HHHS repair one frame at a time, so it never owns
  a socket and never blocks its request loop waiting on network input.
- Projection consumers receive an initial snapshot followed by exact revisions;
  lag produces a reset snapshot.
- The typed window handle exposes current projection/lifecycle as signals and a
  durable `commit` which resolves only after the named projection revision is
  observed. Raw request/repair operations remain explicitly async and fallible.
- A JavaScript `Proxy`, if added, is convenience syntax over that contract. It
  cannot turn remote operations into synchronous property access or hide
  backpressure, restart, or projection fences.
- Existing Room-v5 IndexedDB rows remain the recovery source during migration.

## Risks / Trade-offs

- The same Wasm module initially boots in both realms, increasing startup and
  memory. A worker-only bundle can follow after the placement contract is
  proven.
- Boundary serialization is measurable overhead. Payloads remain bounded and
  transferable; browser tests report it separately from durable work.

## Migration Plan

1. Prove the typed service and repair stepper with the in-process worker host.
2. Start the service in a real module worker and retain existing persistence.
3. Route commands, live records, and repair through the worker.
4. Delete the main-thread durable host only after restart and reconnect gates
   pass.
