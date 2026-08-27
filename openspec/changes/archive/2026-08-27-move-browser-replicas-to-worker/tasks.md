## 1. Executable boundary

- [x] 1.1 Define bounded, versioned Room worker payloads and projections.
- [x] 1.2 Implement one service owning both durable lane hosts.
- [x] 1.3 Implement frame-stepped repair and in-process convergence tests.

## 2. Browser placement

- [x] 2.1 Start the service in a module worker without mounting the UI there.
- [x] 2.2 Make IndexedDB recovery work in window and worker globals.
- [x] 2.3 Route window commands, live records, grants, presence, and repair.
- [x] 2.4 Remove the superseded main-thread durable path.

## 3. Ergonomics

- [x] 3.1 Add a typed handle with projection-fenced durable commits.
- [x] 3.2 Expose exact projection and lifecycle state as composable signals.
- [x] 3.3 Preserve explicit raw carrier/repair methods below the convenient API.
- [x] 3.4 Keep the typed Rust handle as the primary proxy; reserve a JavaScript `Proxy` adapter for an actual JavaScript consumer API.

## 4. Acceptance

- [x] 4.1 Prove persistence, worker reopen, two-tab sync, and reconnect.
- [x] 4.2 Measure intent, projection, visibility, peer, and repair latency, separating warmup.
- [x] 4.3 Pass the full native, Wasm, release-browser, and source-audit gates.
- [x] 4.4 Keep the reviewed change set tag-free.
