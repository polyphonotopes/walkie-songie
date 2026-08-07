# Data ↔ UI coupling: enforced projection & co-transactional intent

Status: design review, 2026-08-07. Grounded in walkie-songie source at HEAD,
`hhhs-rs @ bd23d4e` (the pinned rev, current master), the HHS3 blackpaper
(`/laboratory/hhs-v3/blackpaper/report.html`, § "Co-transactions") and the
hhs3-ts rdb engine docs (`/laboratory/fe-stuff/hhs3-ts/modules/rdb/README.md`).
No code changed.

The invariants under design, in the words that filed them:

> "a signal based UI should be TIGHTLY coupled to the actual state" — "it
> shouldn't be possible to mutate UI state without mutating actual underlying
> state" — "the UI state should be a direct projection of data" — "intent
> should be preserved; if someone's intent is to toggle off, we shouldn't send
> a raw involution … I could see some gnarly instabilities emerging in the tap
> gap."

Two observed bugs, one family:

1. the local UI shows state the store doesn't back after a peer sets a piece /
   toggles a note;
2. it takes a double-tap to (fail to) turn off a note a peer toggled on, with
   a flicker.

Both are consequences of the same architecture fact: **`state.room` has two
writers** — the projection of the authoritative store, and a dozen tap
handlers that mutate it directly — and the tap path ships its intent as an
**involution** re-resolved later against racing state. §1 maps this precisely;
§2 makes the projection structurally exclusive; §3–4 redesign the command as
an absolute intent and show it is (already, degenerately) an HHHS
**co-transaction**; §5 folds the offline mode into the same seam; §6 is the
staged plan.

---

## 1. Architecture map: one `Mutable`, two writers

### 1.1 The reactive read model the UI binds to

`AppState.room: Mutable<RoomState>` (`src/web/app.rs:77`) wraps the legacy
Yrs document adapter (`RoomState`, `src/room/yrs_state.rs:84`). Everything the
keyboard renders reads it:

- `sync_active_pitches` (`src/web/keyboard.rs:178-233`) — `room.lock_ref()`,
  reads `shared_pitches()` / `all_pieces()` / voice state, paints DOM
  overlays. Called from the tap handler (`keyboard.rs:1411`), from every
  `RoomEvent` (`app.rs:1634-1645`), and after every projection
  (`app.rs:466`).
- `setup_piece_sync` (`keyboard.rs:656-702`) — turns `room.events()` into a
  `pieces_signal` (`keyboard.rs:701`) driving per-piece DOM elements; it also
  back-writes `state.pieces_locked` (`keyboard.rs:713-715`).
- `emoji_picker` (`src/web/components.rs:93-101`) — reads
  `available_emojis()` inside a signal map.
- MIDI output (`app.rs:880` on) and IndexedDB persistence (`app.rs:1666`)
  read the same adapter.

So `state.room` **is** the UI state. The question is who may write it.

### 1.2 The authoritative path (native modes)

In both native modes (`native_backend`, `app.rs:154-161`: Tauri, or the
in-page `browser_host` — note `web-ui` enables `browser-net`,
`Cargo.toml:18-28`, so every shipped browser build runs the in-page host),
the write path is a strict one-way loop:

```
tap → ClientCommand ──dispatch_native (app.rs:218-232)──▶ host
  host.dispatch (browser_host.rs:193-273 / src-tauri/src/lib.rs:142-…)
    → submit_durable (browser_host.rs:768-798, mpsc to the room task)
    → commit_room_op (browser_host.rs:1299-1324)
        → RoomStore::commit (src/room/store.rs:379-390)
            prepare_commit stamps observed = store frontier (store.rs:357-374,
            observed_frontier store.rs:343-350) and signs (ops.rs:426-439)
        → gossip broadcast (browser_host.rs:1312-1320)
    → store.view() (store.rs:393-408) — pure fold over the causal DAG
    → apply_room_view (browser_host.rs:882-963) — diffs the view against the
      host snapshot, emits ordered AppEvents (DegreeAdded/DegreeRemoved at
      897-921, PieceUpserted/Removed at 935-946, config at 949-957)
  UI: apply_native_event (app.rs:334-356; sequence-gated, folds the delta
      into native_snapshot via apply_native_delta app.rs:1049-1127)
    → project_native_snapshot (app.rs:358-467)
    → RoomState::replace_native_projection (yrs_state.rs:277-352) — wholesale
      rewrite of the Yrs maps → RoomEvents → sync_active_pitches
```

Remote ops enter the same funnel: gossip ingest at
`browser_host.rs:564-578`, anti-entropy at `browser_host.rs:1211-1260` —
both end in `apply_room_view`. **The projection loop is correct and total.**
The materialization it projects is HHHS-native: degrees are a content-keyed
add-wins set resolved by `ReachIndex` ancestry (`store.rs:411-452`), pieces
are owner-gated per-owner seq registers (`store.rs:457-551`), tuning/config
are causal-maxima registers (`store.rs:555-605`).

### 1.3 Every UI write that bypasses the store

The complete inventory of direct `state.room.lock_mut()` (and sibling
optimistic `Mutable` writes) outside the projection:

| Site | Write | Gated on offline? | Store analog |
|---|---|---|---|
| `keyboard.rs:1390-1391` | `toggle_pitch(pc)` in the tap handler | **no** | `ToggleDegree` dispatched at 1397 |
| `keyboard.rs:1394` | `clear_voice_at_pitch_class(pc)` | **no** | none (presence is host-side) |
| `keyboard.rs:989` | `remove_piece` on hole-drop | **no** | `RemovePiece` at 988 |
| `keyboard.rs:1039` | `move_piece` on drag-drop | **no** | `MovePiece` at 1038 |
| `keyboard.rs:1187-1189` | `add_piece` on HTML5 drop | yes (`!native_backend`) | `PutPiece` at 1185 |
| `keyboard.rs:1310-1311` | `add_piece` on pointer drop | yes | `PutPiece` at 1309 |
| `components.rs:56-59` | `clear_pitches/voice/pieces` (Clear button) | **no** | `clear_native_musical_state` at 53 |
| `components.rs:81-83` | `pieces_locked.set` + `set_pieces_locked` | **no** | `SetRoomConfig` at 84 |
| `components.rs:431` | `set_tuning_scl` (SCL editor) | **no** | `SetTuning` at 429 |
| `app.rs:599-601` | `set_voice(Some…)` during voice lock | **no** | `SetVoicePreview` at 602 |
| `app.rs:763` | `set_voice(None, None)` on voice stop | native-only branch | — |
| `app.rs:863-868` | MIDI add/remove_pitch | yes (`browser_host` branch at 856 dispatches instead) | — |
| `app.rs:1567` | `load_state` from IndexedDB | yes | — |

Ten of the thirteen mutate the adapter **even when the store is
authoritative**. Each is a second, unsynchronized writer racing the
projection.

### 1.4 The three runtime modes

- **Tauri** (`tauri_backend`, `app.rs:213-216`): store lives in
  `src-tauri/src/lib.rs`; identical command handlers (e.g. `ToggleDegree` at
  `src-tauri/src/lib.rs:159-173` mirrors `browser_host.rs:210-225`).
- **browser_host**: store lives in-page (`browser_host.rs:82-125`); same
  `ClientCommand`/`AppEvent` seam, deliberately indistinguishable to the UI
  (`browser_host.rs:1-8`).
- **offline** (no Tauri, built without `browser-net` — the fallback build):
  `dispatch_native` is a no-op (`app.rs:219-221`); the Yrs adapter **is** the
  authoritative state, persisted as a Yrs update to IndexedDB
  (`app.rs:1662-1671`, loaded at `app.rs:1562-1571`).

The invariant must hold in all three. Today it holds in none: in native
modes the UI writes an adapter it doesn't own; offline, the UI *is* the
owner, so the same handler code is load-bearing there and corrupting
elsewhere — which is why every fix that "just deletes the lock_mut" breaks
offline, and why §5 routes offline through the same host seam instead.

### 1.5 The bug mechanics, precisely

**(a) The flicker and the first wasted tap.** Tap a note a peer toggled on.
`keyboard.rs:1391` optimistically flips the adapter → key paints OFF.
`toggle_native_degree` (`app.rs:258-263`) dispatches
`ClientCommand::ToggleDegree`. The host resolves the toggle **by
authorship**, not presence (`browser_host.rs:212-218`): "am I in
`pitch_authors[pitch]`?" You aren't (the peer is), so it submits
`WalkieOp::AddDegree` — your *off* intent became *on* data. The view's
author set for that degree changes `{peer}→{peer, you}`, so
`apply_room_view` emits `DegreeAdded` (`browser_host.rs:915-920`), the
projection rewrites the adapter (`app.rs:459-465`) and the key snaps back ON.
One flicker, zero musical effect, and you are now a co-author.

**(b) The tap gap.** `ToggleDegree` is an involution *resolved at apply
time*: the presence check runs in `dispatch` on the host's current
`pitch_authors`, but the commit round-trips an async channel
(`browser_host.rs:768-798` → `373-380`) and the authors map is only updated
by `apply_room_view` (`browser_host.rs:922-923`). Two quick taps both read
the stale map → **two `AddDegree`s** → the note can no longer be toggled off
by parity-counting at all. This is exactly the "gnarly instabilities … in
the tap gap": the involution's meaning depends on *when* it is evaluated,
and the evaluation races the previous tap.

**(c) Permanent divergence when the op is a no-op.** `apply_room_view` emits
only diffs (`browser_host.rs:895-958`). Drag a **peer's** piece to the
delete hole: `keyboard.rs:989` removes it from the adapter unconditionally,
while the store's `RemovePiece` is owner-gated to a no-op
(`store.rs:509-513` filters `r.2 == owner`). View unchanged → zero events →
`project_native_snapshot` never re-runs → the adapter keeps showing the
deletion **indefinitely**, until some unrelated event forces a projection.
That is bug 1: UI state with no data behind it, and no snap-back because
snap-back is diff-driven. The same hole exists for `move_piece` of a peer's
piece (`keyboard.rs:1039`) and for any command the store rejects or
resolves to identity.

**(d) The store already does the right thing.** The op layer needs no fix
for the *off* semantics: `RemoveDegree` retracts every add **in its causal
past, across authors** — `with_pitches` kills an add iff any same-key remove
is its causal descendant, with no author comparison (`store.rs:437-446`);
the doc pins it ("A `RemoveDegree` supersedes only the adds in its causal
past; a concurrent add survives", `ops.rs:16-18`); and the tests prove the
cross-author kill (`store.rs:758-765`: C's remove, observing A's add via
`observed`, leaves degree 7 "fully removed"; concurrent-remove-loses at
`store.rs:897-923`). Because `commit` stamps `observed = store frontier`
(`store.rs:370`), a remove signed after the peer's add was *rendered*
necessarily observes it. The entire bug family lives **above** the store: in
the command's authorship predicate and in the UI's second write path.

---

## 2. The invariant, made structural

"Should not mutate" must become "cannot mutate". Convention already failed —
half the sites in §1.3 are annotated "optimistic" and still shipped the bug.

### 2.1 The mechanism: a read-only handle, one private writer

`futures_signals` already ships the exact tool:
`Mutable::read_only() → ReadOnlyMutable<T>`, which offers `signal_cloned()`,
`lock_ref()`, `get_cloned()` — and **no `lock_mut`**. The shape:

```rust
// app.rs
pub struct AppState {
    /// The UI read model. PRIVATE: the projection is the only writer.
    room: Mutable<RoomState>,          // was: pub room
    …
}

impl AppState {
    /// The one way the rest of `web` sees room state.
    pub fn room(&self) -> ReadOnlyMutable<RoomState> { self.room.read_only() }

    /// The one writer. Private; called only from project_native_snapshot.
    fn apply_projection(&self, …) {
        self.room.lock_mut().replace_native_projection(…);
    }
}
```

Rust privacy is module-scoped: `keyboard.rs`, `components.rs`, `graph.rs`
are sibling modules of `app.rs` under `web/`, so a non-`pub` field is
inaccessible to them — every `state.room.lock_mut()` in §1.3 becomes a
compile error, not a review comment. Reads migrate mechanically
(`state.room.lock_ref()` → `state.room().lock_ref()`). This is the same
capability discipline the op layer already uses: `VerifiedOp` has private
fields so "a store write that takes a `VerifiedOp` cannot be handed
unverified data" (`ops.rs:352-355`). Apply it one layer up: *a render
adapter that only the projection can construct cannot show unbacked state.*

Prefer this over a `Projected<T>` newtype with an apply-token: the token
buys nothing `ReadOnlyMutable` + field privacy doesn't already enforce, and
it adds a type the ecosystem doesn't know. Also make the adapter's mutators
(`toggle_pitch` `yrs_state.rs:464-472`, `add/remove_pitch` 434-461,
`add/remove/move_piece` 1021-1104, `set_pieces_locked` 1180, `set_tuning_scl`
633, `set_voice` 843) `pub(crate)`-or-narrower once §5 lands, so even code
holding a `&mut RoomState` outside the projection module shrinks to nothing.

The sibling `Mutable`s that projection also owns get the same treatment
where they have optimistic writers today: `pieces_locked` (written by the
projection at `app.rs:458`, by `lock_button` at `components.rs:81`, and by
`setup_piece_sync` at `keyboard.rs:713-715` — the last two go), and `tuning`
(the SCL editor sets it directly at `components.rs:427` *and* dispatches;
keep only the dispatch, let `TuningChanged` → `app.rs:369-377` set it).

### 2.2 What each write site becomes

| §1.3 site | Replacement |
|---|---|
| tap `toggle_pitch` | dispatch absolute intent (§3); no adapter write |
| tap `clear_voice_at_pitch_class` | host-side: an *absent* degree intent already silences the toggle; peer voice rows expire by lease (`browser_host.rs:680-698`) — the local "clear their voice now" echo was never data and is dropped |
| piece drop/move/delete | command only; the projection moves the DOM piece. Rejected ops (peer-owned) now correctly do nothing — surface a `Diagnostic` if silent no-op reads as broken |
| Clear button | `clear_native_musical_state` (`app.rs:293-304`) only |
| lock button | `SetRoomConfig` only; `pieces_locked` paints from `RoomConfigChanged` |
| SCL editor | `SetTuning` only |
| voice lock/stop | `SetVoicePreview` only; the local echo arrives as `VoiceUpdated` from `apply_presence` (`browser_host.rs:966-1002`) — in-page that is one task tick |

Latency note: the in-page host commits and projects within the same event
loop turn (`commit_room_op` → `apply_room_view` → subscriber fan-out is all
synchronous once the control channel yields), so "projection-only" costs one
tick, not a network round trip. Tauri costs one IPC round trip; if that ever
reads sluggish, the answer is an *explicitly modeled* pending overlay (a
separate `Mutable<PendingIntents>` the projection clears on ack) — never a
silent write into the projected model. The invariant forbids unbacked state,
not honest "in flight" affordances.

---

## 3. Intent over involution

### 3.1 Why a toggle cannot survive this architecture

HHHS state is a *reconciled set of signed ops*: ingest deduplicates by op id
(`store.rs:249-255`), RBSR anti-entropy converges peers on the entry-hash
identity set (`store.rs:143-148`, `sync_root` 194-196), and the read model
is a pure fold over the DAG snapshot (`store.rs:393-408`). That machine is
built for ops that are **idempotent** (re-delivery is a no-op — guaranteed
by set semantics) and **commutative up to causality** (any delivery order
with the same causal closure folds to the same view). `AddDegree`/
`RemoveDegree` are exactly that.

A toggle is the opposite: an involution whose meaning is a function of the
state *at evaluation time*. In an op-set it is uninterpretable — two
concurrent toggles annihilate or double-apply depending on fold order; a
duplicated delivery flips state; nothing converges. Walkie never puts the
toggle on the wire, but `ClientCommand::ToggleDegree` (`client.rs:52-55`)
just moves the involution to the dispatch seam, where §1.5(b) showed it
races itself across the tap gap — and its authorship predicate
(`browser_host.rs:213-218`) additionally mistranslates *presence* intent
into *authorship* bookkeeping (§1.5(a)). Two hosts duplicate this logic
(`src-tauri/src/lib.rs:159-173`).

### 3.2 The fix: derive an absolute intent from the projection at tap time

The commands are already there and already absolute: `AddDegree` /
`RemoveDegree` (`client.rs:49-58`). The MIDI path already uses them
absolutely (`app.rs:856-859`, note-on/off → `set_native_degree(pc, on)`), as
does voice commit (`app.rs:760`). Only the tap uses the involution. So:

```rust
// keyboard.rs keyclick handler (replaces keyboard.rs:1389-1400)
let present = state.degree_is_active(pc);       // read the PROJECTION
state.set_native_degree(pc, !present);          // absolute, idempotent
```

where `degree_is_active` reads the projected snapshot
(`native_snapshot.active_degrees`, the exact set `apply_room_view` computed
— `browser_host.rs:895-922` / `app.rs:1067-1075`), or equivalently the
adapter the projection wrote. **Presence, not authorship**: tapping a
peer's on-note yields intent "→ absent" → `RemoveDegree` → the store's
observed-remove clears it for everyone (§1.5(d)) — one tap, no flicker, no
double-tap.

Idempotence closes the tap gap: two rapid "absent" taps produce two
`RemoveDegree`s — the second retracts an already-retracted set, view
unchanged, verdict identical on every replica and on re-delivery. Two
racing peers tapping off: both removes, converged off. Off racing a fresh
on: add-wins keeps the new note (`store.rs:897-923`) — the concurrent
*positive* intent survives, which is the musically right bias (no lost
notes; the off only cancels what it observed, §4).

`ClientCommand::ToggleDegree`, `AppState::toggle_native_degree`
(`app.rs:258-263`), both host arms, and `RoomState::toggle_pitch` then have
zero callers and are deleted — the protocol is in-repo on both ends
(`CLIENT_PROTOCOL_VERSION` 1, `client.rs:11`), so no compat shim is needed.
(The offline `else` branch at `keyboard.rs:1398-1400` is already dead
weight: `set_native_degree` no-ops offline, `app.rs:219-221`.)

---

## 4. Co-transactions: the HHHS-native intent primitive

### 4.1 What a co-transaction is

The HHS3 blackpaper (report.html, § "Co-transactions") defines it as the
coordination-free dual of a transaction: an operation **carries preconditions
asserting the observed state at the causal position it was authored** ("how
much of B's history A has seen is part of A's state"; preconditions are
attached when appending an op that depends on locally-observed properties),
and every replica **re-evaluates those preconditions at-use** — reading *at*
the op's own position *from* the current frontier — deterministically
voiding whatever a concurrent history invalidated. hhs3-ts ships this
verbatim: "each operation is re-checked through MVT's view-revision
mechanism … whatever breaks the schema in that view is discarded … Every
honest replica reaches the same verdict. Reconciliation is
coordination-free" (`modules/rdb/README.md`, § Co-transactions). The
deep-core survey (`/laboratory/fe-stuff/misc/hhs3-riffcat-relation-2026-07-17/
hhs3-deep-core-fable.md`, §1) confirms the division of labor: monotone ops
(the CALM fragment) need no machinery; the non-monotone residue (removes,
revokes — "barrier" ops) is where preconditions and at-use re-evaluation
live, and "state" is always a query over the log, never a stored thing.

### 4.2 Walkie already ships the degenerate co-transaction

Map the pieces one-to-one:

| co-transaction concept | walkie mechanism |
|---|---|
| observed position as part of the op | `VersionedOp.observed` = store frontier at signing (`ops.rs:206-210`, stamped at `store.rs:370`) |
| precondition | *implicit*: "the on-ness I cancel is what my horizon sees" |
| at-use re-evaluation, AT op FROM frontier | `with_pitches`: remove kills exactly the adds with `is_ancestor(add, remove)` (`store.rs:437-446`), recomputed on every `view()` |
| deterministic verdict on every replica | view is a pure fold over the converged entry set (`store.rs:5-16`) |
| concurrent invalidation → void | a concurrent add is *outside* the remove's horizon and survives (add-wins) — the precondition-uncovered part simply doesn't fall |
| monotone fragment needs nothing | `AddDegree` is unconditional and commutes |

So the tap's absolute intent **is** a co-transaction with a
system-generated, implicit precondition — precisely the blackpaper's "if
state reads could be automatically detected … preconditions asserting the
observed state could be system-generated". The projection loop supplies the
"automatic read detection": what the user saw when tapping *is* a rendered
revision of the store, and the frontier stamped at signing is a superset of
it (signing happens strictly after render). Nothing new must be invented for
the tap path to be co-transactionally sound; it must merely **stop being an
involution** (§3) and stop having a second writer (§2).

This is also why the intent model and the signal model are the same shape:
`hhhs-reactive`'s revision streams carry the horizon they were computed at
(`Revision { added, retracted, at: Position }`,
`hhhs-reactive/src/lib.rs:63-67`). A signal-bound UI renders *at* a
position; an intent derived from that render is an op *observing* that
position; the store re-evaluates it *from* every future frontier. Signals
down, co-transactions up — one causal vocabulary.

### 4.3 The API adjustment: make the precondition explicit (when the schema next moves)

The implicit horizon-precondition is sound but blunt: "retract every add in
my causal past" ⊇ "retract the adds I was shown" (the delta is adds lifted
between render and sign — sub-tick in practice). The cotx-native sharpening
makes the intent *data*:

```rust
/// v4 candidate — append-only variant per the evolution discipline (ops.rs:27-29)
RetractDegreeAdds { pitch: TunedDegree, adds: Vec<OpId> }
```

where `adds` is the exact live add set the projection rendered — available
today as `pitch_authors`' backing entries (`store.rs:435-448`) and cheap to
surface per degree. This is byte-for-byte the shape `hhhs-core` blesses as
its one built-in replicable type: the position-keyed observed-remove set,
`lens::Op::Remove(EntryHash)` targeting the `Add` that introduced the
element (`hhhs-core/src/lens.rs:1-25`) — generalized to a multi-target
retract. Properties gained:

- **intent preserved verbatim**: "turn off what I see", never "what I could
  have seen"; the render-to-sign gap can no longer widen the kill set;
- **provenance**: who retracted whose add is queryable, enabling attribution
  UI and, later, *policy* — an evaluator that voids retracts of foreign adds
  would implement per-author ownership as a **view-side rule with zero op
  changes**, exactly rdb's "authority is the schema, not the message"
  separation;
- **causal safety for free**: the targets ride `observed`, so strict
  deferral (`store.rs:275-284`) guarantees they are lifted before the
  retract is, and dangling targets are structurally impossible.

Costs: a schema bump (`OP_SCHEMA_VERSION` 3→4, `ops.rs:48`) or an appended
variant + version gate, and a few hundred bytes per retract. Verdict:
**adopt at the next planned schema move, not as part of this fix** — the
implicit-horizon `RemoveDegree` already converges deterministically under
RBSR and delivers the user-visible behavior; the explicit form is the right
long-term primitive, not the bug fix. (What is *not* wanted at any layer: a
toggle op — §3.1 — or an LWW register per degree, whose causal-maxima
tie-break (`register::resolve`, `store.rs:579-593` pattern) would drop one
of two concurrent ONs: a lost note, the wrong bias for an instrument.)

### 4.4 Per-author vs shared: decided

- **Shared observed-remove** (what the store already implements,
  §1.5(d)): anyone can silence a sounding note; the retraction is scoped to
  observed adds; concurrent re-adds win. Tap-gap analysis: off∥off → off;
  off∥on → on (add survives); duplicated/reordered delivery → same verdict.
  Stable, convergent, and it matches the instrument: the keyboard is one
  shared surface, and "display = union, you own only yours" is precisely
  what made a peer's note un-turn-off-able — the observed bug elevated to a
  rule.
- **Per-author** ("remove retracts only my authorship") would require
  *adding* an author filter to `with_pitches` (today there is none —
  `store.rs:437-441`), reintroduces the dead first tap by construction, and
  turns every disagreement into a stalemate a UI cannot render honestly.

**Recommendation: shared.** Keep authorship as *attribution* (the
`pitch_authors` map already flows through `DegreeAdded.authors`,
`client.rs:195-198` — render it as color, not as a veto). If a future room
wants ownership-gated degrees, express it as evaluator policy over explicit
retracts (§4.3), where both semantics coexist per-room without touching the
op alphabet — intent is preserved either way, which is the property the
user asked for.

---

## 5. Offline: the same seam, not a parallel truth

The invariant "UI is a projection of data" needs a *data* for offline. Give
it the same one: an **offline host** — the `browser_host` skeleton minus
networking. Concretely: `HostState` + `dispatch` + `apply_room_view`
(`browser_host.rs:72-273, 882-963`) are already network-free; `start_room`
is where the network binds (`browser_host.rs:303-334` — under concurrent
edit, see §6). An offline room is `RoomStore::new()` + the IndexedDB
identity seed (`browser_host.rs:110-111`) + a commit loop, full stop.

Persistence upgrades from "Yrs update blob" (`app.rs:1662-1671`) to a
**signed-op journal in IndexedDB**: append each committed op's
`to_wire_bytes` (`ops.rs:281-292`) under the room topic; on entry, verify +
ingest the stored ops (dedup makes replay idempotent, `store.rs:249-255`) —
the browser analog of the native journal (`src/room/journal.rs`), and it
incidentally fixes the browser-host amnesia the module header admits ("a
lone tab's history does not survive a reload (yet)",
`browser_host.rs:10-12`): the same journal serves both browser modes.

Result: `native_backend` is true in every mode, `dispatch_native`'s early
return (`app.rs:219-221`) disappears, and every offline-only adapter write
in §1.3 (`app.rs:863-868`, `1562-1571`, `keyboard.rs:1187-1189`,
`1310-1311`) is deleted rather than preserved behind flags. The Yrs
`RoomState` is then a pure render adapter everywhere — and becomes
replaceable later by `hhhs-reactive` views bound straight to the store
(`signal_vec_view`, `hhhs-reactive/src/lib.rs:245`; requires exposing a
`Growth`-subscribed handle from `RoomStore`, whose `MemDagStore` is private
today, `store.rs:115` — a separate proposal, not on this critical path).

---

## 6. Implementation plan (each step buildable & testable)

Concurrency callout: another agent is editing `src/net/**` and
`browser_host.rs`'s `start_room`. Steps below touch `browser_host.rs` only
in its **command handlers and projection** (`dispatch`, `apply_room_view`) —
coordinate the merge, don't touch `start_room`.

1. **Absolute tap intent** — `keyboard.rs` (tap handler 1389-1400),
   `app.rs` (add `degree_is_active` reading the projection). Tap computes
   presence from the projected state and calls
   `set_native_degree(pc, !present)`; delete the optimistic
   `toggle_pitch`/`clear_voice_at_pitch_class` writes. *Fixes both observed
   bugs.* Test: native — a host-level test driving two rapid
   `RemoveDegree`s + a peer add through `RoomStore` asserting the view
   (extend `tests/l0_convergence.rs` patterns); wasm — two tabs, peer
   toggles on, one tap turns off everywhere, no flicker.
2. **Retire the involution** — `client.rs` (drop `ToggleDegree`),
   `browser_host.rs:210-225`, `src-tauri/src/lib.rs:159-173`,
   `app.rs:258-263`, `yrs_state.rs:464-472`. Pure deletion; both targets
   build (`cargo build` + `cargo build --target wasm32-unknown-unknown
   --features web-ui`); `cargo test` (l0 suites unaffected — the op alphabet
   is untouched).
3. **Single-writer sweep (native modes)** — `keyboard.rs` (989, 1039,
   piece-drop echoes), `components.rs` (56-59, 81-83, 427-431), `app.rs`
   (599-601, 763): command-only per the §2.2 table; add a `Diagnostic`
   surface for rejected piece ops. Test: Tauri + browser-host manual matrix
   (drag own/peer piece, lock, clear, SCL edit) — peer-owned piece drag now
   visibly snaps back instead of silently diverging.
4. **Offline host + signed-op journal** — new `web/offline_host.rs` (or a
   host constructor without a network), `web/storage.rs` (op-journal
   get/append), `app.rs` init (route offline through it; drop the Yrs
   persistence path 1562-1571/1662-1671). Test: offline build, reload
   restores state via replay; then the same journal wired into browser_host
   restores a lone tab.
5. **Structural enforcement** — `app.rs`: `room` goes private,
   `pub fn room() -> ReadOnlyMutable<RoomState>`; mechanical
   `lock_ref()`-accessor migration in `keyboard.rs`/`components.rs`/
   `graph.rs`; narrow `RoomState` mutators to the projection path. The
   compiler now proves the invariant; steps 1–4 made it true, this makes it
   *stay* true. Test: full build matrix + the manual matrix from step 3.
6. **(Later, op-layer)** — explicit-retract co-transaction
   (`RetractDegreeAdds`, §4.3) at the next `OP_SCHEMA_VERSION` bump, and the
   `hhhs-reactive` projection replacing the Yrs adapter (§5). Each is its
   own proposal; neither blocks the fix.

Steps 1–2 are the user-visible repair; 3–5 are the invariant; 6 is the
architecture converging on its own stack.
