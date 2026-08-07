# Eventually-consistent pitch-sets

**Status:** vision / jumpoff, 2026-08-07. Not a spec. Everything speculative in
here is marked by mood, not by disclaimer — but every mechanism named is real,
and cited. Grounding: walkie-songie at HEAD (`src/room/ops.rs`,
`src/room/store.rs`, `src/web/app.rs`, `src/web/browser_host.rs`,
`src/tuning/`, `src/room/presence.rs`), the companion designs
(`docs/research/ui-state-coupling-design.md`,
`docs/research/reactive-rollback-api-design.md`), and the HHS3 corpus
(`/laboratory/fe-stuff/hhs3-ts`,
`/laboratory/fe-stuff/misc/hhs3-riffcat-relation-2026-07-17/`).

---

## 0. The stuck note is a confession

Every MIDI musician has lived it: a note-off gets lost, and a note screams
until you power-cycle the synth. The stuck note is not a bug in some cable. It
is MIDI telling you what it actually is: a stream of ephemeral, ordered,
anonymous events whose *meaning* is state that lives nowhere. The protocol
says "F# began" and "F# ended" and never once says "F# is sounding" — that
fact exists only as a fragile inference inside every receiver, unowned,
unverifiable, unrepairable. Drop one datagram and the world's receivers
disagree about the music, forever, with no mechanism that could even express
the disagreement, let alone heal it.

Forty years later we know exactly what that failure is called. It's a
replication problem. MIDI is a replication protocol that never admitted it —
one with no identity, no merge function, no anti-entropy, no provenance, and a
consistency model of "hope."

walkie-songie, mostly by trying to let two phones jam, has backed into the
alternative. It does not send note events between peers. It maintains a
**hot pitch-class set**: a live, shared, continuously-reconciling set of
tuning-scoped degrees, held per-author, merged by causal add-wins, carried on
a grow-only signed op-log that any two replicas can reconcile from any state
of disagreement. The sounding surface is not a stream. It is a *value* — one
that many people are allowed to be wrong about temporarily, because the
substrate knows how to make them agree.

This document takes that seriously as a paradigm, not an implementation
detail: what if the successor to MIDI is not a faster event stream but a
reconciling musical state — sets, not messages; convergence, not delivery;
provenance, not channels; *eventual consonance*?

---

## 1. The reframe: music as reconciling state

### 1.1 What the substrate already is

Strip walkie to its skeleton and you find this stack, bottom to top:

- **A per-author, append-only, signed op log.** Every musical contribution is
  a `WalkieOp` in a CBOR envelope, Ed25519-signed via p2panda, chained by
  seq/backlink (`src/room/ops.rs`). Who played which note is cryptographically
  established at ingress, once, and never re-litigated.
- **A causal DAG, not a timeline.** Each op stamps the `observed` frontier —
  the op ids its author had accepted when signing (`VersionedOp.observed`,
  `ops.rs:206-210`). The store lifts ops into an HHHS entry whose `prevs` are
  exactly backlink ∪ observed, so the entry hash is a pure function of the
  signed bytes and identical on every peer regardless of arrival order
  (`src/room/store.rs:1-16`). Wall-clock time is demoted, in the source's own
  words, to "display/tiebreak-of-last-resort only; ordering is causal, never
  wall-clock" (`ops.rs:196-199`).
- **State as a query.** There is no stored "current chord." `RoomStore::view()`
  is a pure fold over the DAG snapshot: pitches are a content-keyed add-wins
  set where an add is live iff no same-key remove causally observed it
  (`with_pitches`, `store.rs:411-452`, via `ReachIndex::is_ancestor`); the doc
  comment is the whole semantics in one line — "A `RemoveDegree` supersedes
  only the adds in its causal past; a concurrent add survives"
  (`ops.rs:16-18`).
- **Convergence as a checkable fact.** RBSR anti-entropy converges peers on
  the entry-hash identity set, and `sync_root` digests it so two peers can
  *prove* they hold the same music (`store.rs:77-96, 194-196`).
- **A deliberate two-tier temporality.** Durable ops for structure; signed,
  sequenced, **leased** presence for the ephemeral — voice preview expires in
  1.5 s if unrefreshed, precisely so "a crash or dropped clear frame expires
  locally instead of leaving a permanent sounding note"
  (`src/room/presence.rs:1-17`). Read that again: walkie already solved the
  stuck note, structurally, by sorting musical facts into *those that are
  state* and *those that are leases*.

That is not a MIDI replacement yet. But it is unmistakably the skeleton of
one, and the skeleton has properties MIDI cannot express at any bitrate.

### 1.2 The claim

**Music-as-collaboration is state, and we have been faking state with
events for forty years.**

An event stream is the right shape for exactly one thing: a single performer
driving a single sound generator over a reliable wire in real time. It is the
wrong shape for everything MIDI has been contorted into since — sessions,
scores, ensembles, DAW projects, network jams — because all of those are
*shared mutable structure*, and shared mutable structure faked by event
replay has no merge story, no offline story, no authorship story, and no
history you can stand inside.

Flip the figure and ground. Let the **reconciling set be the musical object**
— durable, shared, author-attributed, tuning-scoped — and let events be what
they always really were: *edges of a projection*. A note-on is not a fact
about the music; it is the derivative of a fact, the moment a fold's output
gained an element. walkie's own architecture docs already enforce this
direction of flow: the UI "is a direct projection of data," gestures carry
absolute idempotent intent instead of toggles, and the planned MIDI output
re-sources from `Revision{added, retracted, at}` diffs of the fold — note-ons
and note-offs *generated at the edge* from set deltas
(`docs/research/ui-state-coupling-design.md`,
`docs/research/reactive-rollback-api-design.md` §3).

What you buy with the flip:

- **Merge is meaningful.** Two histories of a set have a deterministic union;
  two histories of an event stream have a splice, which is to say a lie.
- **Provenance is musical material.** `pitch_authors` maps every live degree
  to the set of authors holding it (`store.rs:623`,
  `src/web/browser_host.rs:75`). The harmony knows who is in it.
- **Offline is not an error mode.** A peer that vanishes mid-chord and
  returns forty seconds later is just a replica with a fork to reconcile —
  and the fork-meet theorem bounds exactly how far back the reconciliation
  can revise anything (HHS3's revision bound; `hhs3-deep-core-fable.md` §2d).
- **Persistence is not an export.** The op-log *is* the piece. There is no
  "save as"; there is only more history.
- **Time travel is a read, not a feature.** HHS3's deepest move: state is
  `getView(at, from)`, a query at a historical position from a current
  horizon, so "the past" is on-medium and scrubbing is just choosing `at`.
  The 2025 work plan wanted time-travel-by-replay and discovered
  state-as-query had already subsumed it (`hhs3-deep-core-fable.md` §1b).

### 1.3 The hard tension, faced

Real-time ensemble performance wants two things eventual consistency
refuses on principle: **low latency** and **agreed order**. A groove is a
consensus protocol with a 10-millisecond commit window. No amount of CRDT
cleverness makes a transatlantic downbeat simultaneous; physics holds the
gavel.

The resolution is not to argue with physics but to notice that a piece of
music is two objects wearing one name:

1. **The structure** — what is sounding, who holds it, in which tuning, what
   came before: *this* is the shared object, and it tolerates disorder
   beautifully. A chord does not care in what order its notes were agreed
   upon. Harmony is a set; sets commute. This is CALM's monotone fragment
   wearing a tuxedo: adds are coordination-free, and the non-monotone residue
   (removes, tuning changes) is small, tagged, and handled by causal
   observed-remove and registers — exactly the barrier discipline HHS3 built
   (`hhs3-deep-core-fable.md` §2c; `store.rs:414-452, 553-605`).
2. **The performance** — the phase-locked, breath-synchronized *now*: this is
   not shared state at all. It is local rendering of shared state, plus
   ephemeral leased presence for the gestures too fast and too disposable to
   be history (`presence.rs` again). Each peer's audio engine renders its own
   fold *now*, at local latency; the guarantee is not simultaneity but
   *convergence about what the music is* — with the revision bound as the
   contract for how much of the recent past may still be renegotiated.

NINJAM proved twenty years ago that musicians can groove with each other's
*past* — it delays everyone by a bar and calls the result an aesthetic.
Ableton Link proved musicians will happily share a small piece of consensual
session state (tempo and phase) if you make it invisible. The hot-set
paradigm generalizes both: share the structure eventually, render the
performance locally, and let "when" itself become a reconciled coordinate —
because when the score is a causal DAG and reading it is `getView(at, from)`,
playback position, scrub position, and branch point are all just values of
`at`. The metronome may stay a Link-style clock; *everything the metronome
counts through* becomes a set.

---

## 2. The primitive: what a hot pitch-class set is

### 2.1 Versus a MIDI note

A MIDI note is a key number 0–127 on a channel 1–16, born by one event,
killed by another, meaningful only against an implied 12-TET keyboard,
belonging to no one, comparable to nothing.

A hot-set element — take walkie's actual one — is a **holding**:

| facet | walkie's realization |
|---|---|
| content identity | `TunedDegree` — a degree index *bound to a tuning* (`src/tuning/mod.rs`) |
| tuning context | `TuningId` = blake3 of canonical Scala `.scl`/`.kbm` bytes; up to 4096 degrees per period (`tuning/mod.rs:116-137`, `scl.rs:12`). 12-TET is just a built-in constant, not an assumption |
| provenance | the signed add op; surfaced as `pitch_authors: BTreeMap<TunedDegree, BTreeSet<AuthorId>>` (`store.rs:623`) |
| causal position | the entry's place in the DAG; the add's `observed` horizon |
| liveness | a *verdict*, recomputed per fold: live iff no same-key remove causally observed it (`store.rs:435-450`) |
| retraction semantics | observed-remove: you can only kill what you saw; concurrent adds survive (`ops.rs:16-18`) |

Already — today, in the shipping code — a "note" carries identity, tuning,
authorship, causality, and a revisable verdict. The MIDI note carries a
number.

And notice what the set is *not* bound to: not to 12, not to any specific
pitch vocabulary, not even to octave periodicity (the period is whatever the
`.scl` says). When the room retunes, `SetTuning` resolves as a causal-maxima
register and degrees from other tunings are *eclipsed, not reinterpreted* —
they stay in history, invisible under the new tuning, recoverable if it
returns (`store.rs:426-429` filters by active tuning; the eclipse test is
pinned in `store.rs:926-952`). A modulation is a room-wide state change with
an undo.

### 2.2 Versus a chord: the music-theory bridge

Here is the delicious part: music theory got to "the set is the object"
sixty years before distributed systems did. Forte's atonal set theory (1973)
treats a pitch-class set — not the note, not the event — as the analytical
atom: set-classes under transposition and inversion (Tn/TnI), interval
vectors as a set's harmonic fingerprint, Z-relations for sets that share a
fingerprint without sharing a form (the all-interval tetrachords 4-Z15 and
4-Z29, famously). Lewin's transformational theory (1987) then made the
*moves between sets* first-class — his "transformational attitude": don't ask
what interval separates two chords, ask *what characteristic gesture takes me
from one to the other*. Neo-Riemannian theory turned that into an algebra of
operations (P, L, R) walking triads across the Tonnetz.

A hot pitch-class set is this tradition made executable and multiplayer:

- The **set-class machinery becomes a live projection.** walkie already runs
  a small version of it — `src/web/graph.rs` matches the live set against
  scale and chord catalogs; `src/web/solfege.rs` infers modes. Speculate one
  step: a fold-side analyzer that continuously computes the live set's prime
  form, interval vector, and nearest set-classes *with authorship overlaid* —
  Forte numbers as telemetry.
- The **transformational attitude becomes the op alphabet.** Lewin's gesture
  "from s to t" is literally what an intent is: not "the state shall be X"
  but a signed, causally-positioned *move*. A Neo-Riemannian P (C major →
  c minor) is an atomic two-element swap — a `Batch` op, in the vocabulary
  the rollback design already reserves for multi-degree intents
  (`reactive-rollback-api-design.md` §6). The algebra of triadic
  transformations could ship as a library of named batch intents. Theory
  class as protocol surface.

### 2.3 The speculative facets (each with a foothold)

What else can a holding carry? Everything below is speculation with an
existing seam to grow from:

- **Weight / probability.** A holding with weight 0.3 that renderers sample
  or scale to velocity. Foothold: the op-schema evolution discipline —
  "append variants, add fields only as `#[serde(default)]`" (`ops.rs:27-29`)
  — makes `AddDegree { pitch, weight }` a v-next field, not a redesign. A
  weighted hot set is a *distribution over harmony*; a generative agent is
  just another Ed25519 author whose holdings happen to be probabilistic.
- **Lifetime as a spectrum, not a binary.** Today the split is durable op vs
  1.5-second presence lease. Between "the score contains F#" and "a finger is
  touching F# right now" lies a whole continuum — sustain as lease renewal,
  decay as lease expiry, a sostenuto pedal as a lease-to-durable promotion.
  Foothold: the lease machinery exists (`presence.rs`), and the rollback
  design already sorts tentative gestures onto the Draft/lease side of the
  publication boundary on principle (`reactive-rollback-api-design.md` §5.3).
- **Register and voicing.** The set is pitch-*class*; voicing is a rendering
  fact that could ride as a per-holding facet or stay a local renderer
  choice. Foothold: `TunedPeriodicPitch` (degree + signed period) already
  exists and carries the pieces layer (`tuning/mod.rs:80-106`) — the octave
  dimension is modeled, just not yet attached to degree holdings.
- **Expression residue.** The voice path quantizes continuous pitch to the
  nearest degree and computes `cents_deviation` — then throws it away
  (`QuantizeResult`, `tuning/mod.rs:179-190`). That residue is per-note
  expression *already being computed at the boundary between the continuous
  and the discrete*. Keep it (on the presence tier — §8 builds this out)
  and the hot set has MPE-grade inflection as a facet of holdings rather
  than a channel-rotation hack.
- **Sets of sets.** A chord progression is a set of hot sets addressed by a
  temporal coordinate; a score is a reconciling document of them; a *form*
  (AABA, verse/chorus) is a set of sets of sets. The kernel doesn't care —
  it reconciles opaque payloads by causal position. The design question (the
  real one, flagged in §11) is what the temporal coordinate is.

### 2.4 Editing the surface: intents, co-transactions, rollback

The collaboration model is already written, in two design docs whose findings
transfer to the vision wholesale:

- **Every gesture is an absolute intent derived from the projection** — "turn
  this off," never "toggle" — because an involution's meaning races its own
  evaluation and an op-set can't interpret it
  (`ui-state-coupling-design.md` §3; `set_native_degree` /
  `degree_is_active`, `src/web/app.rs:247-270`).
- **Every intent is already a degenerate co-transaction.** The commit stamps
  `observed = store frontier`, so the op carries an implicit precondition —
  "the on-ness I cancel is what my horizon sees" — re-evaluated by every
  replica at-use, deterministically (`ui-state-coupling-design.md` §4.2). The
  cotx-native sharpening is designed and shelved for the next schema move:
  `RetractDegreeAdds { pitch, adds }`, retracting *exactly the holdings the
  gesture was aimed at* (§4.3). HHS3's framing gives this its proper name: a
  co-transaction's validity "is never concluded, only maintained" — a
  greatest-fixpoint over the stream of horizons
  (`hhs3-deep-core-fable.md` §1b). Your note stays yours to justify forever.
- **Rollback is re-projection, and that is the correct musical default.** "A
  pitch toggled off by a peer is not an error condition to be explained — it
  is the music changing" (`reactive-rollback-api-design.md` §3). The Tier 1
  intent-lifecycle layer (EffectMap, supersession attribution, drafts with
  TTLs) exists for the minority of flows that need ceremony — a tuning
  change deserves a toast; a released key deserves nothing.
- **Whose note may you silence?** Decided, and decided *musically*: shared
  observed-remove — anyone can resolve a sounding note, authorship renders
  as color, never as veto (`ui-state-coupling-design.md` §4.4). And the
  ownership-gated alternative isn't dead; it becomes *evaluator policy over
  explicit retracts*, per-room, with zero op changes — HHS3's
  authority-is-the-schema move applied to harmony.

One sentence from the rollback design deserves to be the whole paradigm's
epigraph: **"there is no method that changes a signal without changing
data"** (`reactive-rollback-api-design.md` §5.4). Sound is a signal. Extend
the invariant one layer down and it reads: *nothing sounds that isn't state.*
The inverse of the stuck note.

---

## 3. What the incumbents can't say

Honesty first: MIDI 1.0 is a masterpiece of scope discipline. 1983, 31.25
kbaud, three bytes a note, and it still runs the world. Nothing below is a
sneer. But each incumbent is missing the same organ, and it's the one this
paradigm grows.

- **MIDI 1.0 (1983).** Ordered ephemeral events; state implicit in the
  receiver; no identity, no authorship, no merge, 12-TET-shaped key numbers
  (pitch bend as a channel-wide apology). The session does not exist as an
  object. Two streams cannot be reconciled, only mixed.
- **MPE (2018) and MIDI 2.0 / UMP (2020).** Per-note expression, 32-bit
  resolution, per-note pitch, profiles, bidirectional negotiation (MIDI-CI).
  Genuine progress — at being a *better event stream*. The consistency model
  is unchanged: meaning still lives in receiver state, notes still die when
  their voice does, nothing carries provenance, nothing merges, and a lost
  packet still lies forever. MIDI 2.0 is MIDI 1.0 with more adjectives.
- **OSC (1997).** Names and types over UDP — a fine *syntax* with no
  semantics at all. OSC doesn't know what a note is, let alone what two
  disagreeing replicas of one should do. It's a transport the hot set could
  ride, not an alternative to it.
- **Ableton Link (2016).** The honorable exception: actual shared,
  self-healing, consensual session state — tempo, beat, phase — with a
  peer-to-peer model and no leader. Link is the proof that musicians accept
  reconciling shared state when it's invisible and right. It just stopped at
  the metronome, deliberately carrying zero content. The hot set is "Link,
  but for what's sounding."
- **Network-music practice.** NINJAM buys feasibility by quantizing everyone
  into each other's past — reconciling *time* by delay, sharing no
  structure. Endlesss and loop-based jam apps make async work by exchanging
  frozen audio artifacts. JackTrip moves audio, not meaning.
- **CRDT prior art.** Collaborative music *editors* exist: Flok syncs live-
  coding buffers with Yjs; Soundtrap-style DAWs are server-authoritative
  documents; there's academic work on CRDT score editing. All of them
  reconcile *text or documents about music* — an intermediate notation whose
  performance is downstream and single-player. walkie's ancestor was itself
  a Yjs doc (`src/room/yrs_state.rs`, now a render adapter on death row),
  so this lineage is family, not strawman.

MIDI 2.0 is the strongest incumbent — the one that actually modernized — so
it deserves better than a paragraph. Affordance by affordance:

| MIDI 2.0 feature | what it affords | tutti equivalent | what tutti gains (or trades) |
|---|---|---|---|
| bidirectional MIDI-CI **Property Exchange** | query/set a device's state and capabilities (request/response JSON over SysEx) | nothing to query — the state *is* the shared object; tuning/config are reconciled causal-maxima registers every peer holds (`SetTuning`/`SetConfig`, `store.rs:553-605`) | gains: no polling, no stale answers — state converges to you, provably (`sync_root`). trades: you carry a replica, not a cable |
| **Profiles** (capability negotiation) | two devices agree to speak a feature set ("rotary organ") | the room's tuning + op schema *are* the profile, as replicated state: `TuningId` = blake3 of canonical `.scl`/`.kbm`; `OP_SCHEMA_VERSION` gates the alphabet | gains: the profile is content-addressed, verifiable, and historical — scrub to when the room was 31-EDO. trades: append-only evolution discipline instead of ad-hoc pairwise negotiation |
| **per-note controllers / MPE** per-note pitch + expression | continuous expression bound to a voice instance | per-author, per-holding facets: leased voice presence for live inflection (`presence.rs`), the `cents_deviation` residue (`tuning/mod.rs:179-190`), weight (§2.3) | gains: expression attaches to a shared, *attributed* object — two authors inflect the same pitch class as distinct layers. trades: MIDI 2.0's dedicated per-note stream is leaner at high rates; tutti routes fast expression through leases, never the durable log |
| **32-bit velocity / high-res controllers** | finer value resolution | continuous facets on holdings, CBOR-encoded, added as `#[serde(default)]` fields (`ops.rs:27-29`) | gains: resolution unbounded, and every value carries provenance. trades: a durable facet write is a signed op — bulk continuous data belongs on the presence tier |
| **JR (jitter-reduction) timestamps** | sub-ms send-time stamps for tight playback scheduling | wall-clock demoted to "display/tiebreak-of-last-resort" (`ops.rs:196-199`); the load-bearing coordinate is causal and on-medium — `getView(at, from)` | gains: "when" is queryable, forkable, mergeable; history is addressable. trades: JR solves phase accuracy and tutti deliberately doesn't — performance timing stays local / Link-class (§1.3) |
| **Groups & channels** (UMP 16×16) | routing and multiplexing address space | rooms as signed topics (ops are topic-bound, replay-proof — `ops.rs:224-232`); within a room, authorship layers are the address space — expanded in §3.1 below | gains: cryptographic, unbounded addressing — a "channel" is a keypair with provenance. trades: nothing structural, but none of the 4-bit simplicity either |
| **note-on/off + attribute types** | notes carry articulation data; lifetime = event bracketing | durable holdings (add-wins / observed-remove) vs 1.5 s presence leases — lifetime as a spectrum (§2.3) | gains: stuck notes structurally impossible; silencing kills only what it observed. trades: a holding costs a signed op, not eight bytes |
| **MIDI 1.0 fallback** negotiation | graceful degradation to the 1983 byte stream | the edge bridge: lift note-on/off to absolute intents (`app.rs:866`), project fold `Revision` diffs back out as note events (§5.2) | gains: any MIDI instrument becomes a citizen of the set. trades: the lift is lossy — one author, one tuning, no intent nuance |

Read the trades column honestly and MIDI 2.0 keeps two real crowns: raw
per-note expression bandwidth and sub-millisecond wire latency. Tutti does
not contest the wire — it changes what the wire is *for*. Expression rides
leases; rendering is local; the signed durable tier holds only what deserves
to be history.

### 3.1 Channels, reimagined

One row of that table deserves its own excursion, because it's where the
1983 assumptions run deepest. A MIDI channel is a 4-bit routing tag: sixteen
wires baked into the frame format, and MIDI 2.0's boldest move was to make
it sixteen *groups of* sixteen. The number was never the problem. The *kind*
is the problem: a channel is a static multiplex coordinate with no identity,
no semantics, no owner, and no history. In a peer-to-peer reconciling
substrate, "channel" dissolves into an address space with at least five
richer axes — every one of them with a foothold in walkie's shipping
primitives:

1. **Topics: channels as pub/sub names.** A room is already an arbitrary,
   dynamic, *named* topic — `RoomTopic::from_room_name("quiet-cactus-song")`
   (`src/net/iroh_common.rs`, exercised in `src/net/native.rs:559`), with
   human-memorable adjective-noun-noun names minted from a word list
   (`src/words.rs`), gossip scoped per-topic, and a topic-rendezvous layer
   that auto-peers everyone who names the same room
   (`src/net/rendezvous.rs`, `browser_host.rs:340-343`). The channel is not
   allocated from sixteen; it is *minted by naming it*, and signed ops are
   topic-bound so material cannot replay across channels
   (`verify_signed_op_for_topic`, `ops.rs:594-608`). Unbounded, discoverable,
   forgeproof.
2. **Authorship as address.** Route by self-certifying key, not channel
   number. "Everything Ada holds" is already a first-class query —
   `pitch_authors` partitions the live set by author
   (`store.rs:623, 435-450`) — so every author *is* a channel, one that
   cannot be spoofed (contract property (a), §6). Per-channel analysis
   becomes per-author analysis: the author-partitioned interval vectors of
   §4.3 are exactly "channel meters" for a namespace where the channel is a
   person. A MIDI channel tells you which wire; an author channel tells you
   *who*, with a signature.
3. **Typed channels: the channel as a reconciling sub-object.** A channel
   that carries its own schema, tuning, and capabilities is not an int — it
   is a document. Walkie's room already is one: its tuning is a
   causal-maxima register *of the channel itself* (`SetTuning`), its config
   is channel state (`SetConfig`), and its "profile" is content-addressed
   (`TuningId`, §3's Profiles row). Speculate one step out: per-channel
   facet vocabularies — a percussion channel whose holdings carry no pitch
   facet; a 31-EDO channel nested inside a 12-TET session, each reconciling
   independently and rendered together. MIDI 2.0 negotiates what a channel
   speaks; a typed channel *replicates* what it speaks, verifiably, with
   history.
4. **Intersectional addressing.** Because topic, author, tuning, facet, and
   time are all coordinates of one log, a "channel" can be a *slice*:
   topic ∩ author ∩ facet ∩ frontier. "The F sharps Ada was holding as of
   the bridge" is `getView(at_bridge, from_now)` filtered by author and
   degree — and every term is already expressible (`pitch_authors` for the
   author axis, `TunedDegree` for the pitch axis, the causal frontier for
   `at`). Channels stop being places you route *to* and become standing
   queries you subscribe to — which is precisely the hhhs-reactive shape,
   a `Revision{added, retracted, at}` stream per slice
   (`reactive-rollback-api-design.md` §3).
5. **Capability channels.** The endgame: who may write where, as replicated
   data rather than configuration. HHS3's capability model — grants as
   witness rows, revocation as a barrier-delete re-checked at every horizon,
   delegation as re-validated chains (`hhs3-deep-core-fable.md` §3) — makes
   a channel something you can *hand to someone*: a write-capability grant,
   attenuable into sub-channels ("you may add but not retract," "degrees
   0–11 only"), revocable deterministically even against a racing writer.
   The ephemeral/durable split gives the same namespace a temperature
   axis for free: a leased presence channel (voice today,
   `presence.rs`) beside the durable log, in one address space.

The throughline: **MIDI's sixteen fixed wires become an open, addressable,
reconciling, capability-scoped namespace** in which topic, identity, tuning,
facet, and time are all axes you can slice by — and a "channel" is whatever
slice you can name, query, or grant.

Honesty about distance: axes 1–2 are shipping today; axis 4 is read-side
code over existing data (a filtered fold, no wire change); axis 3 needs the
facet and temporal work of §5.2; axis 5 needs capability machinery walkie
does not carry — HHS3 has it (`hhs3-ts`, Santi's tree, is the source of
truth), but importing it is a real project, not a weekend.

### 3.2 What is genuinely new

The genuinely new thing in the hot-set paradigm is the combination, not any
single part: **the sounding surface itself** (not a score-document, not a
code buffer) as the replicated object; **cryptographic authorship inside the
musical identity layer** (the signed op is the note); **tuning-nativeness at
the protocol floor** (degrees are meaningless without their `TuningId`;
there is no privileged 12); **observed-remove/add-wins as performance
semantics** (your silence only kills what you heard); and **on-medium
time-travel** (the piece's history is addressable state, not an export).
No incumbent has any two of these. The substrate under walkie has all five,
shipping.

---

## 4. Five what-ifs

### 4.1 The chord that crosses the ocean

Three players — Lagos, Berlin, São Paulo — hold one hot set. Each hears their
local fold *now*; the set converges under gossip plus RBSR anti-entropy, and
`sync_root` lets any two of them prove agreement (`store.rs:89-96`). Berlin's
connection dies mid-jam; she keeps playing into her local replica, forty
seconds of offline harmony. On reconnect, reconciliation is a union, never a
revert (`SessionStatus::Divergent` means "re-sync," structurally —
`reactive-rollback-api-design.md` §1.1). Her retraction of the old tonic
kills only the adds it causally observed; the pedal point Lagos added while
she was dark survives on add-wins, exactly per `ops.rs:16-18`. Nobody
experienced an error. The ensemble was partitioned and *the harmony has a
partition-tolerance proof*.

### 4.2 The score you can stand inside

The op-log is the piece — so the piece has a scrub bar. Drag it and you're
choosing `at` in `getView(at, from)`: a pure fold over a causal prefix, no
snapshots, no export (state-as-query; `hhs3-deep-core-fable.md` §1b). Now
fork: branch the DAG at bar 40's frontier, take the quiet version, let your
collaborator take the loud one, and *merge them later* — the union
reconciles, add-wins arbitrates, and the fork-meet bound guarantees the
merge cannot rewrite anything below the branch point. Composition and
version control stop being an analogy. And provenance rides along: scrubbing
a year-old jam, you can ask not just "what was sounding" but "what was *Ada*
holding when this modulation happened" — the answer is a query over signed
history, not a memory.

### 4.3 Who is holding the tension?

Render `pitch_authors` as color and harmony becomes legible as a social
object. The dominant seventh on screen is not an abstraction — its tritone
is visibly *Ada's F and Ben's B*, and everyone can see who has to move for
the resolution. A teacher watches a class build a mode degree by degree,
each holding attributed; the exercise "resolve someone else's suspension" is
literal, because shared observed-remove makes resolving a peer's note a
legitimate musical act, attributed to *you* as the resolver (the explicit
`RetractDegreeAdds` even records whose adds you retracted —
`ui-state-coupling-design.md` §4.3). Analysis gets a new axis nobody's
theory textbook has: the interval vector of the current set, *partitioned by
author*. Counterpoint as a provenance query.

### 4.4 Play-by-mail, converging

A duet conducted entirely offline. You improvise into your replica on a
plane; the signed ops journal locally (`src/room/journal.rs` natively; the
IndexedDB op-journal design for the browser,
`ui-state-coupling-design.md` §5). You export nothing — you hand over the
log, by relay, by sneakernet, by literally mailing a USB stick; RBSR
reconciliation is transport-agnostic set repair. Your partner's replica
unions your week of harmony with hers; conflicts resolve by the same
add-wins/observed-remove verdicts as a live jam, because *there is no
difference in kind between a 10 ms race and a 10-day one* — causality, not
wall-clock, orders the piece (`ops.rs:196-199`). Correspondence chess for
harmony, with deterministic convergence instead of a rulebook argument.

### 4.5 The generative tenant

A generative agent joins the room as what it structurally is: another
author with an Ed25519 key. Its holdings are weighted (§2.3); its adds carry
co-transactional preconditions — "this cluster stands only while the room's
tuning register resolves to 31-EDO" — void-engine-evaluated at every
horizon, so when a human retunes the room, the agent's tuning-specific
material is *deterministically eclipsed on every replica at once*, no
coordination, no kill switch (the eclipse semantics already pinned at
`store.rs:926-952`; the void/precondition machinery per
`hhs3-deep-core-fable.md` §1). Don't like what it played last night? Scrub
back, branch before its entrance, merge forward without it. Its entire
contribution remains in history, signed, attributable, and revocable-by-
supersession — algorithmic co-performance with an audit trail.

---

## 5. The bridge: from here to there

### 5.1 What walkie already has (the vision's floor)

| vision component | shipping today |
|---|---|
| the hot set itself | add-wins `TunedDegree` set with per-author attribution (`store.rs:411-452, 623`) |
| provenance | signed per-author op logs; `pitch_authors` to the UI (`ops.rs`, `browser_host.rs:75`) |
| tuning-nativeness | `TuningId`-scoped degrees, Scala `.scl`/`.kbm`, 4096-degree ceiling, tuning as causal-maxima register (`src/tuning/`, `store.rs`) |
| ephemeral tier | leased voice presence, deliberately non-durable (`presence.rs`) |
| convergence | gossip + RBSR + `sync_root` cross-check; strict deferral; union-only repair |
| edit model | absolute intents; intent-as-cotransaction; Tier 0 rollback-as-reprojection (the two research designs) |
| a second object class | pieces: owner-gated, position-identified, periodic-pitch objects (`ops.rs:118-132`) — proof the substrate hosts more than one musical type |
| live theory layer | scale/chord matching and mode inference over the live set (`graph.rs`, `solfege.rs`) |

### 5.2 What the vision needs (the honest gap list)

1. **A temporal coordinate over sets.** Today a room holds one hot set: the
   eternal now. A *score* needs sets addressed over musical time. Three
   candidate shapes, none chosen: time as a facet on holdings; sets-of-sets
   with a progression index; or round boundaries (potluck's
   `StartNewRound` pattern — an append-only read-model boundary,
   `reactive-rollback-api-design.md` §6.3) as bar lines. This is the
   paradigm's biggest open design, and §8 flags its cultural risk.
2. **Continuous facets.** Weight, expression residue, register — each an
   append-only schema evolution (`ops.rs:27-29`) plus a decision about which
   tier it lives on (durable vs leased). The tier decision is the design
   knife; §8 works it through, §11 flags the residue.
3. **The explicit-target retract.** `RetractDegreeAdds` — designed, costed,
   parked for the next `OP_SCHEMA_VERSION` bump
   (`ui-state-coupling-design.md` §4.3). It upgrades the implicit horizon
   precondition into intent-as-data and unlocks per-room ownership policy as
   pure evaluator config.
4. **Atomic multi-degree intents.** `Batch` ops for chords and
   transformations (`reactive-rollback-api-design.md` §6) — one entry hash,
   one verdict, one signature; HHS3's "a multi-table write is one atomic
   operation" translated to harmony.
5. **Historical reads surfaced.** The store folds only the full snapshot
   (`view()`, `store.rs:392-408`); `getView(at, from)` needs a walkie
   surface. The DAG has everything required — this is API, not archaeology.
6. **The MIDI edge, both directions.** Downhill is planned: MIDI out
   re-sourced from fold `Revision` diffs. Uphill already half-exists: MIDI
   note-on/off lifts to `set_native_degree(pc, is_note_on)` — absolute
   intents — at `app.rs:866`. The full story is an edge adapter that makes
   any MIDI instrument a (single-author, 12-TET-tuning-scoped) citizen of a
   hot set, and any hot set a MIDI source for legacy synths. Interop as a
   projection/lift pair at the boundary, never a compromise in the core.

### 5.3 The smallest experiments that would prove it

Cheapest-decisive first:

1. **The scrub bar** (proves "the score is the log"). Fold the DAG at
   historical frontiers and render the keyboard read-only at `at`. Zero wire
   change, zero schema change, pure read-side code — and the first time
   anyone drags it, the paradigm stops being a metaphor.
2. **Authorship color** (proves "provenance is musical material").
   `pitch_authors` already flows to the UI; paint holdings by author. One
   rendering change; the social layer of §4.3 becomes visible immediately.
3. **One continuous facet** (proves the evolution discipline carries the
   vision). Add `weight` as a `#[serde(default)]` field on `AddDegree`, map
   it to velocity in the renderer and to opacity in the UI. First
   probabilistic hot set for the price of one field.
4. **The chord batch** (proves atomic transformations). `Batch` variant plus
   two named intents — a triad placement and a Neo-Riemannian P swap.
   Transformational theory's first executable gesture on the wire.
5. **The MIDI round-trip** (proves the interop story). Two rooms bridged
   through a hardware synth: hot set → note events → synth → note events →
   lifted intents → hot set. If the music survives the double crossing, the
   edge adapter is real.

Each is independently shippable; none blocks another; 1–3 touch no protocol
at all.

---

## 6. An implementation-agnostic substrate contract

Everything above is grounded in one stack — walkie's HHHS mirror over signed
p2panda logs, with Santi's `hhs3-ts` as the semantics' source of truth. That
grounding was the point: no hand-waving. But a paradigm welded to one
codebase is just a feature list with ambitions. The question that decides
whether tutti is a *protocol* is: what is the minimal contract a substrate
must honor such that every affordance this document leans on — verifiable
authorship, deterministic convergence, coordination-free merge, time-travel
— follows from the contract itself, and not from HHS3 in particular?

### 6.1 Essential: four properties, one small trait

**(a) Self-certifying authorship.** Every op is signed by its author's key,
and the author's identity *is* the verifying key — no registry, no account
server. Verified once at ingress, never re-litigated (walkie: Ed25519 via
p2panda, `verify_signed_op`, `ops.rs:537-592`; the `VerifiedOp` constructor
is private precisely so "a store write that takes a `VerifiedOp` cannot be
handed unverified data," `ops.rs:352-355`).

**(b) A deterministic view function.** State is a pure query
`view(at, from)` over the verified op-set. Two peers holding the same
verified ops MUST compute identical views — on any conforming
implementation, in any language (walkie: the fold in `store.rs:392-408`,
over entries whose hashes are pure functions of the signed bytes and their
causal position, `store.rs:1-16`).

**(c) Commuting merge semantics.** The op alphabet's core is
CALM-monotone — adds commute, union is the merge — with the non-monotone
residue (removes, registers) tagged and resolved causally: add-wins,
observed-remove, causal-maxima. Ops merge coordination-free; delivery order
is irrelevant up to causality.

**(d) An append-only, causally-frontiered, verifiable log.** Ops reference
their causal past by hash; nothing ever removes an op; the frontier is a
first-class value a new op must stamp as observed. This is simultaneously
the integrity story (tamper = hash break) and the temporal coordinate:
because the log is the state's whole history, `at` in `view(at, from)` is
just an argument.

Sketchable as a trait:

```rust
/// The tutti substrate contract. A conforming implementation provides
/// exactly these obligations; every affordance in this document follows
/// from them. Everything else — sync algorithm, storage, transport,
/// crypto suite — is an implementation choice.
pub trait TuttiSubstrate {
    /// (a) Self-certifying identity: the author IS the verifying key.
    type Author;
    /// Collision-resistant op identity, derived from signed content and
    /// causal position — never assigned.
    type OpId;
    /// A causal position: a frontier of op ids.
    type Frontier;

    /// (a) Admit an op iff its signature verifies against its claimed
    /// author and its causal references resolve (defer, don't drop, while
    /// the past is in transit). Verification happens once, at ingress.
    fn ingest(&mut self, signed: &[u8]) -> Result<Vec<Self::OpId>, Reject>;

    /// (d) The grow-only log's current frontier — what a local commit
    /// must stamp as observed.
    fn frontier(&self) -> Self::Frontier;
    fn commit(&mut self, key: &SigningKey, op: TuttiOp) -> SignedBytes;

    /// (b) The deterministic read: the hot set at position `at`, judged
    /// from horizon `from` — a pure function of the verified op-set.
    /// Equal op-sets MUST yield identical views. (c) lives inside this
    /// function: add-wins/observed-remove per content key, registers by
    /// causal maxima.
    fn view(&self, at: &Self::Frontier, from: &Self::Frontier) -> HotSetView;

    /// Convergence as a checkable fact: a digest over the verified-op
    /// identity set. Equal roots mean the same music.
    fn root(&self) -> [u8; 32];
}
```

### 6.2 Swappable: explicitly not in the contract

- **The reconciliation algorithm.** RBSR is an efficiency choice, not a
  semantic one. Any anti-entropy that converges two replicas on the same
  verified op-set conforms — want-lists, Merkle diffs, full exchange over a
  mailed USB stick. The contract's convergence obligation is *set
  equality*, checkable via `root()`; how you get there is your business.
- **Storage.** redb, IndexedDB, sqlite, a ring buffer in SRAM (§6.6).
- **Transport.** iroh, libp2p, BLE, LoRa, sneakernet. Ops are
  self-verifying bytes; the wire owes them nothing but delivery.
- **The hash and signature suite.** The contract needs collision resistance
  and unforgeability, not blake3 and Ed25519 by name — with the sober
  caveat that a suite change re-bases every identity in the system, so
  suites are versioning events, never runtime knobs.

**The one-line verdict: essential = signed self-certifying ops + a
deterministic `at`/`from` view + commuting set semantics + an append-only
causal log; swappable = how replicas find each other's ops, where the bytes
sleep, and which curves sign them.**

### 6.3 Why the affordances follow from the contract, not from HHS3

Trace each promise to its property:

- **Convergence** ⟸ (b) + (d): same verified op-set, same view — so any
  conforming sync, however primitive, delivers agreement, and `root()`
  makes the agreement checkable.
- **Provenance and the Byzantine bound** ⟸ (a): forging Ada's holding
  reduces to forging Ada's signature.
- **Offline-and-merge** ⟸ (c) + (d): a partition is just two log suffixes
  whose union the semantics already interpret.
- **Time-travel and forking** ⟸ (d) + (b): history is retained and `at` is
  an argument; a branch is a frontier you keep.

Which means conforming substrates can be genuinely diverse: HHS3/hhhs-rs is
one; an Automerge-based build is plausibly another (Automerge changes are
already content-hashed and dependency-linked — add signature verification
at ingress, express the degree alphabet's observed-remove semantics on top,
and expose an `at`/`from`-parameterized read, none of which its core
forbids); a bespoke fixed-format log is a third (§6.6). The scenarios of §4
ask permission from the contract, not from any repo.

### 6.4 "BFT," said precisely

An honesty obligation: this document has used "Byzantine" and must say
exactly what is and is not bought. What a signed-CRDT-log substrate
provides is **authenticated, self-certifying, convergent** replication.
That is a real adversarial guarantee and a *different* one than classical
BFT consensus (PBFT, Tendermint-style total-order SMR). The threat model,
spelled out:

- **A Byzantine peer is confined to its own authorship.** It can sign
  garbage — under its own key, into its own layer, where it is attributable,
  filterable, and socially actionable. It cannot forge another author's
  holdings, cannot retract-as-someone-else, and cannot make two honest
  replicas holding the same ops disagree (that would contradict (b)).
- **Withholding degrades completeness, never safety.** A peer that hides
  ops delays convergence; it cannot corrupt it. The `root()` cross-check
  detects an honest withholder but not a root-forging liar
  (the known limit pinned at `hhhs-core/src/sync_session.rs:979-1010`, per
  `reactive-rollback-api-design.md` §7); the residual healing is what it is
  everywhere in this family — multiple peers, periodic anti-entropy.
- **Equivocation is representable, not fatal.** An author who forks its own
  log (two signed ops at one seq) has produced two verifiable ops; the DAG
  admits both as distinct entries and the view folds both. Convergence is
  untouched — the fork is *visible as data*, and sanctioning it is policy
  above the substrate, not a substrate obligation.
- **A malicious barrier is causally caged.** An adversarial remove kills
  only adds in its causal past — observed-remove means you cannot retract
  what you have not seen — and the fork-meet revision bound (HHS3's T1,
  `hhs3-deep-core-fable.md` §2d) confines every retroactive verdict flip
  above the last common meet. Settled history below the meet is immune even
  to adversarially-timed barriers.
- **What is NOT promised: total order, finality, quorum.** Nothing here
  lets the group decide "exactly one of these two concurrent tuning writes
  happened." The register resolves by causal maxima plus a deterministic
  tiebreak — every honest replica computes the *same* verdict, which is
  determinism, not consensus. If tutti ever needs exactly-one semantics
  ("who has the solo"), that is a coordination layer added on top, not a
  contract amendment. For a jam this is the right trade. PBFT does not
  groove.

### 6.5 What HHS3 additionally provides, and whether it's essential

- **The void engine: at-use precondition re-evaluation** (co-transactions,
  drop-on-void, transitive abort — shipped in Santi's `hhs3-ts`, the source
  of truth). *Not contract-essential*: the core alphabet needs only (c).
  It is, however, the growth path for everything policy-shaped in this
  document — ownership-gated retracts (§2.4), capability channels (§3.1),
  tuning-conditional generative holdings (§4.5). A substrate without it
  must hand-roll policy inside `view()` and re-earn determinism each time.
- **The no-verdict-cache doctrine.** Not a contract line item — a
  *theorem-shaped consequence* of (b): once verdicts involve negation over
  cycles, a memoized verdict is traversal-order-dependent and breaks
  cross-replica determinism. hhs3-ts shipped that cache once, paid for it,
  and removed it (`hhs3-deep-core-fable.md` §2a). Any conforming
  implementation with rich verdicts inherits the obligation whether or not
  it read the memo.
- **The fork-meet revision bound (T1–T3).** Beyond the contract:
  convergence doesn't need it, but the *experience* of history does — it is
  the promise that makes the scrub bar trustworthy and §6.4's
  malicious-barrier bound quantitative. Substrates without it still
  converge; they just cannot tell you when the past is settled.
- **Engineering bonuses**: RBSR's bandwidth profile, strict-deferral
  liveness, the staged/rollback taxonomy. Wanted, swappable.

### 6.6 The leaf profile: tutti on the tiniest hardware

Here is the corollary that sounds like a joke and isn't: the contract fits
on an ESP-32. Smaller, probably. And not despite the distributed-systems
machinery — *because of which machinery it refuses*.

Why a microcontroller is a natural tutti citizen:

- **Coordination-free is the whole win.** A classical BFT-consensus node
  cannot exist on an MCU — quorums, view changes, leader elections,
  multi-round voting, all of it latency- and state-expensive. Tutti's
  contract demands none of it: (c) means adds commute and merges are
  unions; there are no locks, no rounds, no consensus to participate in.
  The device signs its op and is *done*. The refusal of global
  coordination (§6.4) is exactly what makes the floor this low.
- **Per-device state is bounded by what it renders, not by the world.** A
  leaf holds its own holdings plus the union it needs to sound — a
  pitch-class set over at most 4096 degrees (`scl.rs:12`) fits in a
  bitfield; the authored layer it must track is its own. An op is small
  and fixed-shape: degree + tuning id + author key (32 B) + signature
  (64 B) + seq/backlink — a few hundred bytes of CBOR, static-buffer
  friendly, allocation-optional.
- **Sync is bandwidth-proportional to disagreement.** RBSR-style range
  reconciliation moves data on the order of the symmetric difference, and
  a leaf's difference is tiny by construction. But per §6.2 even RBSR is
  optional — a leaf may simply exchange "my ops since your last visit"
  with a fuller peer.
- **Local-first is performance-first.** The leaf renders its local fold at
  hardware latency, offline, forever; it syncs opportunistically when a
  peer, phone, or relay is reachable. Presence leases (1.5 s default,
  `presence.rs:16-17`) already assume a lossy world.

The profile, mapped onto the essential/swappable split: a **leaf node**
implements the essentials *scoped to a window* — it signs its own ops (a),
keeps its own log head for continuity, holds a bounded recent suffix plus a
compacted current view rather than full history, and computes its view
deterministically over what it holds (b, c). What it delegates is the
archive: deep history, `getView(at, ·)` for arbitrary `at`, and long-range
repair live on **full nodes** — a phone, a laptop, a room server — that the
leaf syncs against. The leaf never forges or rewrites history (it prunes
its *copy*, never the network's log); it simply declines to remember
everything, and the tiering is honest about which affordances thin with it:
a leaf converges and attributes, but it cannot scrub.

The honesty ledger for this profile: Ed25519 on an MCU is feasible but not
free — sign/verify lands in the milliseconds on a 240 MHz ESP-32 (some
variants carry crypto acceleration for the hash side), fine for holdings,
which change at musical rates, and one more reason bulk expression stays on
the lease tier (§8). The grow-only/limited-flash tension is real and is the
embedded face of §11's pruning risk: a leaf *must* checkpoint and prune,
which forfeits on-device time-travel by design — acceptable exactly because
the network keeps the history the leaf drops. And flash wear plus power
budget push the same direction everything else in this section pushes:
durable ops for structure, RAM-resident leases for gesture, radio duty
cycles set by musical time rather than network time.

The payoff is the vision's best image: **physical tutti nodes**. An ESP-32
with a handful of keys and LEDs that holds three degrees of the room's set
and shows you, in light, who else is holding yours. A grid of them across a
gallery — each mega-efficient because it tracks only its own holdings plus
the union it renders — is a distributed instrument whose "patch cables" are
signed ops, whose ensemble state reconciles over whatever radio reaches,
and whose installation survives any subset of it losing power. Eventual
consonance, with a body.

## 7. Provable lenses: the analysis tower as law-carrying views

walkie already computes derived views of the hot set — `src/web/graph.rs`
matches the live set against scale and chord catalogs, `src/web/solfege.rs`
infers modes — as ad-hoc code with no stated relationship to the thing it
views. One shelf over from HHS3 sits the discipline that upgrades them:
**riff-cat** (`/laboratory/fe-stuff/riff-catalog`), the facet-relative
content-addressing engine. In two sentences: every artifact is lowered to a
canonical graph and hashed once per *dimension* (structure, names,
constants, ...); two artifacts "rhyme at facet F" when their digests agree
on F's dimension subset. What makes it more than a hashing trick is that
its core is held to *machine-checked laws*.

### 7.1 What is actually proved (and about what)

The Lean development (`riff-catalog/lean/Riffcat/Laws.lean`) proves three
laws outright, with no `sorry`:

- **Law 1, determinism**: the digest is order-independent — any permutation
  of the input canonicalizes identically.
- **Law 2, facet refinement** (`facet_refinement`): agreement at a finer
  facet *forces* agreement at every coarser one. The lattice is real.
- **Law 3, anchor/transport** (`transport_factors` / `transport_respects`,
  both directions): a fact "rides facet F" — is well-defined on the
  quotient — exactly when it respects `~_F`. Equivalently: **a predicate is
  facet-anchored iff it factors as `valid = valid_F ∘ π_F`.**

And the part that makes this section almost unfair to write: riff-cat's
Cubical Agda prototype (`riff-catalog/cubical/`) picked, as the worked
instance of its whole quotient machinery, **Forte set-class theory**.
`Riffcat/Music.agda` builds pitch-class sets as 12-bit vectors,
transposition as the Z/12 rotation action, inversion as the dihedral
mirror, `SetClass = Pcs / ~SC`, and `primeForm` as the computable
normalization (Rahn's packing rule) — and *checks, by `refl`*, that C
major and A minor land in one set class (Forte 3-11, prime form
`[0,3,7]`), that the augmented triad does not (`[0,4,8]`), and that the
minor seventh normalizes to the published compact form `[0,3,5,8]`. The
facet-refinement surjection is instantiated *by the music tower itself*
(`setClassFromTransClass↠`: transposition classes surject onto set
classes). §2.2 called pc-set theory the conceptual bridge from event to
set; riff-cat already walked that bridge carrying proofs.

### 7.2 The tutti tower

Layer the same discipline over the hot set. Each row is a projection
(get-only — see the honesty list below), and the whole stack is
tuning-relative, parameterized by the room's `TuningId`:

| view | projection | foothold |
|---|---|---|
| absolute pitch (PS) | — | `TunedPeriodicPitch` (degree + period) — the pieces/voice layer (`tuning/mod.rs:80-106`) |
| pitch-class set (PCS) | **mod the period** — literally drop `period` via `.degree()` | the hot set itself; walkie's degree set is already this view |
| Tn class | quotient by rotation (Z/n; Z/12 instance checked in `Music.agda`) | "the same chord, transposed" |
| TnI set-class | quotient by the dihedral action; **prime form** as the split-quotient normalizer (compare normal forms, never touch the path constructor) | Forte's catalog; checked for the triad and 4-26 cases |
| interval vector | a function of set-class, strictly coarser | the Z-relation (4-Z15 vs 4-Z29 share a vector) *proves* the tower loses information at each step — which is the point |

PS → PCS is deliberately the trivial rung: mod the period, nothing more.
The value of the tower is not any one projection but the **laws between
them**: refinement (Law 2) says agreement propagates coarseward;
transport (Law 3) says facts anchored coarse are immune to motion that is
invisible coarse.

### 7.3 Why lenses reconcile for free

The substrate contract does the heavy lifting. Property (b) makes the base
view a pure function of the verified op-set; a lens is a pure function of
the view; so `lens ∘ view` is itself a deterministic view — **every derived
view converges exactly when the base does.** Two replicas may transiently
disagree about the chord, but at equal op-sets they *cannot* disagree about
its prime form, its interval vector, or any other lawful projection. In
signal terms this is signal-over-signal: hhhs-reactive's
`Revision{added, retracted, at}` diffs lift through a lens as a mapped
signal, recomputed per coalesced growth under the same no-cache discipline
as everything else (`reactive-rollback-api-design.md` §3, §5.2). Live,
reconciling, provably-correct analysis: the Forte readout is not a panel
bolted onto the music — it is another projection of the same log.

### 7.4 Anchored preconditions: facet-robust policy

Now compose with §2.4's co-transactions. A precondition that factors
through a coarse facet (`valid = valid_F ∘ π_F`) is, by transport,
invariant under every op the facet cannot see. Musically:

- "this generative holding stands while the room's harmony is in set-class
  3-11" — anchored at the set-class facet, so *transposition can never
  void it*, by construction, while a genuine quality change (major →
  augmented) deterministically does;
- a pedagogy room's rule "no more than four distinct interval classes" is
  an interval-vector-anchored verdict — robust to voicing, register,
  transposition, and authorship churn;
- this is the corpus's G6 move (structure-only facets commute with
  renames) transposed to music, where "rename" literally *is*
  transposition.

And representation changes commute checkably: a Lewin-style `Tn` batch
intent (§2.2) maps through `π_setclass` to the identity — the square
closes, and the tower tells you exactly which views each transformational
action fixes.

### 7.5 Honesty ledger

- **Status precision** (per the corpus's own adversarial review,
  `adversarial-review-fable.md` E5): Lean Laws 1–3 are proved; Laws 4–6
  (names-blindness, WL soundness, encoding injectivity) are *stated axiom
  targets*, and BLAKE3 collision-resistance is a named axiom. The cubical
  witness proves the quotient/transport layer over an *abstract* hash.
  Everything §7.2–7.4 leans on is in the proved subset — quotients,
  refinement, transport — not the targets.
- The corpus's G5 "facet peel" is unsound as specified (the review's E1
  counterexample); nothing here uses it. Lenses in this section only ever
  *read*.
- `Music.agda` is 12-specific. Tutti needs the Z/n cyclic and dihedral
  generalization (n ≤ 4096, `scl.rs:12`) plus n-ary interval vectors —
  routine mathematics, unwritten code and proof. And for non-octave `.scl`
  periods, "pitch class" is really *period class*: the tower is
  tuning-relative all the way down, which is why `TuningId` parameterizes
  every lens.
- These are **views, not bidirectional lenses**. Editing "at the set-class
  level" is not a `put` — it is an intent at the base (§2.4) whose effect
  provably commutes (or provably doesn't) with the projection. No claim of
  a lawful `put` is made anywhere in this section.

Smallest experiment: replace `graph.rs`'s catalog matching with a
prime-form + interval-vector readout computed from the fold — the first
law-shaped lens, pure read-side. Then lockstep an n-generic `prime_form`
against a Lean or Agda spec with golden vectors, exactly the discipline
riff-cat already runs for its own digest engine (`lean/README.md`).

---

## 8. Time-sensitive facets: generators, not streams

§2.3 promised continuous facets and §11 warns that expression bandwidth
wants to destroy the log. This section is the resolution, and it is one
move applied consistently: **the log stores the description; the renderer
performs it.** A continuous musical facet — a CC curve, an ADSR envelope, a
morph — enters tutti as a durable, reconciling set of **sparse control
points or generator parameters**, and leaves it as sound via local
evaluation at performance latency. Store kilobytes; render megahertz.

### 8.1 The two tiers, drawn precisely

The structure/performance split (§1.3) gives continuous data a bright
line:

- **A live fader sweep is presence.** While your finger is on the control,
  values stream as leased presence frames — the exact machinery of voice
  preview (`PresenceBody` carries a value, expires in 1.5 s unrefreshed,
  never enters history, `presence.rs`). Low latency, lossy-tolerant,
  self-cleaning: a crashed peer's filter sweep decays instead of freezing.
- **Recorded automation is committed control points.** Release the fader
  (or arm the record) and the gesture is decimated to breakpoints and
  signed: sparse ops under the append-only evolution discipline
  (`ops.rs:27-29`) — a `SetFacetPoints`-shaped variant, or per-point ops
  keyed content-wise so the *existing* add-wins/observed-remove semantics
  become automation-editing semantics for free: concurrent edits to
  different breakpoints merge; edits to the same breakpoint resolve like
  any register. Promotion from tier one to tier two is deliberate — it is
  literally record-arm, the §2.3 "knife" as a user gesture.

MIDI's CC is the instructive contrast: 7-bit samples at wire rate, meaning
implicit in the receiver — the stuck note's legato cousin (a lost CC
leaves the filter wherever it happened to be, forever). OSC got the
*syntax* right — typed address space, bundles, timetags that even schedule
the future — and stopped there: a consistency-free value stream with no
merge, no provenance, no state. Tutti takes OSC's lesson (rich addressing:
§3.1's topic ∩ author ∩ facet slices) and adds what OSC never had: the
values reconcile, and the curve *is somewhere*.

### 8.2 Interpolation, envelopes, blends

- **Interpolation is a renderer contract, not log data.** Control points
  carry their interpolation kind (step, linear, spline knots); the audio
  thread evaluates. Determinism per the substrate contract: equal op-sets
  yield equal curves as *functions*; what differs per peer is only the
  local clock sampling them.
- **Envelopes and LFOs are generators.** An ADSR is four numbers plus a
  trigger rule; an LFO is shape, rate, phase. Durable, tiny,
  author-attributed. The honest wrinkle is the **time origin**: op
  wall-clock is display-only (`ops.rs:196-199`), so a durable generator
  is anchored either to score-time (the §5.2 temporal coordinate — still
  open) or to a *render-local* trigger (the holding's local onset). The
  latter is deterministic per peer but not phase-aligned across peers —
  which is not a bug, it is §1.3's split holding its line. Do not pretend
  a transatlantic LFO is in phase.
- **A blend is a lawful lens** (§7): a crossfade or morph between two
  authors' curves is a pure function of two reconciled views —
  deterministic, convergent, and *anchored*: a blend that factors through
  the facets it reads is invariant, by transport, to every op those facets
  cannot see. "Morph between Ada's automation and Ben's" is a §7.3-class
  derived view, and inherits its convergence proof instead of needing one.

Grounding check: the live tier half-exists (voice presence streams a
continuous-ish value; the voice conditioner already quantizes a continuous
signal and computes the `cents_deviation` residue §2.3 wants to keep). The
durable tier is designed surface, not shipped code — `midi.rs` today
handles notes only, no CC lift (§9). That is the gap list's item 2 made
concrete.

---

## 9. Bridging the incumbents: MIDI devices and DAWs at the edge

The interop stance from §3's table, operationalized: tutti speaks MIDI
**at the edges**, through bridge nodes, and never compromises the core.
A bridge is any replica with a MIDI face: the browser (Web MIDI —
shipping), the Tauri host (`native_midi` capability in the snapshot,
`app.rs:391-410`), the DAW plugin (below), or an ESP-32 leaf with a DIN
jack (§6.6).

### 9.1 Inbound: lifting events to intents

- **Notes.** Note-on/off lift to absolute intents — shipping today:
  `app.rs:866` maps `is_note_on` straight to
  `set_native_degree(pc, on)`. A mode switch decides the tier per §8:
  *performance mode* maps keys to leases (play through the room without
  writing history); *composition mode* commits durable adds. The lift is
  honest about its losses: the whole keyboard is one author (the bridge's
  key), and its 12-key-per-octave frame reaches the room's tuning through
  the Scala keyboard mapping — which walkie already parses
  (`src/tuning/kbm.rs`), so "which physical key means which degree" is
  data, not assumption.
- **CCs** lift to §8 control points — presence frames while moving,
  committed breakpoints on record-arm. Not yet built: `src/web/midi.rs`
  handles notes and All-Notes-Off only. Named as future work, not
  described as present.

### 9.2 Outbound: the set-delta edge, and owning the stuck note

Outbound MIDI is the fold's derivative: set-membership deltas become
note-on/off. This is not a sketch — it is shipping code:
`sync_toggle_notes` diffs the current set against the sounding set and
"send[s] offs for removed, ons for added" (`midi.rs:123-134`), with
`all_notes_off` as the panic bar (`midi.rs:175`).

The stuck-note problem, exiled from the core in §0, *reappears here* —
the hardware synth on the far side of the DIN cable is the one
implicit-state receiver tutti cannot abolish. So the bridge owns note
lifecycle explicitly:

- it renders the **local current view**; a late-arriving op is simply a
  new delta, hence new events — reconciliation upstream, events
  downstream;
- every outbound note-on is tracked against the view (the
  `MidiOutputManager` already keeps the playing-notes set), so the
  bridge can always produce the exact off-set;
- the bridge's own death must fail to silence: All-Notes-Off on drop and
  a watchdog — the lease discipline (§1.1) applied to the analog world.

**Microtonal out** goes through MPE or MIDI 2.0 per-note pitch: the
tuning layer computes each degree's exact position (the quantization
machinery exposes center frequencies and cents, `tuning/mod.rs:179-190`),
emitted as per-note pitch offsets. Honest limits: MPE's channel rotation
caps a zone near 15 sounding voices; MIDI 1.0 targets need a negotiated
pitch-bend range; and a 4096-degree tuning is addressable by per-note
pitch but hopeless as key numbers — the `.kbm` layer decides which
degrees get keys at all.

### 9.3 The DAW: tutti as a plugin

The vehicle exists: walkie ships a MIDI-only note-effect plugin
(CLAP + VST3, `src/plugin/mod.rs`) whose `process()` already speaks both
directions — DAW notes on a pitch-classes channel toggle room degrees, a
voice channel drives the monophonic voice, and room deltas come back as
note-on/off on three param-configurable channels. That channel scheme is
§3.1's rich address space folded down to MIDI's, deliberately. The
`nice-plug` migration (`docs/research/nice-plug-migration.md`) is the
modernization path: same programming model, permissive VST3 licensing, an
easy real standalone binary. Honest status, from that doc's own critical
finding: the plugin's networking thread is bit-rotted — written against
libp2p + yrs, it does not compile against the iroh/HHHS stack, and
re-pointing it (its "Stage 0a") is days of real work and effectively part
of the p2panda rewrite. The bridge vision rides on that revival.

What the DAW connection buys once it stands:

- **tutti as instrument**: the DAW records the room's reconciling history
  as ordinary MIDI — a distributed ensemble's converged performance
  becomes a take on a timeline, mixable like anything else;
- **tutti as controller**: DAW automation lanes drive §8 facet control
  points — the DAW becomes one more author with very steady hands;
- **timelines meet** (speculative, but every part is named machinery):
  DAW transport scrub ↔ `getView(at)` — the plugin renders historical
  frontiers as the timeline rewinds, making the DAW's ruler a window onto
  the op-log's temporal coordinate. Needs §5.2's historical reads
  surfaced; needs nothing else new.

The asymmetry stays the asymmetry (§3, §11): projecting out is easy and
faithful; lifting in is easy and lossy. A bridge makes a MIDI instrument
a *citizen* of the set — one author, one mapping, no intent nuance — and
that is the correct modesty for an edge adapter.

---

## 10. Naming it

Working name: **tutti**.

In a score, *tutti* is the marking at which everyone plays — the moment the
music is, by definition, the union of everybody's parts. That is this
paradigm in one word: the session is a union, membership is authorship, and
the union is the score.

The pitch, one paragraph: **tutti is a session protocol in which the shared
musical object is not a stream of note events but a hot pitch-class set — a
live, tuning-scoped, author-attributed set of holdings carried on a
grow-only signed op-log, reconciled by causal add-wins and observed-remove,
rendered locally at performance latency, and readable at any point of its own
history.** Notes are signed facts with provenance instead of anonymous
datagrams; silencing is a causal act that kills only what it observed; the
tuning is part of every pitch's identity, so 12-TET is one setting rather
than an ontology; ephemeral gesture lives on leases while structure lives in
history; and because state is a query over the log, the piece's past is
addressable, forkable, and mergeable. Where MIDI ships *what just happened*
and hopes every receiver agrees, tutti ships *what is* — and can prove that
everyone eventually holds the same music. Eventual consistency, in a musical
session, has a better name: eventual consonance.

---

## 11. Open questions and honest risks

- **Rhythm is not solved and this document does not pretend it is.** The
  hot set carries harmony and structure; phase-accurate ensemble timing
  still needs a Link-class clock layer or NINJAM-class delay aesthetics, and
  fusing a reconciled score-time coordinate with a low-latency performance
  clock is genuinely unexplored. The paradigm's claim is scoped: structure
  eventually-consistent, performance local.
- **The temporal coordinate could smuggle in an ontology.** Choosing bar
  lines as the set-of-sets index quietly bakes Western metric hierarchy into
  the substrate, the same way MIDI's key numbers baked in the piano. The
  tuning layer got this right (periods and degrees, no privileged 12); the
  time layer must clear the same bar, and none of the three candidate shapes
  in §5.2 obviously does yet.
- **Fold cost versus the no-cache doctrine.** Verdicts are deliberately
  recomputed, never memoized — caching verdicts over negation breaks
  cross-replica convergence, a lesson HHS3 paid for
  (`hhs3-deep-core-fable.md` §2a). Room-sized sets refold trivially;
  score-sized logs with per-holding facets are unpriced. The permitted
  escape is the advisory-accelerator discipline (an accelerator that must
  equal the from-scratch oracle) — but that model needs building *before*
  the score dimension lands, not after.
- **Expression bandwidth wants to destroy the log.** MPE-grade continuous
  inflection at signed-op granularity would bloat a permanent history with
  data nobody replays. The durable/leased split is the answer in principle
  — expression rides presence, promotion to history is deliberate — but
  "which facts deserve to be forever" is a per-facet editorial judgment the
  protocol can host and cannot make.
- **Grow-only means forever, and forever is heavy.** A lifetime of jams on
  an append-only log meets no pruning story that preserves the time-travel
  promise. Checkpointing, partial replication, or facet-graded logs (the
  HHS3/riff-cat "graded replication" summit) are plausible; all are
  research, not roadmap. The leaf profile (§6.6) sharpens the question
  rather than answering it — leaves prune by design and lean on fuller
  nodes to remember, which works only as long as *someone* is a full node.
- **Shared silence is a social choice wearing a technical hat.** Anyone-can-
  resolve is right for a jam room and possibly wrong for a recital; the
  policy-as-evaluator route keeps both expressible, but real rooms will
  discover social failure modes (retract wars, provenance spoofing at the
  human layer — the keys sign ops, not intentions) that no fold semantics
  anticipates.
- **The lift from MIDI is lossy by nature.** A hardware keyboard enters the
  set as one author in one tuning with no per-holding intent. Fine as an
  edge; a problem only if the edge is mistaken for the paradigm.
- **And the largest risk: that structure isn't where the music is.** A
  skeptic can grant every mechanism here and still claim that what matters
  in ensemble music is precisely the part this paradigm declares local —
  timing, breath, phase. If so, tutti is a composition and pedagogy
  substrate rather than a performance protocol. That would still be worth
  building. But the bet this document actually makes is sharper: that
  harmony-as-shared-state is a musical instrument nobody has played yet,
  and that the first room full of people watching their chord reconcile
  will hear something MIDI never had a message for.
