# Change: Move browser replicas to a dedicated worker

## Why

Release measurements show that the keyboard renderer is sub-millisecond while
HHHS admission, materialization, IndexedDB completion, and carrier callbacks
still contend with input and animation on the window thread.

## What Changes

- Put both Room-v5 durable Replica lanes, their materializers, and IndexedDB
  logs behind one `hhhs-web-browser` dedicated-worker service.
- Keep DOM, audio, MIDI, Iroh/WebRTC handles, discovery, and peer presentation
  in the window.
- Cross the boundary with bounded typed commands, projection
  snapshot/revision/reset events, public Replica records, and stepwise repair
  frames.
- Preserve the existing Room-v5 IndexedDB history and capability semantics.

## Impact

- Affected specs: `browser-replica-worker`.
- Affected code: `src/room`, `src/web/replica_host.rs`, `src/web/storage.rs`,
  browser acceptance and performance gates.

## Approval

The active development thread explicitly requested that the worker work land
before 0.5, make replicas easy to place in workers, and improve API boundaries
rather than hide the latency with application hacks. This records that
approval; no release tag is part of this change.
