# Co-review questions for potluck — before any hhhs crate-boundary freeze

Context: we're considering factoring `hhhs-core` into `hhhs-dag` (contract floor) /
`hhhs` (facts front door) / `hhhs-sync` (reconciliation engine) / `hhhs-testkit`,
with `hhhs-core` kept as a deprecated path-stable re-export shim. Full design:
`docs/vision/hhhs-ecosystem-design.md`. An import audit across the hhhs-rs workspace
(`hhhs-datalog`, `hhhs-strategy-toyfacet/riffcat`, `hhhs-reactive`) plus walkie
(via tutti-core) shows only walkie's `src/net/sync.rs` touches the reconciliation
engine — everyone else uses the facts surface. Three things need potluck's answer
before we freeze anything.

## 1. Do you ever drive hhhs's `SyncSession` / `reconciliation`?
`sync_session.rs:6-8` claims both walkie and potluck drive it, but potluck's tree has
no such import — potluck's anti-entropy looks like its own inventory protocol in
`potluck-wire`. **If potluck never drives `hhhs-sync`,** it becomes a leaf crate with a
soft freeze (walkie is its only consumer, evolve freely). **If potluck does or plans
to,** its surface must freeze harder and co-evolve. Which is it?

## 2. Can we agree the `hhhs` front-door export list before it freezes?
Potluck `pub use`s kernel `void` types into its *own* public API, so the exact set the
`hhhs` facts crate re-exports (`cover` / `register` / `canonical_index` / `lens` /
`graph` / `query` / `void`) becomes part of potluck's public surface too. Let's fix
that list together rather than discover a mismatch at re-pin.

## 3. `DagRead`-family trait evolution + testkit relocation — OK?
Proposal: the `DagRead` / `DagStore` / `Growth` / `DagDelta` family evolves
**default-methods-only** (additive, so a new method never breaks an existing impl), and
the conformance / `test_utils` suite moves into an `hhhs-testkit` crate. The latter
touches potluck's CI. Are you good with that policy + the CI change?

---
Bottom line: the substrate is already generic across real consumers at the **hhhs**
layer (music via walkie, datalog, strategy crates, reactive) — the open coordination
is which layer potluck actually binds to (facts only, or facts + engine), which #1
settles.
