# Rollback semantics for the reactive HHHS projection

Status: design review, 2026-08-07. Grounded in walkie-songie source at HEAD and
`hhhs-core` / `hhhs-reactive` at rev `bd23d4e` (checkout under
`~/.cargo/git/checkouts/hhhs-rs-cf27a40398583f68/bd23d4e/`; kernel refs below are
relative to that root). Companion designs, referenced rather than restated:

- `docs/research/ui-state-coupling-design.md` (in flight) — the invariant "the
  signal UI is a *direct projection* of data" and intent-over-involution (taps
  carry absolute idempotent intent, not a raw toggle). This doc supplies the
  rollback semantics for the intents that design defines.
- `docs/research/peer-discovery-design.md` — transport/discovery, context only.

The question under design: *"a nice (reactive-friendly) API that deals with the
semantics of rollbacks."* The answer is tiered. **Tier 0**: on this substrate
almost every rollback is *invisible* — the op-set changes, the projection
recomputes, the UI re-renders, and no rollback-specific code exists. **Tier 1**
is an opt-in intent-lifecycle layer for the minority of flows that genuinely
need durational rollback UX (conflict prompts, "reverted N minutes ago",
undo history, drafts with grace periods). Escalation criteria in §5.1.

---

## 1. Ground truth: what "rollback" can mean here at all

### 1.1 Every layer of the stack is grow-only

There is no operation anywhere in the system that removes an op. Verified
layer by layer:

| Layer | Evidence |
|---|---|
| Signed op log | Per-author append-only log; evolution discipline is append-variants-only (`src/room/ops.rs:3-9`, `27-29`) |
| DAG store | `DagStore` has exactly `append` + `missing_prevs`; `AppendOutcome` = `Appended \| Duplicate \| MissingPrevs \| BadDigest` — the *entire* admission vocabulary. No remove/prune/compact on any store trait (`hhhs-core/src/dag.rs:258-266`, `350-362`) |
| RoomStore | `ingest_verified` dedups, parks, drains — never deletes (`src/room/store.rs:247-258`, `319-339`); missing prevs are *deferred, never rejected* (`store.rs:275-284`; `hhhs-core/src/dag.rs:354-356`) |
| Journal | Append-only file; the only truncation is torn-tail recovery on open (`src/room/journal.rs:28-33`, `120-133`) |
| Gossip ingest | Verify → ingest → re-fold; rejection means the bytes never enter the store (`src/web/browser_host.rs:607-624`) |
| Anti-entropy | RBSR repair only *adds*: `SyncStoreAccess::apply` verifies and ingests, no branch removes (`src/web/browser_host.rs:1211-1260`); kernel-side `apply_staged` likewise (`hhhs-core/src/replica.rs:348-383`). The harness ground truth is **converge to the union of the two initial sets** (`hhhs-core/tests/harness/invariants.rs:8-11`, `71-73`) |
| Divergence | `SessionStatus::Divergent` means "closed; the periodic anti-entropy re-syncs" — another additive union pass, *never* an error and never a revert (`src/net/sync.rs:29-33`, `700-707`; `hhhs-core/src/sync_session.rs:194-206`) |

So the direct answer to "is there any window where local state must be
un-applied after reconciliation?" is **no — structurally no**, at the data
layer. Reconciliation can only make the projection *move* by adding history
(§3). The only un-apply windows in the tree today are UI-side optimistic
mutations (§2, R3′), which the coupling design eliminates.

### 1.2 The kernel has already made this call — and named the taxonomy

`hhhs-core/src/rollback.rs:1-10` (module doc):

> "Rollback" is one word for three different situations, and picking the wrong
> one is a correctness bug rather than a style choice. … A published entry is
> in peers' logs already: **there is no mechanism to un-send it, and any API
> that appears to offer one is lying.** Published state is undone by *adding*
> history that supersedes it.

The three primitives (`rollback.rs:16-32`), keyed on the **publication
boundary**:

- **`AbandonStaged`** — staged and unpublished: drop it; nothing observed it.
  The staging vehicle is `StagedDag` (`hhhs-core/src/staged.rs:23`): an
  immutable snapshot base plus an unpublished extension, with
  `stage`/`abandon`/`into_staged` (`staged.rs:40-54`, `62-65`, `68-70`) —
  begin/commit/abort in all but name, purely local, O(extension) not
  O(history).
- **`Retract`** — published: append a retraction; reads stop counting the
  target while it stays in history as evidence.
- **`Compensate`** — published, not retractable in isolation: supersede with a
  compensating entry (value-shaped effects, e.g. a register write undone by
  another write).

`rollback_for(store, staged, entry, retractable)` (`rollback.rs:39-55`)
computes the discriminator; `retractable` is caller-supplied because
retractability is a property of the payload language, not the DAG
(`rollback.rs:36-38`). The one hazard is pinned by test: an entry that is both
staged *and* published is treated as **published** — "if a stage list is stale
and the entry landed meanwhile, abandoning would silently diverge from peers
who already hold it" (`hhhs-core/tests/rollback.rs:45-57`). Potluck reached the
identical doctrine independently: *"Rollback expressed properly: tombstone,
compensating op, round boundary. Branch-abandon is the wrong primitive for a
published append-only log"* (`/laboratory/dweb-camp-2026/potluck/GOAL.md:417-418`).

### 1.3 What a "cotransaction" actually is

The Rust kernel has **no transaction or cotransaction API** (grep across the
workspace: zero hits; `STATUS.md:69` lists transactions as deliberately not
implemented; `docs/potluck-wishlist.md:673-674` records the `Transactions` row
as "none"). The term comes from the HHS3 TypeScript line —
`/laboratory/fe-stuff/hhs3-ts/modules/rdb/README.md:5-11`:

> A row operation … is validated optimistically against the local replica …
> Valid operations apply immediately, with no coordination. When a peer's
> updates arrive, each operation is re-checked … at the version where the
> operation was authored, now seeing everything concurrent that is visible
> from the frontier. Whatever breaks the schema in that view is discarded …
> Every honest replica reaches the same verdict.

Two properties matter for this design:

1. A co-transaction's "abort" is a **deterministic, retroactive verdict
   change**: the op stays in the log; concurrent history flips it to inert.
   That is *exactly* walkie's existing fold semantics (owner-gating, add-wins,
   causal-maxima registers) re-evaluated per growth — R1 in §2.
2. Atomicity is achieved by making the multi-part write **one operation**
   ("a multi-table write is one atomic operation",
   `hhs3-ts/modules/rdb/README.md:23-27`) — not by a wire-level commit
   protocol. §6 adopts this.

So "mapping intents onto cotransactions" (the coupling design) costs nothing
new at this layer: an intent that becomes one signed op *is* a co-transaction
— optimistically effective locally, verdict re-computed deterministically as
history arrives, atomic because it is one entry.

---

## 2. The rollback taxonomy in walkie-songie

Five real cases, each already latent in the code:

**R1 — Reconciliation supersession** (a peer's history outranks yours). The
fold's three conflict regimes each define a way an op you committed stops
having effect:

- *Causal remove*: an `AddDegree` is dead iff a same-key `RemoveDegree`
  causally observed it; a concurrent add survives (`src/room/store.rs:414-452`,
  add-wins via `ReachIndex::is_ancestor` at `store.rs:439-441`; kernel
  rationale `hhhs-core/src/cover.rs:24-26`). Proven:
  `concurrent_add_remove` (`store.rs:688-715`),
  `concurrent_remove_does_not_kill_add` (`store.rs:897-923`).
- *Register loss*: `SetTuning`/`SetConfig` resolve by causal maxima then max
  raw-bytes entry hash (`store.rs:553-605`; `hhhs-core/src/register.rs:1-24`).
  Your write can lose to a concurrent one, or an entire class of degree ops
  can be *eclipsed* by a tuning change — old contributions "hidden without
  being reinterpreted" (`store.rs:926-952` test; also
  `openspec/changes/pivot-to-tauri-iroh/design.md:126-129`).
- *Authority gate*: a non-owner's `MovePiece`/`RemovePiece` is stored but
  never takes effect (`store.rs:493-533`, test `store.rs:986-1015`). This is
  the fold-level instance of the kernel's blessed pattern: "unauthorised
  operations remain durable DAG entries, but never become negative
  dependencies" (`hhhs-slice-tests/tests/authority_retraction.rs:1-7`,
  `80-131`).

In every sub-case the local op is never un-applied — the *projection* moves.

**R2 — User-initiated undo/revert.** Already in the alphabet as new signed
ops: `UnremovePiece` is "a remove-of-remove; resurrects the piece"
(`src/room/ops.rs:129-132`, fold at `store.rs:509-527`, test
`store.rs:954-984`); re-adding a removed degree resurrects it
(`retract_then_recreate_is_live`, `store.rs:863-895`). Resurrection-by-
remove-of-remove is native kernel behavior (`hhhs-core/src/void.rs:276-288`).
What's missing is only the *general* mechanism: an inverse function from
intent to compensating op, and a history surface (§6, §7 step 4).

**R3 — Rejected/invalid op.** Validation is strictly **before** insertion:
command validation (`src/web/browser_host.rs:193-273`), domain/wire caps
(`src/room/ops.rs:145-190`), signature/structure at every ingress
(`ops.rs:537-592`). A rejected op never enters the store, so nothing needs
reverting; the Tauri spec makes this normative — "the command returns a
structured error and emits no partial state mutation"
(`openspec/changes/pivot-to-tauri-iroh/specs/desktop-run/spec.md:26-38`).
Post-insertion rejection does not exist (`dag.rs` admission vocabulary,
§1.1); its place is taken by *semantic inertness* (R1's authority gate).

**R3′ — Optimistic-apply rollback: the class pure projection deletes.** Today
there are exactly two optimistic UI mutations: the keyclick handler mutates
`state.room` *before* dispatching (`src/web/keyboard.rs:1390-1399` —
`room.toggle_pitch(pc)` then `ToggleDegree`), and the emoji drop does a
legacy-mode `add_piece` ("Optimistic legacy view; the native snapshot replaces
it once signed", `keyboard.rs:1186-1189`). Their "rollback" is a silent
snap-back when `replace_native_projection` clears and rewrites the view model
(`src/web/app.rs:459-465` → `src/room/yrs_state.rs:277-352`). Under the
coupling design both sites become intent submissions, and — because a local
commit is *synchronous and real* (`commit_room_op` signs, ingests, and
re-projects in one call, `src/web/browser_host.rs:1299-1324`) — there is no
optimistic window left to roll back. **Pure projection eliminates this whole
class rather than moving it.** What remains of "pending" is not a UI guess
but genuine distributed facts: was the signed op durably journaled
(`prepare_commit` two-phase seam, `store.rs:352-374`), did broadcast succeed
(the `gossip_broadcast` "committed locally but broadcast failed" diagnostic,
`browser_host.rs:1313-1319`), does any peer provably hold it. Those are Tier 1
material (§5), and potluck's tri-state is the right model: an unacknowledged
write is **unresolved**, "rather than submitted or definitively rejected until
exact source bytes or the remote author head prove the outcome"
(`potluck/STATELESS-TABLES-HHHS.md:433-436`, `516-519`).

**R4 — Cotransaction abort.** Two positions relative to the publication
boundary (§1.2): *before* publish, a multi-op intent staged on a
`StagedDag`-style overlay aborts for free (`AbandonStaged`); *after* publish,
it rolls back by one compensating op. Atomicity design in §6.

**R5 — Durational / leased state.** Already shipped for voice: presence is
"signed, sequenced, leased" and deliberately outside durable history so "a
crash or dropped clear frame expires locally instead of leaving a permanent
sounding note" (`src/room/presence.rs:1-5`, lease constants `presence.rs:16-17`).
Lease expiry is a time-based revert that reconciles trivially because nothing
durable ever existed. This is the template for tentative-for-a-window UX
(§5.3) — prefer *leases and drafts* over publish-then-auto-retract.

---

## 3. Tier 0 (the default): rollback is a re-projection, and that's the API

The baseline requires almost nothing to be built, because it falls out of the
coupling design's invariant. With UI = pure projection of the op-set:

- a peer's superseding op arrives (R1) → `ingest_verified` → `view()` re-fold
  (`store.rs:392-408`) → the keyboard key un-highlights;
- your own undo op commits (R2) → same path;
- a repair session backfills missed history → same path
  (`browser_host.rs:1250-1255`);
- a cotransaction verdict flips (R4-as-R1) → same path.

One code path, zero rollback-specific branches, deterministic on every peer
(out-of-order convergence proven across permutations,
`store.rs:1107-1138`). At the signal level this is the hhhs-reactive
discipline: a view is a pure function `(store, frontier) → keyed rows`;
growth triggers a coalesced full re-fold and a key-wise diff
(`hhhs-reactive/src/lib.rs:17-20`, `80-103`, `197-233`), delivered as
`Revision{added, retracted, at}` (`lib.rs:63-67`) or a `SignalVec`
(`lib.rs:245-265`). `retracted` even carries the previous row value
(`lib.rs:60-61`), so Tier 0 covers "render what left" (note-offs, exit
animations) without any extra machinery — the planned MIDI re-sourcing
("MIDI deltas re-source from `Revision` added/retracted",
`openspec/changes/rewrite-p2panda-hhhs-stack/tasks.md:52-54`) is a Tier 0
consumer.

Tier 0 is also the correct *product* default for this app: walkie is a jam
space. A pitch toggled off by a peer is not an error condition to be
explained — it is the music changing. Most rollbacks should be exactly as
ceremonial as someone else releasing a key.

What Tier 0 deliberately does not carry is **provenance**. The current delta
seam demonstrates the loss: `AppEvent::DegreeRemoved { pitch }`
(`src/client.rs:199-201`) is emitted identically whether *you* removed it, a
peer superseded you, or a tuning change eclipsed the degree
(`browser_host.rs:938-965`) — by the time it reaches the UI, the *why* is
gone. That is fine until a flow needs the why. Then, and only then, escalate.

---

## 4. Interlude: the current pipeline launders rollbacks (and must shrink)

Today a fold result crosses **four** re-encodings before a signal fires:
`RoomView` → diff against `AppSnapshot` emitting delta `AppEvent`s
(`browser_host.rs:923-1000`) → deltas re-applied to a mirrored snapshot
(`src/web/app.rs:334-356`, `1049-1136`) → snapshot projected into the legacy
yrs `RoomState` inside `state.room: Mutable<RoomState>` (`app.rs:77`,
`358-467`) → yrs-doc diff re-emits `RoomEvent`s for the signal layer
(`yrs_state.rs:343-350`, `src/room/streams.rs:275-326`). Each hop re-derives
diffs from state that already was a diff, and each is a place where a
rollback's context dies. The coupling design collapses this to
`RoomStore → hhhs-reactive views → dominator`
(`openspec/changes/rewrite-p2panda-hhhs-stack/proposal.md:28-35`); this doc
assumes that collapse and adds one thing to it: the fold should expose
**per-op effect attribution** (§5.2) so Tier 1 can be built as *another
projection* instead of another pipeline.

---

## 5. Tier 1 (opt-in): the intent lifecycle layer

### 5.1 When to escalate

Stay in Tier 0 unless the interaction meets at least one criterion:

| Escalate when… | Example |
|---|---|
| The user must be *told* about a supersession, not just shown the result | "Your tuning was replaced by Ada's" toast; "reverted N minutes ago" |
| The flow requires a *decision* | conflict prompt: reapply / discard / merge |
| The effect is tentative over a **duration** | drag-preview a piece for 3 s before it commits; a draft chord held until confirmed |
| The user navigates **history** | undo/redo surface, session-scoped revert |
| Distributed acknowledgment matters | "no peer has your last 4 ops yet" indicator, potluck's unresolved tri-state |

Everything else — the overwhelming majority of taps — is Tier 0.

### 5.2 Foundation: effect attribution in the fold

The single mechanism Tier 1 needs from the data layer: `view()` already
*computes* per-op liveness (which adds were killed and by what,
`store.rs:437-443`; which lifecycle op won a piece, `store.rs:505-527`; which
register write won, `store.rs:579-604`) and then throws that information away,
keeping only the surviving rows. Extend the same fold pass to also emit an
attribution map:

```rust
/// Computed in the SAME pass as RoomView — never a second source of truth.
pub struct EffectMap(BTreeMap<OpId, Effect>);

pub enum Effect {
    Effective,                          // the fold currently gives this op effect
    Superseded(Supersession),           // stored, verdict: no effect
    Inert,                              // structurally effectless (e.g. non-owner op)
}

pub struct Supersession {
    pub by: Vec<OpId>,                  // the winning op(s)
    pub authors: BTreeSet<AuthorId>,    // who superseded
    pub kind: SupersessionKind,         // CausalRemove | RegisterLoss |
                                        // AuthorityGate | TuningEclipse | Retracted
}
```

This is deterministic (a pure function of the op-set, testable against the
existing oracle exactly like `view()` — `store.rs` parity tests,
`integration-tests.md:59-61`) and honors the kernel's **no-verdict-cache
doctrine**: it is recomputed per growth, never memoized
(`hhhs-core/src/void.rs:21-32`, `register.rs:26-37`, `cover.rs:15-17`; if
recompute cost ever bites, follow the advisory discipline of
`hhhs-datalog/src/advisory.rs:1-41` — an accelerator that must equal the
from-scratch oracle by definition).

### 5.3 The lifecycle: a rollback is a process, not an event

Tier 1's model is an **IntentLog**: a local, per-author record of submitted
intents (intent body, the op ids it became, submission wall-time) joined
against the `EffectMap` on every growth. Phases are *derived facts*, not
stored status flags — the only stored things are the intent journal entries
and the op-set itself:

```
Draft ──abandon──▶ Abandoned                (staged, unpublished; AbandonStaged; free)
  │ confirm / ttl-expire
  ▼
Published { journaled, broadcast: Ok|Unresolved }
  │                                          (signed + ingested locally; commit is real)
  ├──▶ Effective          ◀──────────────┐   (EffectMap: fold gives it effect)
  │        │                             │
  │        ▼                             │
  │    Superseded { by, authors, kind,   │   (EffectMap verdict flipped; durable and
  │                 since }  ────────────┘    queryable for as long as it holds —
  │        │                                  resurrection flips it back: R2)
  │        ▼
  │    (user resolution → new intent: reapply / merge / accept)
  ▼
Retracted { by our own compensating intent }
```

Durational semantics, explicitly:

- **Phases persist and are continuously observable.** `Superseded` is not a
  fired-and-forgotten event; it holds until history changes it (possibly
  back — add-wins re-adds and remove-of-remove make resurrection normal, so
  the state machine is re-entrant by design). "Reverted N minutes ago" is a
  query over `since` + the superseding op's author timestamp — display-only,
  per the wall-clock discipline (`ops.rs:196-199`: "display/tiebreak-of-last-
  resort only; ordering is causal, never wall-clock").
- **Grace windows live on the Draft side of the publication boundary.** A
  tentative interaction (drag preview, held chord, "undo send" window) should
  be a *draft with a TTL* — staged locally, auto-abandoned or confirmed —
  because pre-publish abort is free and invisible to peers (§1.2). Publishing
  immediately and auto-retracting on timeout is the fallback for effects that
  must be *heard by peers during* the window; it costs a real op each way.
  R5's presence leases show a third shape for purely ephemeral effects.
- **A long-running rollback composes with continued edits for free.** Because
  a retraction only kills its causal past, anything a user (or peer) does
  *during* a rollback flow is concurrent with it and survives — the exact
  semantics already proven by `concurrent_add_remove` (`store.rs:688-715`).
  No locking, no "editing disabled while reverting."
- **Reconnection is a non-event.** The IntentLog joins against the fold; after
  a repair session backfills history, phases recompute like any other growth.
  Natively the intent journal persists beside the op journal; in the browser
  it is session-scoped until IndexedDB lands (same standing limitation as the
  op history itself — browser rebuilds from peers on reload).

### 5.4 API surface

All types speak `futures-signals` (dominator's engine —
`rewrite-p2panda-hhhs-stack/proposal.md:32-35`), and the whole surface has a
structural property worth stating: **there is no method that changes a signal
without changing data.** Reads are signals over projections; writes are
intent submissions; UX resolutions are intent submissions.

```rust
pub struct IntentId(u64);              // local handle; wire identity is the op ids

pub enum SubmitMode {
    Publish,                           // sign → journal → ingest → broadcast (Tier 0 path)
    Draft { ttl: Option<Duration> },   // staged, unpublished; abandon() or confirm()
}

pub enum IntentPhase {
    Draft { expires_at: Option<Instant> },
    Published { journaled: bool, broadcast: Delivery },   // Delivery::{Ok, Unresolved}
    Effective,
    Superseded { info: Supersession, since_ms: u64 },
    Retracted { by: IntentId },
    Abandoned,
}

impl RoomHandle {
    /// Tier 0 write path. Absolute, idempotent intent (coupling design).
    /// The returned handle may be dropped — dropping opts OUT of Tier 1.
    pub fn submit(&self, body: IntentBody, mode: SubmitMode) -> IntentHandle;

    /// Tier 0 read path: the projection the DOM binds to.
    pub fn view(&self) -> impl SignalVec<Item = Row>;         // hhhs-reactive views

    // ---- Tier 1, opt-in ------------------------------------------------
    /// Durable, queryable lifecycle model: every tracked intent with its
    /// current phase, as a live collection (undo surfaces, pending badges).
    pub fn intents(&self) -> impl SignalVec<Item = TrackedIntent>;

    /// Richly-typed transition stream for UX flows (toasts, prompts,
    /// animations). Each event carries what/why/who/when/what-it-superseded.
    pub fn lifecycle(&self) -> impl Stream<Item = LifecycleEvent>;

    /// Undo: compute the compensating intent (Retract or Compensate per the
    /// payload language) and submit it. An undo is a NEW signed op.
    pub fn revert(&self, id: IntentId) -> Result<IntentHandle, RevertError>;
}

pub struct IntentHandle {
    pub id: IntentId,
    /// Per-intent phase signal — pending/committed affordances bind here.
    pub fn phase(&self) -> impl Signal<Item = IntentPhase>;
    pub fn confirm(&self);             // Draft → Published
    pub fn abandon(&self);             // Draft → Abandoned (re-checks store
                                       // membership first: published wins,
                                       // hhhs-core/tests/rollback.rs:45-57)
}

pub enum LifecycleEvent {
    Published   { intent: IntentId },
    Superseded  { intent: IntentId, info: Supersession },  // conflict prompt, toast
    Resurrected { intent: IntentId },                      // verdict flipped back
    DraftExpiring { intent: IntentId, deadline: Instant }, // grace-period UX
    Delivery    { intent: IntentId, state: Delivery },     // unresolved → ok
    Retracted   { intent: IntentId, by: IntentId },
    Abandoned   { intent: IntentId },
}
```

Mechanics, and why this stays cheap:

- `lifecycle()` is not a new event bus. It is the **phase diff between
  consecutive fold epochs** — the same `diff_view` discipline hhhs-reactive
  already uses for rows (`lib.rs:80-103`), applied to
  `IntentLog ⋈ EffectMap`. Coalescing (`lib.rs:110`) means a burst of ops
  yields one recompute and one batch of transitions.
- A conflict-resolution prompt's buttons are just `submit`/`revert` calls:
  *reapply* = submit the same absolute intent again (its new op causally
  observes the superseding remove, so it wins add-wins cleanly — that is
  `retract_then_recreate_is_live`); *discard* = do nothing (the data already
  says discarded); *merge* = submit a composed intent. The prompt itself
  holds no state the IntentLog doesn't.
- Redo of an undo is `revert(revert_id)` — remove-of-remove, which the fold
  and kernel already resurrect (`store.rs:509-527`, `void.rs:276-288`).
- `Delivery::Unresolved` adopts potluck's tri-state (§2 R3′): broadcast
  failure or silence is *unresolved*, upgraded to `Ok` by sync evidence —
  a completed repair session whose union provably contains the op
  (`SyncApply.admitted`, `src/net/sync.rs:354-375`; peer `Done{root}`
  cross-check, `store.rs:187-195`). Conservative, display-only, never
  load-bearing for correctness.

### 5.5 What this looks like in practice

- **Tap a key** (the 99% case): `submit(SetDegree{pitch, on}, Publish)`,
  handle dropped. Tier 0 end to end; if a peer later supersedes it, the key
  simply un-highlights. Total rollback code: none.
- **Drag an emoji piece**: `submit(PlacePiece{..}, Draft{ttl: 3s})` on
  pointer-down; `confirm()` on drop, `abandon()` on escape. The draft renders
  through the same projection (the staged overlay is part of the local view,
  visually marked as tentative via its phase), peers never see an aborted
  drag.
- **Change the room tuning** (destructive, room-wide): `Publish`, keep the
  handle, subscribe `lifecycle()`. If `Superseded{kind: RegisterLoss}`
  arrives, show "Ada's tuning replaced yours — reapply?" — the reapply button
  submits a fresh `SetTuning` that causally observes Ada's.
- **Undo surface**: bind a list to `intents()` filtered to own effective
  intents; each row's action is `revert(id)`.

---

## 6. Cotransactions: multi-op intents and their rollback

Today every intent maps to exactly one `WalkieOp`, so intents already *are*
co-transactions in the HHS3 sense (§1.3) and this section is forward-looking
(chord placement = N degree ops, "move all my pieces up an octave").

Options for all-or-nothing across peers, given that gossip delivers ops
individually and anti-entropy transfers arbitrary subsets:

1. **One op carries the batch** (recommended): append a
   `WalkieOp::Batch(Vec<WalkieOp>)` variant (append-only enum evolution,
   `ops.rs:27-29`; the 1 MiB payload ladder already bounds it,
   `ops.rs:49-64`). Atomic by construction — one entry hash, one verdict, one
   signature — the direct translation of HHS3's "a multi-table write is one
   atomic operation" (`hhs3-ts/modules/rdb/README.md:23-27`). Fold cost: sub-ops
   key as `(entry_hash, index)` where op-id identity matters (piece creation
   inside a batch). Its rollback is symmetric: **one** compensating batch op
   retracts the whole unit — no partial-rollback state exists.
2. *Causal chain + seal op* (rejected for v1): sign the parts as a
   backlink-chained run and give effect only when a closing "seal" op is
   present. Strict deferral already guarantees a peer cannot materialize the
   chain out of order (`store.rs:275-284`; liveness pinned by W8,
   `rewrite-p2panda-hhhs-stack/integration-tests.md:65-83`), but every read
   rule must learn seal-awareness, and an unsealed prefix is a new
   liveness-limbo state to reason about. More machinery for the same
   guarantee option 1 gets structurally.
3. *Round boundaries* (different tool): potluck's `StartNewRound` /
   `round_anchor` pattern — an append-only read-model boundary that makes
   *many* prior ops inert at once without rewinding logs
   (`potluck/STATELESS-TABLES-HHHS.md:43-46`, `67-76`). Not an atomicity
   primitive, but the right shape if walkie ever wants "clear the room" /
   session-scoped bulk revert (R2 at session granularity): one signed
   boundary op, Tier 0 re-projection, trivially convergent.

Abort semantics for a batch intent then follow §1.2 exactly: `Draft` batch →
`abandon()` (free); published batch → `revert()` submits the inverse batch
(one op). The `IntentPhase` machine of §5.3 applies unchanged because the
batch is a single tracked op.

---

## 7. Convergence and correctness of rollbacks themselves

The rule that makes everything above safe: **a rollback is a signed op (or a
purely local pre-publish discard) — never a local erasure.** Checklist against
the reconciliation model:

1. **Undo ops converge like any op.** They enter the same union
   reconciliation; deterministic outcome across arrival orders is already
   proven for remove/unremove races (`store.rs:1107-1138`; W6 partition test,
   `integration-tests.md:65-83`).
2. **Phases are local but deterministic.** `EffectMap` is a pure function of
   the op-set, so two peers with equal op-sets compute equal verdicts for any
   op; the IntentLog adds only the author's own private journal. Peers may
   see transitions at different wall times (`since_ms` is receipt-local,
   display-only) but never reach different verdicts. Nothing in Tier 1 is
   gossiped, so it cannot cause divergence.
3. **No verdict caching.** `EffectMap` recomputes per coalesced growth,
   honoring the load-bearing no-cache doctrine (`void.rs:21-32`,
   `register.rs:26-37`) — a memoized verdict is a convergence bug, not an
   optimization.
4. **Draft TTLs are safe by construction** — an auto-abandoned draft was
   never published, so peers never knew it. Auto-*retract* timers on
   published leases are convergent but note the multi-device case: two
   devices of one author racing an auto-retract emit two retractions of the
   same target — idempotent at the fold (both void the same causal past),
   harmless.
5. **The stale-staged hazard is a MUST.** `abandon()` re-checks store
   membership and refuses to abandon anything already published
   (`hhhs-core/tests/rollback.rs:45-57`) — published always wins, then the
   correct rollback is `revert()`.
6. **Delivery/acknowledgment is evidence, not authority.** `Unresolved → Ok`
   upgrades only on sync proof (admitted sets, `Done{root}` coverage), and
   the root cross-check's known limit applies — it detects an honest
   withholder, not a root-forging peer
   (`hhhs-core/src/sync_session.rs:979-1010`); residual repair is multi-peer
   gossip + periodic anti-entropy, as everywhere else.
7. **Two divergence-adjacent flags**, pre-existing and unchanged by this
   design: the browser keeps no journal (an intent journal there is
   session-scoped until IndexedDB; consequences bounded to Tier 1 history
   surfaces), and `SessionStatus::Divergent`/`root_mismatch` remain "re-sync,
   never revert" (`src/net/sync.rs:29-33`, `browser_host.rs:1284-1295`) — no
   Tier 1 feature may react to divergence by mutating state.

---

## 8. Implementation plan (each step independently testable, Tier 0 first)

1. **Land the coupling design's pure projection** (its plan, not this one):
   kill the two optimistic sites (`keyboard.rs:1390-1399`, `1186-1189`) and
   the `ToggleDegree` involution (`browser_host.rs:210-225` reading
   `pitch_authors` to invert) in favor of absolute intents. *This alone ships
   Tier 0 rollback in full.* Test: two tabs, concurrent add/remove of one
   key — projections converge with no snap-back artifacts.
2. **`EffectMap` in the fold** (`store.rs`): same pass as `view()`, oracle
   parity + mutation tests like the existing ones (`store.rs:1184-1224`).
   Small, pure, no wire change.
3. **IntentLog + `IntentPhase` diffing**: intent journal (native: beside
   `FileOpJournal`; browser: in-memory), join against `EffectMap` per growth,
   expose `intents()` / `lifecycle()` / per-handle `phase()`. Test:
   scripted histories assert exact transition sequences
   (Published → Effective → Superseded → Resurrected).
4. **`revert()`**: per-`IntentBody` inverse (Retract vs Compensate per
   payload semantics — the `retractable` bool `rollback_for` asks for,
   `rollback.rs:36-55`); undo history surface binds `intents()`. Test: undo,
   redo (remove-of-remove), undo racing a peer's concurrent edit.
5. **Draft mode**: staged overlay + TTL + confirm/abandon with the
   published-wins re-check; drag-preview UX rides it. Test: abandoned draft
   is invisible to a peer; confirm publishes bytes identical to a direct
   publish.
6. **First Tier 1 UX consumer**: `Superseded{RegisterLoss}` toast for tuning
   (§5.5) — deliberately the *destructive* flow, not the keyboard. Gate: the
   keyboard path must still contain zero rollback code.
7. **(Later) `Delivery` tri-state** from sync telemetry; **(later) `Batch`**
   op variant when the first multi-op intent exists (§6).

Steps 1–2 are the substance; 3–5 are one bounded module (`src/room/intents.rs`
or the coupling design's equivalent); 6 proves the tiering held.
