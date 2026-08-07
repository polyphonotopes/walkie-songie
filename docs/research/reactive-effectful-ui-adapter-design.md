# A reactive + effectful adapter for the imperative UI

Status: design review, 2026-08-07. Sequel to
`docs/research/ui-state-coupling-design.md` (whose steps 1–2 — absolute tap
intent, `ToggleDegree` deleted — are shipped; the tap path now satisfies the
invariant) and companion to `docs/research/reactive-rollback-api-design.md`
(the intent-lifecycle/rollback taxonomy). Grounded in walkie-songie source at
HEAD, `hhhs-rs @ bd23d4e`, `futures-signals 0.3` and `dominator 0.5.38`
(registry sources cited where API shape is load-bearing). No code changed.

The invariant under design, in the words that filed it:

> "we may want to conceive of a tighter adapter into the classic UI
> components / flows" — "ideally nicely reactive and effectful."

The coupling design made the *signal-bound* UI a pure projection of data and
proved the discipline on the toggle path. This doc generalizes its step 3 to
the part of the UI that is **not** signal-bound today: the imperative-DOM
tangle in `src/web/keyboard.rs` — raw `create_element`/`set_attribute`
children driven into an external web component, a hand-rolled drag lifecycle,
and a dozen "call `sync_*` after every mutation" sites. The concrete failure
it must fix: **dragging an emoji piece decouples the mover's UI from the
mover's own store, permanently.** §1 verifies that diagnosis against source;
§2 designs the adapter (signals out, *bound effects* in); §3 extends the
structural invariant to imperative sinks; §4 gives the drag lifecycle over a
reconciling store; §5 surfaces the owner-gated-vs-shared semantics call
(user's decision); §6 specifies the tests; §7 is the staged plan.

---

## 1. The failure, verified: a drag the data never accepted

### 1.1 The store is right and convergent

Pieces are an owner-gated, per-owner seq register: "Only the owner's ops
affect a piece; the greatest-seq lifecycle op decides liveness; the
greatest-seq move decides position" (`src/room/store.rs:454-456`). The fold
enforces it literally — removes filtered `r.2 == owner && r.1 == piece_id`
(`store.rs:501`, `store.rs:509`), unremoves `u.1 == owner` (`store.rs:515-518`),
moves `m.1 == owner && m.0 == piece_id` (`store.rs:530`). `with_pieces` is a
pure function of the op-set (`store.rs:457-551`, called from the pure `view()`
fold at `store.rs:393-408`), so every peer — **including the non-owner who
authored the move** — computes the identical verdict: the op is stored,
durable, and inert. Proven at three levels: single-store
(`non_owner_piece_ops_are_ignored`, `store.rs:987-1014`), inside the
rich-history oracle ("a non-owner move by B ignored", `store.rs:767`,
`786-793`), and cross-node with partitions and deferred delivery (W5,
`tests/l0_convergence.rs:174-220`: "non-owner move is ignored" asserted after
`assert_converged`). The data was never the bug.

### 1.2 The drag handlers write everything except the store's opinion

The document-level `pointerup` handler (`src/web/keyboard.rs:952-1084`) does
three kinds of write on drop:

1. **Dispatch** (correct): `remove_native_piece` on hole-drop
   (`keyboard.rs:988`), `move_native_piece` on a valid key delta
   (`keyboard.rs:1038`) → `ClientCommand::{RemovePiece,MovePiece}`
   (`src/web/app.rs:287-291`, `278-285`) → host `dispatch`
   (`src/web/browser_host.rs:219-226` — note: **no ownership check**; any
   author's move/remove is signed and committed) → `commit_room_op`
   (`browser_host.rs:1283-1308`).
2. **Optimistic adapter write** (the first divergence): `state_up.room
   .lock_mut().remove_piece(...)` at `keyboard.rs:989` and `.move_piece(...)`
   at `keyboard.rs:1039` — **ungated**. The sibling add-paths already learned
   the lesson: both `PutPiece` drops guard their optimistic write with
   `if !state.native_backend` (`keyboard.rs:1186-1189`, `1310-1311`,
   comment: "Optimistic legacy view; the native snapshot replaces it once
   signed"). Move and remove never got the guard, so in every connected mode
   the render adapter is mutated with state the store may not back.
3. **Imperative DOM reposition** (the second divergence, and the deeper one):
   `keyboard.rs:1049-1079` computes `new_pitch` **locally** (start pitch +
   drag delta, `keyboard.rs:1028-1047`), then writes it straight into the
   sink the web component consumes — `set_attribute("data-key", …)` and
   `data-original-pitch` at `keyboard.rs:1061-1068`, then a deferred
   `remove_property("position"/"left"/"top")` at `keyboard.rs:1070-1079` so
   the element snaps to the locally-chosen key. This runs **regardless of
   mode and regardless of what the store decides**. Even the guard inputs are
   read from the mutable adapter, not the projection: the occupancy check
   `state_up.room.lock_ref().has_piece_at(new_pitch)` (`keyboard.rs:1035`)
   consults state that step 2 itself corrupts.

### 1.3 Why nothing ever snaps back

The projection is delta-driven end to end. A local commit folds a fresh view
and diffs it against the host snapshot (`commit_room_op` →
`apply_room_view`, `browser_host.rs:1307`, `909-991`); pieces emit
`PieceUpserted`/`PieceRemoved` only where the view *differs*
(`browser_host.rs:952-974`). For a non-owner's `MovePiece` the view is
**identical before and after** (§1.1), so `apply_room_view` emits zero
events, `apply_native_event` never fires, `project_native_snapshot`
(`app.rs:365-474`) never re-runs, and the optimistic writes of §1.2 stand
forever — exactly the "second divergence source" the coupling design flagged
(§1.5(c) there): *rejected-as-inert ops produce no view delta, so diff-driven
correction never arrives.* The one artifact that does exist,
`AppEvent::Diagnostic`, reaches only the console (`app.rs:1139-1141`).

Net effect on the mover's screen: the piece sits on the new key (DOM write),
the adapter agrees (optimistic write), MIDI re-syncs from the adapter
(`keyboard.rs:1083` → `sync_midi_toggle_output`, which reads
`self.room.lock_ref()`, `app.rs:887-899`) — while the mover's **own store**,
and every peer, holds the owner's position. The UI is not showing a race; it
is showing fiction.

### 1.4 The imperative surface to be tamed (inventory)

Everything below writes DOM outside any signal binding. This is the sink
inventory the adapter must own:

| Sink | Mechanism today | Driven by |
|---|---|---|
| Keyboard element attrs (`pressed-notes`, `lit-notes`, `notes-in-octave`, `raised-notes`, `pie`) | `set_keyboard_attr` querying the element (`keyboard.rs:51-57`, `111-115`, `130-137`, `140-154`) | `sync_active_pitches` (`keyboard.rs:178-233`) — hand-called from **18 sites** (9 in `app.rs`, 7 in `keyboard.rs`, 2 in `components.rs`) |
| Toggle / piece / voice overlays | create/remove raw `div[data-key-overlay]` children (`keyboard.rs:317-357`, `361-403`, `407-473`) | same |
| Clef indicators | get-or-create `div[data-pitch]` children (`keyboard.rs:237-313`) | same |
| Piece elements | `create_piece_element` (`keyboard.rs:770-831`) + a bespoke stream loop `setup_piece_sync` (`keyboard.rs:656-767`) that diffs by id and pokes `data-key` ("Piece exists - just update data-key if pitch changed", `keyboard.rs:741-752`); also back-writes `state.pieces_locked` (`keyboard.rs:713-715`) | `RoomEvent` stream off the yrs adapter |
| Drag lifecycle | element-held state (`data-dragging`, `data-pointer-id`), inline `position:fixed/left/top` (`keyboard.rs:844-938`), document-level pointerup/cancel that re-query the element by selector (`keyboard.rs:968-975`, `1103-1111`), thread-local `ACTIVE_DRAGS`/`EMOJI_DRAGS` (`keyboard.rs:37-42`) | pointer events + local math |
| Delete hole | `show_delete_hole`/`hide_delete_hole` create/remove by id (`keyboard.rs:60-92`) | drag handlers |
| Emoji drag ghost | body-appended element moved per pointermove (`keyboard.rs:1201-1231`, `1243-1273`) | pointer events |
| Debug tripwire | a per-piece `MutationObserver` hunting for whoever sets `data-pitch` (`keyboard.rs:791-821`) | the fact that nobody knows who writes what — the tell that single-writer discipline is missing |

And the external consumer these sinks feed, extracted from the shipped
bundle (`assets/all-around-keyboard.esm.min.js`; offsets are into the
minified single-line file):

- The component runs a `MutationObserver` **on itself** with
  `childList: true, subtree: false` and `attributeFilter: ["data-pitch",
  "data-key", "data-radius", "data-wave-number", "data-wave-amplitude",
  "data-wave-phase", "data-key-overlay"]` (~byte 20100–21100). Attribute
  changes re-run `_updateIndicator`; child-list changes re-run
  `_updateIndicators`/`_updateOverlays`.
- `_updateIndicator` reads `dataset.key`/`dataset.pitch`, computes polar
  position, writes `--indicator-x/y` custom properties and sets
  `data-positioned` (~byte 22600–25200); slotted CSS keeps an indicator
  `visibility: hidden` until `data-positioned` appears (~byte 15400–15900).
- `getNoteAtPoint(x, y)` is the component's public hit-test (~byte 25200),
  already wrapped at `keyboard.rs:1338-1350`.

Two facts matter for the design. First, the component's own contract is
*declarative* — the file header says so ("State in via attributes …
Indicator children: `data-pitch`, `data-key` … Events out",
`keyboard.rs:1-6`). Second, because it observes **its own children and their
attributes**, any element that dominator creates and any attribute a
dominator binding writes is picked up identically to a hand-rolled
`set_attribute`. There is no impedance mismatch: **the web component is
already a signal sink; the codebase just isn't feeding it signals.** The
in-repo proof is the pitch indicator, which drives `data-pitch`,
`data-radius`, `class` and `style` on a slotted child *entirely* through
`attr_signal`/`class_signal` (`keyboard.rs:598-635`) — the one piece of this
file that has never had a divergence bug.

---

## 2. The adapter: signals out, bound effects in

### 2.1 Shape: three layers with one-way writes

```
  RoomStore (authoritative op-set)
      │  fold + diff (host)                     [exists: browser_host.rs:909-991]
      ▼
  ProjectedRoom — keyed, per-facet reactive read model     [new, §2.2]
      │  signals (read-only handles)
      ▼                                    ┌── GestureState (ephemeral,
  render = pure fn(projected ⊕ gesture) ◀──┤    presentation-only; §4)
      │  bound effects (§2.3)              └── written ONLY by input handlers
      ▼
  DOM / web component / MIDI  (sinks — written ONLY by bindings)
      ▲
      └── input handlers: read projection, write GestureState, dispatch
          ClientCommands. They own NO element handles and write NO DOM.
```

The rule in one sentence, matching the coupling design's §2: *durable state
flows store → projection → effect; the only UI-writable state is the
explicitly-ephemeral gesture layer; and the DOM is written exclusively by
effects declared at element-construction time.*

### 2.2 `ProjectedRoom`: the reactive OUT surface

Today the "projected read model" is smeared across four places the UI reads
inconsistently: `native_snapshot: Mutable<Option<AppSnapshot>>`
(`app.rs:61`, read by `degree_is_active`, `app.rs:262-270`), the yrs adapter
`state.room` (`app.rs:77`, read by every `sync_*`), the loose
`pieces_locked: Mutable<bool>` (`app.rs:135` — written by the projection at
`app.rs:465` *and* back-written by `setup_piece_sync` at
`keyboard.rs:713-715` *and* by `lock_button` at
`src/web/components.rs:81-84`), and `tuning` (`app.rs:79`, also written by
the SCL editor directly, `components.rs:427`). Collapse the read side into
one facade, written by exactly one function:

```rust
// src/web/projection.rs (new)
use futures_signals::signal::{Mutable, ReadOnlyMutable};
use futures_signals::signal_map::MutableBTreeMap;

/// Projected per-piece view-model. The `pitch` cell is written ONLY by
/// `ProjectedRoom::apply`; element identity is the PutPiece OpId, so the DOM
/// node for a piece is stable across moves (key-stable rendering, §2.3).
pub struct PieceVm {
    pub id: OpId,
    pub owner: AuthorId,
    pub owned_by_me: bool,          // derived once: owner == local author
    pub emoji: String,
    pub pitch: Mutable<i32>,        // absolute pitch, adapter units (app.rs:432-441)
}

pub struct ProjectedRoom {
    pieces: MutableBTreeMap<OpId, Rc<PieceVm>>,
    degrees: MutableBTreeMap<u8, DegreeVm>,      // pc → authors (attribution)
    voices: MutableBTreeMap<AuthorId, VoiceVm>,
    pieces_locked: Mutable<bool>,
    tuning: Mutable<Tuning>,
}

impl ProjectedRoom {
    /// THE writer. Private to the projection path: diffs the snapshot into
    /// keyed updates (insert/remove keys; in-place `pitch.set_neq` for moves).
    fn apply(&self, snapshot: &AppSnapshot, local: AuthorId) { … }

    /// Read-only handles for everyone else (§3.1).
    pub fn pieces(&self) -> PiecesReadOnly<'_> { … }
    pub fn pieces_locked(&self) -> ReadOnlyMutable<bool> { self.pieces_locked.read_only() }
    …
}
```

`apply` is called from `project_native_snapshot` (`app.rs:365-474`) — the
same single funnel that already rewrites the yrs adapter — so this adds no
new writer, it *names* the existing one. Keyed diffs matter: a
`MutableBTreeMap` insert/remove emits per-key `VecDiff`s to subscribers
(`futures-signals-0.3 signal_map.rs:806-1000`), and a move touches only that
piece's `pitch: Mutable` — no whole-keyboard re-render per event, which is
precisely what `setup_piece_sync` hand-rolls today with its
`HashMap<String, HtmlElement>` (`keyboard.rs:663-665`).

Two deliberate properties:

- **It is the hhhs-reactive landing pad.** A `ProjectedRoom` facet has the
  same shape as an `hhhs-reactive` view: keyed rows, diffed per epoch
  (`Revision{added, retracted, at}` / `signal_vec_view`,
  `hhhs-reactive/src/lib.rs:63-67`, `245-265`). When the store grows a
  `Growth` handle (its `MemDagStore` is private today, `store.rs:115`; the
  coupling design §5 defers that exposure), `apply` is replaced by binding
  the facets to `signal_vec_view(store, pieces_view)` — and *nothing above
  the facade changes*. Until then the facade is fed by the existing
  snapshot pipeline and works in all three runtime modes.
- **It subsumes the adapter reads.** `emoji_picker` reading
  `available_emojis` out of the yrs doc inside a signal map
  (`components.rs:93-101`), `sync_midi_toggle_output` reading
  `room.lock_ref()` (`app.rs:887-899`), `degree_is_active` reading the raw
  snapshot — all become facade reads. The yrs `RoomState` then has no
  UI readers left and shrinks to what the coupling design already scoped for
  it (offline persistence until its step 4, then retirement).

### 2.3 The effect discipline: **bound effects** (`effect(signal, sink)`)

Name the rule: **every imperative write is an *effect bound to a signal at
construction time*; no handler, no `sync_*` free function, no thread-local
ever writes a sink.** Dominator's builder methods *are* this pattern for the
common cases, and they are already proven against this exact web component
by the pitch indicator (`keyboard.rs:598-635`):

| Sink class | Bound effect | Replaces |
|---|---|---|
| Element attribute | `.attr_signal("data-key", sig)` — `Option<String>`-shaped, so `None` *removes* the attribute (dominator `dom.rs:1298`) | `set_attribute`/`remove_attribute` choreography (`keyboard.rs:1061-1068`, `741-752`) |
| Inline style | `.style_signal("left", sig)` (`dom.rs:1530`) | `style().set_property/remove_property` pairs (`keyboard.rs:875-896`, `1070-1079`) |
| Class toggling | `.class_signal("hidden", sig)` (`dom.rs:1410`) | `class_list().add_1/remove_1` (`keyboard.rs:270-312`) |
| Keyed child sets | `.children_signal_vec(facet.entries_cloned().map(render))` (`dom.rs:1187`; `entries_cloned`, futures-signals `signal_map.rs:867`) | every create/remove-children loop: overlays (`keyboard.rs:317-473`), pieces (`setup_piece_sync`), delete hole (`keyboard.rs:60-92`), ghost (`keyboard.rs:1209-1230`) |
| Singleton child | `.child_signal(sig)` (`dom.rs:1170`) | get-or-create clef elements (`keyboard.rs:242-266`) |
| Non-DOM sinks (MIDI, and any sink dominator has no combinator for) | an **effect task** declared beside the element/owner: `spawn_local(sig.for_each(move \|v\| { sink.apply(v); ready(()) }))`, or `.future(…)` on the owning Dom so the effect's lifetime is the element's (`dom.rs:966`) | the 11 hand-placed `sync_midi_toggle_output` calls and 18 `sync_active_pitches` calls |

The whole `sync_active_pitches` function — the "one big repaint, please call
me after every mutation" effect — dissolves into per-facet bindings on the
`pitch_keyboard` builder (`keyboard.rs:583-653`), which is *already* the
single place the component element is constructed:

```rust
html!("all-around-keyboard", {
    // state in via attributes — the component's own contract (keyboard.rs:3-6)
    .attr_signal("notes-in-octave", projected.tuning().signal_ref(|t| t.pitch_class_count().to_string()))
    .attr_signal("raised-notes",    projected.tuning().signal_ref(|t| notes_to_json(&compute_raised_notes(t))))
    .attr_signal("pressed-notes",   always("[]".into()))          // overlays carry presence today (keyboard.rs:202-203)

    // keyed child effects — the component's MutationObserver consumes these (§1.4)
    .children_signal_vec(projected.degrees().entries_cloned().map(toggle_overlay_dom))
    .children_signal_vec(projected.voices().entries_cloned().map(voice_overlay_dom))
    .children_signal_vec(projected.pieces().entries_cloned()
        .map(clone!(gestures => move |(id, vm)| piece_dom(id, vm, gestures.for_piece(id)))))
    .child_signal(clef_signal(&projected).map(clef_doms))          // min/max over voices ∪ pieces (keyboard.rs:214-229)
    .child_signal(gestures.any_piece_drag().map(|d| d.then(delete_hole_dom)))

    // gesture input — listeners owned by the element, no forget(), no re-query
    .global_event(clone!(gestures => move |e: events::PointerMove| gestures.pointer_move(&e)))
    .global_event(clone!(state, gestures => move |e: events::PointerUp| {
        if let Some(cmd) = gestures.pointer_up(&e, &projected) { state.dispatch(cmd); }
    }))
    .global_event(clone!(gestures => move |e: events::PointerCancel| gestures.pointer_cancel(&e)))
})
```

(`global_event` registers on the window but is owned by the Dom,
`dominator dom.rs:950` — replacing the document-level
`add_event_listener…forget()` pairs at `keyboard.rs:1085-1086`,
`1137-1139`, `1271-1273`, `1316-1317`, `1332-1334`, which today leak and
survive any future teardown.)

A piece renders as a pure function of its projected cell **composed with**
its gesture cell:

```rust
fn piece_dom(id: OpId, vm: Rc<PieceVm>, gesture: ReadOnlyish<PieceGesture>) -> Dom {
    // presented_key: Option<i32> — None while a fixed-position drag owns the visual
    let presented_key = map_ref! {
        let pitch   = vm.pitch.signal(),
        let gesture = gesture.signal_cloned() =>
        presented_key(*pitch, gesture)          // pure; unit-tested natively (§6.3)
    };
    html!("div", {
        .class("piece-indicator")
        .attr("data-piece-id", &id.to_hex())
        .attr("data-emoji", &vm.emoji)
        .text(&vm.emoji)
        .attr_signal("data-key", presented_key.map(|k| k.map(|k| k.to_string())))
        .style_signal("position", gesture.signal_ref(drag_position))   // "fixed" while Dragging, None otherwise
        .style_signal("left",     gesture.signal_ref(drag_left))
        .style_signal("top",      gesture.signal_ref(drag_top))
        .class_signal("dragging", gesture.signal_ref(|g| g.is_dragging()))
        .event(clone!(gesture => move |e: events::PointerDown| gesture.begin(&e)))
    })
}
```

Everything `create_piece_element` + `setup_piece_drag_handler` +
`setup_document_drag_handlers` does imperatively (`keyboard.rs:770-1140`) is
here as five bindings and three gesture-writes. The debug `MutationObserver`
(`keyboard.rs:791-821`) is deleted, not ported: under single-writer bindings
the question it exists to answer — *who set this attribute?* — has exactly
one possible answer per attribute, statically.

One honest wrinkle, kept **inside** the sink: the old code deferred
stripping `position:fixed` until the component had repositioned the element,
and cleared `data-positioned` so it stays hidden meanwhile
(`keyboard.rs:1058-1079`). That is sink choreography, not state, and it
stays local to `piece_dom`: a narrow `effect(gesture-transition signal,
with_node sink)` that clears `data-positioned` on Dragging→(Settling|Idle)
and lets the component re-set it (§1.4). The escape hatch is allowed
precisely because it is *declared on the element* and driven by a signal —
the discipline is single-writer-per-sink, not "no imperative code exists."

### 2.4 Reads from the DOM stay reads

`get_key_at_point` → `getNoteAtPoint` (`keyboard.rs:1338-1350`) is a query,
not a write; the gesture layer keeps using it for hit-testing. The rule
constrains *writes to* sinks, and imperative *reads of* layout are exactly
what a hit-test is.

---

## 3. The invariant, extended to imperative sinks

The coupling design's §2 made "should not mutate" into "cannot mutate" for
`state.room` via privacy + `ReadOnlyMutable`. The equivalent for the
imperative UI has three legs:

### 3.1 Read-only facade handles

`ReadOnlyMutable<T>` covers the scalar facets (`pieces_locked`, `tuning`).
`MutableBTreeMap` has no ready-made read-only twin, so the facade wraps it —
ten lines, same capability move as `VerifiedOp`'s private fields
(`src/room/ops.rs:352-355`):

```rust
pub struct ReadOnlyMap<K: Ord + Clone, V: Clone>(MutableBTreeMap<K, V>);
impl<K: Ord + Clone, V: Clone> ReadOnlyMap<K, V> {
    pub fn entries_cloned(&self) -> impl SignalVec<Item = (K, V)> { self.0.entries_cloned() }
    pub fn signal_map_cloned(&self) -> impl SignalMap<Key = K, Value = V> { self.0.signal_map_cloned() }
    pub fn lock_ref(&self) -> … { self.0.lock_ref() }
    // no lock_mut — structurally.
}
```

`ProjectedRoom`'s mutable fields are private; `apply` is the one writer,
called only from the projection funnel. Sibling modules (`keyboard.rs`,
`components.rs`) get `ReadOnlyMap`/`ReadOnlyMutable` and can no longer
compile a back-write like `state.pieces_locked.set(...)`
(`keyboard.rs:713-715`, `components.rs:81`).

### 3.2 Element handles never escape the view module

The reason today's handlers *can* write `data-key` is that they can reach
elements: `get_keyboard()` re-queries the document (`keyboard.rs:51-57`),
and the pointerup handler re-finds the piece by selector
(`keyboard.rs:968-975`). Under §2.3 all elements are constructed and bound
in one place. Enforce the boundary with the module split:

```
src/web/keyboard/
    view.rs      — the html! builders + bound effects. The ONLY module that
                   imports web_sys element types. Nothing here is pub except
                   pitch_keyboard(state) -> Dom.
    gesture.rs   — GestureState + the pure transition function (§4). No web_sys
                   imports at all (compiles and unit-tests natively).
    hit.rs       — get_key_at_point / is_over_delete_hole reads.
```

`gesture.rs` physically cannot poke the DOM (it can't name an element type),
and nothing outside `view.rs` holds a node. `ACTIVE_DRAGS`' element-flavored
duties (attributes as drag state, style writes) disappear; what remains of
it is data in `GestureState` (§4).

### 3.3 Backstop: lint the raw sink calls

Rust privacy can't forbid `web_sys::Element::set_attribute` globally — but
clippy can make bypass a CI failure instead of a review comment:

```toml
# clippy.toml
disallowed-methods = [
  { path = "web_sys::Element::set_attribute",    reason = "DOM writes are bound effects; see docs/research/reactive-effectful-ui-adapter-design.md §3" },
  { path = "web_sys::Element::remove_attribute", reason = "…" },
  { path = "web_sys::CssStyleDeclaration::set_property", reason = "…" },
]
```

with `#[allow(clippy::disallowed_methods)]` granted to exactly
`web/keyboard/view.rs` (and dominator's own internals are outside the lint's
scope). This is the imperative-sink analog of "`room` goes private": the
mechanical guarantee is privacy (§3.1–3.2); the lint catches the one hole
privacy can't close.

Together with the coupling design's step 5 (`state.room` private,
`pub fn room() -> ReadOnlyMutable<RoomState>`, its §2.1), the full handler
capability set becomes: *read-only projection + gesture mutables + dispatch*.
Nothing else is reachable, so "UI mutates presented state" is a compile
error everywhere except the one visibly-ephemeral place it is the point
(§4).

---

## 4. The drag lifecycle over a reconciling store

### 4.1 The model

A drag is a **local, ephemeral, presentation-only gesture**. It is never
written to `state.room`, never staged, never signed — in the rollback
taxonomy it sits *below* `AbandonStaged` (`reactive-rollback-api-design.md`
§1.2): abandoning it isn't even a rollback because no data layer ever
learned of it. Durability enters at exactly one point: the drop dispatches a
`ClientCommand`, and then the gesture **releases the visual back to the
projection**. Whatever the store decides — accept (owner) or inert
(non-owner, §1.1) — the piece renders the store's answer, so the owner-gated
rejection snaps back *for free*, by construction rather than by a
correction event that §1.3 showed will never come.

```rust
// gesture.rs — pure data; no web_sys
pub enum PieceGesture {
    Idle,
    Dragging { pointer_id: i32, x: f64, y: f64, start_pitch: i32, over_hole: bool },
    /// Post-drop optimism, explicitly modeled and time-bounded — the
    /// "pending overlay" the coupling design sanctioned (its §2 latency
    /// note) instead of a silent write into the projected model.
    Settling { expected_pitch: i32, deadline_ms: f64 },
}

pub enum GestureVerdict { None, Dispatch(ClientCommand) }

/// THE transition function. Pure: (state, event, projected-facts) → (state, verdict).
/// projected-facts = { locked, owned_by_me, projected_pitch, key_at_point, over_hole }
/// — all read from ProjectedRoom / hit-tests, never from the yrs adapter.
pub fn step(g: PieceGesture, ev: GestureEvent, facts: &Facts) -> (PieceGesture, GestureVerdict);
```

Transitions, replacing `keyboard.rs:952-1140` clause by clause:

| Event | Today (imperative) | New (gesture + projection) |
|---|---|---|
| pointerdown on piece | element attrs + inline fixed positioning + `remove_attribute("data-key")` + `show_delete_hole()` (`keyboard.rs:844-898`) | `Idle → Dragging{…}`. The fixed positioning, the missing `data-key`, and the hole's visibility are all *renderings of the Dragging state* (§2.3 bindings) |
| pointermove | `set_property("left"/"top")` (`keyboard.rs:906-933`) | update `Dragging.x/y` (a `Mutable` write; `style_signal` follows) |
| drop on hole, unlocked, owned | dispatch + adapter write + style reset (`keyboard.rs:987-1001`) | `Dispatch(RemovePiece)`; `→ Idle`. The element disappears when the projection drops the key (`children_signal_vec` removal) — within one host tick for an accepted local commit, since `commit_room_op → apply_room_view → project` is one funnel (`browser_host.rs:1291-1307`) |
| drop on valid key, delta ≤ 5, target free, owned | dispatch + adapter write + **local** `data-key` write (`keyboard.rs:1028-1068`) | `Dispatch(MovePiece{expected})`; `→ Settling{expected, deadline}` |
| drop, no valid target / delta 0 / not owned / locked | style reset to locally-remembered `start_pitch` (`keyboard.rs:1043-1047`, `1125-1133`) | `→ Idle` — renders the *projected* pitch, which is the only correct "back" |
| pointercancel | 15 lines of attribute/style undo (`keyboard.rs:1114-1135`) | `→ Idle` |

`presented_key` (the composition in §2.3) is then trivially total:

```rust
fn presented_key(projected_pitch: i32, g: &PieceGesture) -> Option<i32> {
    match g {
        PieceGesture::Dragging { .. }                 => None,              // fixed-position visual owns it
        PieceGesture::Settling { expected_pitch, .. } => Some(key_index(*expected_pitch)),
        PieceGesture::Idle                            => Some(key_index(projected_pitch)),
    }
}
```

### 4.2 `Settling`: honest optimism with an expiry

Why not release straight to `Idle` on drop? Under the in-page browser host
the accepted move projects within the same event-loop turn, but the dispatch
does round-trip an async channel (`submit_durable`'s mpsc — coupling design
§1.5(b)), and under Tauri it is an IPC round trip. Releasing instantly can
show one stale frame (old position) before the projection catches up.
`Settling` renders the expectation **as data that is visibly provisional and
self-destructs**:

- resolution effect (an `effect(signal, sink)` task per §2.3, owned by the
  piece Dom): `map_ref!{ vm.pitch, gesture }` — when
  `projected == expected` → `Idle` (accept confirmed; rendering is
  identical, so no visual event); when `deadline_ms` passes with the
  projection unmoved → `Idle` (the move was inert or lost; the piece
  visibly snaps home) — optionally raising a UI-surfaced diagnostic, since
  today's only rejection channel is the console (`app.rs:1139-1141`).
- `Settling` is *not* authoritative anywhere: it lives in `GestureState`,
  renders with a `.settling` class if the product wants a shimmer, and no
  other subsystem (MIDI, persistence, peers) can observe it, because none of
  them read gesture state. This is the exact boundary the rollback doc draws
  for R3′: pure projection deletes the optimistic-apply class; what remains
  of "pending" is an explicitly modeled affordance
  (`reactive-rollback-api-design.md` §2 R3′, §5.3 — and if Tier 1's
  `IntentHandle.phase()` ever lands, `Settling`'s resolution predicate
  upgrades from "pitch matched / deadline" to the real phase signal with no
  UI change).

v1 may ship with `deadline_ms ≈ 250` and no shimmer; the state exists so the
optimism is *typed*, bounded, and cannot outlive its welcome — the three
properties the `keyboard.rs:1049-1079` write has none of.

### 4.3 Dispatch gating is advisory, and read from the projection

The old handler's guards read the mutable adapter (`has_piece_at`,
`keyboard.rs:1035`, `1181`, `1305`; `pieces_locked.get`, `keyboard.rs:987`,
`1168`, `1293`). Under the facade they read projected facts:

- `locked` — from `projected.pieces_locked()` (authoritative:
  `RoomConfigChanged`, `browser_host.rs:976-985`).
- `owned_by_me` — from `PieceVm.owned_by_me` (`PieceSnapshot.owner`,
  `src/client.rs:117`, vs the local author). While pieces are owner-gated
  (§5), the gesture layer should **not dispatch** a move/remove for a piece
  it can prove inert — dispatching it would append a durable, permanently
  ineffective op (§1.1) and then eat the `Settling` timeout. Whether the
  *affordance* (drag at all) is offered on peers' pieces is a §5 product
  question; either way this predicate is derived from projected data, so it
  is a UI courtesy, not a second implementation of the store rule — the
  fold remains the only authority.
- occupancy — note in passing: `has_piece_at` is UI-only; the store has **no
  occupancy rule** (`with_pieces` never compares pitches across pieces,
  `store.rs:493-549`), so two concurrent drops onto one key converge to two
  co-located pieces on every peer. The advisory check keeps the local flow
  tidy but cannot promise the invariant. If one-piece-per-key should be
  *true* rather than usual, that is a store-rule/product decision adjacent
  to §5 — flagged, not designed here.

The emoji-picker drag (new pieces) folds into the same machine: the ghost
element (`keyboard.rs:1201-1231`) becomes a rendering of an
`EmojiGesture::Dragging` cell (a body-level `child_signal`), `EMOJI_DRAGS`
dies with `ACTIVE_DRAGS`, and drop dispatches `PutPiece` with no optimistic
branch at all — deleting the last two `!native_backend` adapter writes
(`keyboard.rs:1186-1189`, `1310-1311`) once the offline host (coupling
step 4) makes every mode host-backed; until then that gated write moves into
the facade's offline writer, not the handler.

---

## 5. Semantics to surface, not decide: owner-gated vs shared pieces

Degrees went **shared** — the coupling design resolved that anyone may
silence a sounding note, with authorship kept as attribution (its §4.4), and
that is shipped semantics for the tap path. Pieces today are **owner-gated**
(§1.1) and convergent. Both are coherent; they are different answers to
"whose object is on the shared instrument?", and the choice belongs to the
product owner, not this doc.

**Option A — stay owner-gated (status quo).**
- Semantics: only the owner's ops affect a piece; everyone converges on the
  owner's positions (`store.rs:454-456`; tests §1.1). Nothing changes in the
  store.
- Requires exactly what §4 builds: rejection must be *rendered* (snap-back)
  and ideally *explained* (diagnostic), because inert ops emit no deltas
  (§1.3). The affordance question follows: with gating kept, prefer not
  offering drag on peers' pieces (`owned_by_me == false` → no pointerdown
  begin; visual affordance via CSS), so the snap-back path is reserved for
  races (ownership changed mid-gesture is impossible — ownership is the
  immutable PutPiece author — so in practice only lock races remain).
- Cost: "why can't I move the cactus?" — a per-object permission on an
  otherwise fully shared instrument, the same asymmetry that made a peer's
  note un-turn-off-able before steps 1–2.

**Option B — go shared (any author moves/removes).**
- This is a **store-rule change in `with_pieces`, and not a one-line one**:
  the current resolution is a *per-owner seq register* (greatest `seq` of
  the owner, `store.rs:505-538`), and `seq` values are per-author log
  positions — incomparable across authors. A shared rule must switch the
  piece's position (and liveness) to the same machinery the cross-author
  registers already use: causal maxima over the piece's move-writes with
  the max-entry-hash tiebreak (`register::resolve`, used for tuning/config
  at `store.rs:579-604`), and observed-remove/unremove for lifecycle (the
  degree pattern, `store.rs:435-448`, rather than owner-seq). Converges by
  the same arguments; concurrent moves of one piece resolve to one
  deterministic winner; a remove kills only the causal past it observed, so
  move-vs-remove races behave like degree add-wins.
- Consequences: anyone can rearrange or delete anyone's piece (the shared
  `pieces_locked` config, `store.rs:585-593`, remains the blunt room-wide
  guard); ownership becomes pure attribution (render the owner as color /
  provenance, as `DegreeAdded.authors` already flows for degrees,
  `client.rs:191-194`); the entire "silently inert op" class for pieces
  disappears — every dispatched move means something, so §4's snap-back
  path degenerates to race-handling only.
- Cost: a real op-schema-adjacent change (fold rules; op alphabet is
  untouched since `MovePiece`/`RemovePiece` already carry no owner
  assumption — `ops.rs` needs nothing), plus migration thought for rooms
  with histories authored under the old fold. And a musical-social one:
  pieces stop being "mine".

**Recommendation (flagged as the user's call):** the same instrument-first
logic that decided shared degrees points to **shared pieces** — the keyboard
is one surface, and per-object vetoes on a jam surface are exactly the shape
of bug this family keeps producing; `pieces_locked` already exists as the
consent mechanism. But pieces carry identity in a way degrees don't, and
"someone deleted my cactus" is a real social cost — this is a product
decision, and nothing in §2–4 depends on it. The adapter is
semantics-agnostic by design: change `with_pieces`, and the same projection,
gesture machine, and bindings render the new rule with zero UI edits
(affordance gating in §4.3 flips from `owned_by_me` to `!locked`).

---

## 6. Tests

### 6.1 Store level: the two-peer convergence scenario (the user's ask)

Documents owner-gating as a *cross-peer* invariant in its minimal form and
pins "the data was never the bug." W5 already proves a superset with
three peers, partitions and deferred delivery
(`tests/l0_convergence.rs:174-220`); this is the canonical two-peer
statement, next to it:

```rust
/// W17 — the drag-divergence scenario at the data layer: A owns a piece,
/// non-owner B moves it, both ingest both ops. BOTH converge on the owner's
/// position — including B, whose own store never showed the move. Any UI
/// that displayed B's move was showing state without data (see
/// docs/research/reactive-effectful-ui-adapter-design.md §1).
#[test]
fn w17_non_owner_move_converges_to_owner_position() {
    let mut net = SimNet::new(6, &["A", "B"], Policy::Fifo);   // support/mod.rs:200
    let put = net.act("A", PutPiece { emoji: "🌵".into(), pitch: tet_pitch(60) });
    net.step_until_quiescent();                                 // support/mod.rs:551
    let piece = op_id_of(&put);                                 // support/mod.rs:639
    net.act("B", MovePiece { piece, pitch: tet_pitch(64) });
    net.step_until_quiescent();
    net.assert_converged();                                     // support/mod.rs:589 — views, hashes, pending, oracle
    for name in ["A", "B"] {
        let view = net.view(name);
        assert_eq!(view.pieces[&piece].pitch, tet_pitch(60),
                   "{name} holds the owner's position");
    }
}
```

### 6.2 Store level: the projection-silence lemma

The mechanism of §1.3, pinned so a future "optimize the diff" change can't
un-document it: an inert op produces **no view delta**, therefore a
diff-driven projection emits **no correction** — the reason optimistic UI
writes can never be repaired after the fact. Store-level (the host is
wasm-only; the lemma isn't):

```rust
#[test]
fn non_owner_move_produces_no_view_delta() {
    // B's store: ingest A's put, snapshot the view, commit B's own move.
    …
    let before = store.view();
    store.commit(&b_key, TOPIC, now, MovePiece { piece, pitch: tet_pitch(64) }); // store.rs:379
    assert_eq!(store.view(), before,
        "inert op ⇒ zero delta ⇒ a diff-driven projection has nothing to say; \
         any snap-back must come from rendering the projection, not from events");
}
```

(Seams: `Peer`/`tet_pitch` from `src/room/test_support.rs:88-111`, `45`;
this can live in `store.rs`'s test module beside
`non_owner_piece_ops_are_ignored`, `store.rs:987-1014`.)

### 6.3 Gesture machine + composition: native unit tests

`gesture.rs` is pure (§3.2), so the drag lifecycle tests run under plain
`cargo test --lib` — no wasm, no DOM:

- **transition table**: drop-on-valid-key ⇒ `(Settling{expected},
  Dispatch(MovePiece))`; drop while `locked` ⇒ `(Idle, None)`; drop with
  `!owned_by_me` (while §5 stays Option A) ⇒ `(Idle, None)` *and no
  dispatch*; hole-drop ⇒ `(Idle, Dispatch(RemovePiece))`; cancel from any
  state ⇒ `Idle`.
- **`presented_key` composition** (the §4.1 function): `Idle` renders the
  projected pitch; `Dragging` renders `None`; `Settling{64}` renders 64
  *while the projection still says 60* — then (a) projection updates to 64 ⇒
  resolution effect yields `Idle`, presented key unchanged (accept is
  visually seamless), or (b) deadline passes, projection still 60 ⇒ `Idle`,
  presented key 60 — **the snap-back, asserted as a pure-function fact**.
  Timestamps are injected (`deadline_ms` compared against a passed-in now),
  so no timers in tests.
- **facade apply diffs**: `ProjectedRoom::apply` on a snapshot with a moved
  piece updates only that `PieceVm.pitch` (assert via `signal_vec` diff
  capture — the same `now_or_never` polling discipline hhhs-reactive's own
  tests use, `hhhs-reactive/src/lib.rs:29-32`); an unchanged snapshot emits
  nothing.

### 6.4 End-to-end (wasm / manual matrix)

Extends the coupling design's step-3 matrix: two browser-host tabs — drag
own piece (moves everywhere, no flicker), drag peer's piece (Option A:
either no affordance, or snap-back + diagnostic; never a divergent local
position), hole-drop own/peer, drag during `pieces_locked`, and the
emoji-picker drop. Verify with the DOM inspector that piece elements'
`data-key` mutates only on projection changes — the assertion the deleted
debug `MutationObserver` (`keyboard.rs:791-821`) was groping toward.

---

## 7. Staged plan (each step buildable native + wasm, ordered)

Composition with in-flight work: coupling steps 1–2 are shipped (absolute
tap intent at `keyboard.rs:1389-1419`, `degree_is_active` at
`app.rs:262-270`; `ToggleDegree` gone). Steps below **implement coupling
step 3 for the piece/drag/lock/SCL sites** (the remaining direct writers,
coupling §1.3 table), are independent of its step 4 (offline host — noted
where it deletes residue), and land its step 5 enforcement extended per §3.
Build/test gates per step: `cargo build --lib --features native-net` +
`cargo test --lib --features native-net` + `cargo build --lib --target
wasm32-unknown-unknown --no-default-features --features web-ui`.

1. **Stop the lie (surgical, current architecture).** `keyboard.rs`: gate
   the move/remove adapter writes exactly like the add path
   (`if !native_backend` on `989`, `1039`); replace the locally-computed
   reposition (`1049-1068`) with a restore from the *projection* (read the
   piece's projected pitch — `native_snapshot` pieces, `app.rs:428-443` —
   and set `data-key` from that; accepted moves then arrive as
   `PieceUpserted` → `setup_piece_sync`'s existing update path,
   `keyboard.rs:741-752`). Add tests §6.1 + §6.2. This fixes the observed
   divergence without the rebuild — and every line of it is deleted again by
   step 3, which is why it stays minimal.
2. **`ProjectedRoom` facade** (`src/web/projection.rs`, new;
   `app.rs`): facets + `apply` called from `project_native_snapshot`
   (`app.rs:365-474`); offline writer beside it until coupling step 4
   unifies modes. Migrate the pure *reads*: `emoji_picker`
   (`components.rs:93-101`), `degree_is_active`, `sync_midi_*` inputs.
   Kill the `pieces_locked`/`tuning` back-writes (`keyboard.rs:713-715`,
   `components.rs:81`, `components.rs:427`) — those handlers become
   dispatch-only per coupling §2.2. Tests: §6.3 facade diffs.
3. **Reactive pieces + gesture machine** (`web/keyboard/` split per §3.2):
   `children_signal_vec` piece rendering, `gesture.rs`, delete hole and
   emoji ghost as bindings, `global_event` handlers. Delete
   `setup_piece_sync`, `create_piece_element`, both drag-handler
   installers, `ACTIVE_DRAGS`/`EMOJI_DRAGS`, the debug observer, and the
   step-1 patch (`keyboard.rs:656-1140`, `1199-1335` collapse). Tests:
   §6.3 transitions/composition + §6.4 matrix.
4. **Reactive overlays, clefs, keyboard attrs, MIDI effect.** Replace
   `sync_active_pitches` (18 call sites) and the hand-called
   `sync_midi_toggle_output` (11 sites) with facet bindings and one MIDI
   effect task (§2.3 table). Delete `set_keyboard_attr`,
   `set_pressed_notes`, `set_lit_notes`, the three overlay sync functions
   and `sync_clef_indicators` (`keyboard.rs:111-137`, `178-473`).
   `update_tuning` becomes `attr_signal`s on the builder.
5. **Structural enforcement.** Coupling step 5 (`state.room` private +
   `ReadOnlyMutable`) + facade `ReadOnlyMap` handles + the `clippy.toml`
   disallowed-methods backstop (§3.3) + narrow the now-unreferenced yrs
   mutators (`yrs_state.rs:1010-1092`, `1169`) toward the projection path.
   The compiler and CI now hold the invariant that steps 1–4 made true.
6. **(Decision-gated / later.)** (a) The §5 pieces-semantics call — if
   "shared" is chosen: rework `with_pieces` onto causal registers +
   observed-remove, update the §1.1 tests' expectations, no UI change.
   (b) hhhs-reactive direct binding: expose a `Growth` handle from
   `RoomStore` (dag private today, `store.rs:115`) and swap
   `ProjectedRoom::apply` for `signal_vec_view` facets
   (`hhhs-reactive/src/lib.rs:245-265`), retiring the snapshot→facade hop;
   `Revision.retracted` (`lib.rs:60-67`) then feeds exit animations and
   MIDI note-offs directly. Each is its own proposal.

Steps 1–3 repair and re-found the piece path; 4 finishes the imperative
sweep; 5 locks it; 6 is the architecture converging on its own stack.
