# tutti + AMY on the ESP32 leaf: a verifiable synthesizer node

**Status:** research + design, 2026-08-08. No code changed. Companion to
`docs/vision/eventually-consistent-pitchsets.md` (§6.6 the leaf profile, §8
time-sensitive facets, §3.1 channels/device controls, §9 the MIDI edge),
`docs/vision/hhhs-ecosystem-design.md` + `docs/vision/
hhhs-reorg-spec-and-migration.md` (the `hhhs-dag` floor and the `DagRead`
seam), `docs/vision/tutti-crate-architecture.md` (§3 `OpLanguage`/`FoldCtx`,
§6.4 leaf honesty), `docs/research/transport-agnostic-direct-peering-design.md`
(§7 device links), and `docs/research/performance-benchmark-suite.md` (§7.2
the M0–M4 ladder; §2 the leaf targets).

**AMY grounding:** shorepine/amy at HEAD (main, 2026-08; MIT license), read
from source and docs — `README.md`, `docs/api.md`, `docs/synth.md`,
`docs/arduino.md`, `src/amy.h`, `src/api.c`, `src/amy.c`, `src/parse.c` — and
the sibling project Alles (shorepine/alles `README.md`), the existing
AMY-mesh-over-WiFi system, which turns out to be this design's most
instructive foil. Line references are to the fetched HEAD copies. Sources:

- https://github.com/shorepine/amy — "AMY - A high-performance fixed-point
  Music synthesizer librarY"
- https://github.com/shorepine/amy/blob/main/docs/api.md — wire codes, C API
- https://github.com/shorepine/amy/blob/main/docs/synth.md — synthesis model
- https://github.com/shorepine/alles — the AMY WiFi mesh synth

The question under design: tutti is a generic, verifiable,
eventually-consistent substrate over an hhhs causal op-DAG; AMY is a tiny,
battle-tested C synthesizer that runs on the exact silicon the leaf profile
names. Wire one to the other so that an ESP32-S3 is simultaneously **a
converging tutti peer and a local sound source** — the vision's "physical
tutti node" (`eventually-consistent-pitchsets.md:998-1005`), now with a
speaker.

**Contents:** §1 AMY, summarized from source · §2 what the tutti side
supplies (and doesn't, yet) · §3 the core mapping: two framings, one verdict ·
§4 continuous facets ↔ AMY envelopes · §5 the minimal leaf stack + an honest
ESP32-S3 budget · §6 the verifiable synth mesh (vs. Alles) · §7 experiments,
risks, open questions.

---

## 1. AMY, the real thing

### 1.1 What it is

AMY ("Additive Music synthesizer librarY") is "a fast and small music
synthesizer library written in C" by DAn Ellis and Brian Whitman
(README.md:3,29), the engine of the Tulip Creative Computer and the Alles
mesh speakers. It compiles standalone — "simply copy the .c and .h files in
`src` to your program… No other libraries should be required to synthesize
audio" (README.md:89) — on ESP32/S3/P4/C3/C6, RP2040/RP2350, nRF52, Teensy
3.6/4.1, Daisy, Playdate, desktop (Mac/Linux/Windows), and the web via
Emscripten (README.md:15-25). Rendering is fixed-point by default
(`AMY_USE_FIXEDPOINT`, amy.h:129; internal `SAMPLE` is S8.23 in an int32,
output int16), which is why it holds up on MCUs without FPUs.

### 1.2 Synthesis model

The engine is a pool of band-limited oscillators composed into three
management layers (docs/synth.md):

- **Oscillators** — sine, band-limited pulse (with duty), saw up/down,
  triangle, noise, Karplus-Strong, PCM, wavetable (16,384-sample packs, with
  `duty` interpolating across 64 stored cycles), `ALGO` (DX7-style FM
  operator groups, algorithms 1-32 "borrowed from the DX7"), `PARTIAL`/
  `BYO_PARTIALS` (additive synthesis: each partial is an oscillator with its
  own breakpoint envelope), plus custom C oscillators. Per-osc: frequency,
  amplitude, phase, pan, feedback, portamento, one biquad filter (LP/BP/HP/
  double-LP/notch/phaser, `filter_type` 0-6, resonance 0.5-16).
- **Voices** — named groups of oscillators (a Juno voice is 5 oscs); patches
  (0-127 Juno-6, 128-255 DX7, 256 piano, 384-390 GM drum kits, 1024-1055
  RAM "memory patches" defined at runtime as wire strings) instantiate
  voices. Notably, AMY stores its built-in patches *as wire messages*
  (README.md:162) — a patch is a string value, a fact §3.3 leans on.
- **Synths** — instrument-level voice management: `synth`/`num_voices`
  allocate a voice set with note stealing, MIDI channel mapping, sustain
  pedal, per-synth level (docs/api.md, the `i`-prefixed fields). The caller
  can address a synth with plain note-on/off and never touch oscillators.

Modulation is uniform: every continuously-controllable parameter (amp, freq,
filter freq, duty, pan) is a **ControlCoefficient vector** — up to 9 sources
(constant, MIDI note, velocity, EG0, EG1, LFO/mod_source, pitch bend, two
external channels) each scaled and summed (docs/synth.md). LFOs are "one
oscillator modulating another" via `mod_source`. Each oscillator has **two
breakpoint envelope generators**: lists of `(time_ms, value)` pairs — default
8, max 24 breakpoints per EG (amy.h:238-242) — where "the last pair you
specify will always be seen as the 'release' pair, which doesn't trigger
until note off" (docs/synth.md), with four interpolation regimes per EG:
RC/analog, linear, DX7-style, true exponential (`eg_type` 0-3, docs/api.md).
Global per-bus effects (4 buses, amy.h:122): chorus, reverb, echo, 3-band EQ.

### 1.3 Control surface / wire protocol

AMY has one control model surfaced two ways:

- **`amy_event`** (amy.h:538-607): a plain C struct — `time`, `osc`, `wave`,
  `preset`, `midi_note` (a *float* — fractional notes are first-class),
  `patch_number`, `velocity`, the five coef arrays, `feedback`, breakpoint
  arrays (`eg0_times/values`, `eg1_times/values`, 24 slots each), filter
  fields, algorithm/algo_source, synth-layer fields (`synth`, `num_voices`,
  `pedal`, …), sequencer `ticks[3]`, bus/effects fields. Unset fields carry
  sentinel values; only set fields are applied.
- **Wire messages**: the same event serialized as a compact ASCII string of
  single-letter codes, e.g. `v0n50l1K130` = osc 0, note 50, velocity 1,
  patch 130 (README.md). Key codes (docs/api.md): `v` osc, `w` wave, `n`
  note, `l` velocity, `f` freq coefs, `a` amp coefs, `F` filter-freq coefs,
  `A`/`B` the two breakpoint strings (`"0,1,150,0.5,250,0"`), `T`/`X` EG
  types, `G` filter type, `R` resonance, `o` algorithm, `I` ratio, `K` load
  patch, `u` define memory patch, `i` synth, `iv` num_voices, `Q` pan, `H`
  sequencer ticks, plus global effects (`h` reverb, `k` chorus, `M` echo)
  and utility codes. Max wire command length 256 bytes (amy.h:163).
  Partial breakpoint updates are positional (`bp0=",,,,0.2"` changes only
  the 4th value) — i.e. **wire messages are deltas against current synth
  state**, a fact that matters a great deal in §3.

Internally every event is decomposed into per-parameter **deltas** pushed
onto a time-sorted queue (`amy_event_to_deltas_queue`, amy.c:649; one delta
per set field), and the render loop executes all deltas whose time has come
before synthesizing each block (`amy_execute_deltas`, amy.c:2045-2050).

**Scheduling and time.** The system clock is derived from audio itself:
`amy_sysclock()` returns integer milliseconds computed from total samples
rendered — "on all platforms, sysclock is based on total samples played,
using audio out (i2s or etc) as system clock" (api.c:237-256). An event's
`time` field is an absolute sysclock ms deadline, intended "for near-term
ordering (e.g. noteon_delay) and output-latency compensation, not for
long-horizon scheduling; use ticks= for that" (api.c:326-335); a global
`latency_ms` offset is added to every event, which is how Alles achieves
mesh-wide sync (§6). Long-horizon scheduling goes through the built-in
sequencer: `H=tick[,period[,tag]]` at 48 PPQ (amy.h:221) against a tempo
(default 108 BPM), with repeating patterns and cancellable tags, clocked "by
rendered samples so it works in real-time and offline rendering" (README.md).

### 1.4 C API surface and real-time shape

The application-facing API (docs/api.md, api.c):

```c
amy_config_t c = amy_default_config();   // features, limits, pins, hooks
c.features.chorus = 0; ...               // trim to the platform
amy_start(c);                            // allocate + start (I2S/MIDI if configured)
amy_event e = amy_default_event();
e.osc = 0; e.freq_coefs[COEF_CONST] = 440; e.velocity = 1;
amy_add_event(&e);                       // or: amy_add_message("v0f440l1");
int16_t *block = amy_simple_fill_buffer();  // render AMY_BLOCK_SIZE frames
uint32_t now_ms = amy_sysclock();
float load = amy_get_render_load();      // 0..1 CPU fraction
```

Rendering is block-based: `AMY_BLOCK_SIZE` = **256 samples** (128 on Daisy;
amy.h:77-81) at `AMY_SAMPLE_RATE` = **44100 Hz** (48 kHz on Daisy and
Emscripten; amy.h:84-89) — one block = **5.8 ms**. Stereo by default
(`AMY_NCHANS 2`). On dual-core MCUs (ESP32 variants, RP2040/2350) AMY splits
oscillator rendering across both cores (`platform.multicore`, on by
default), and on ESP32 it manages its own I2S driver and render task. A
CPU-overload failsafe resets the engine if smoothed render load sits ≥ 0.98
for 250 ms (api.c:39-42) rather than wedging the host. The config carries
host-integration hooks — `write_samples_fn` (bring your own audio output),
`amy_external_render_hook`, per-bus postprocess, MIDI in/out hooks, and
`ram_caps_*` fields that steer each allocation class (event pool, synth
state, delay lines, samples) into internal SRAM vs. SPIRAM on ESP32
platforms (amy.h:714-803, api.c:54-70).

### 1.5 Footprint, honestly

Defaults are desktop-ish: `max_oscs` 250, `max_voices` 64, `max_synths` 64,
`max_memory_patches` 32 (api.c:48-52) — all config-scalable downward. The
README publishes no RAM totals ("highly optimized for polyphony… on even the
lowest power and constrained RAM microcontroller", README.md:25), so the
honest bounds are evidential:

- AMY (v1 era) ran **120 oscillators with per-osc filters, LFOs and
  envelopes, plus the WiFi stack, on the classic ESP32** (~520 KB SRAM,
  ~200-290 KB usable heap) as the Alles speaker (alles README.md:13).
- It runs on the RP2040 (264 KB total SRAM) with reduced oscillator counts.
- Per-oscillator state (`struct synthinfo` + `mod_synthinfo`,
  amy.h:609-675) is a few hundred bytes plus dynamically-sized breakpoint
  arrays — order 300-500 B/osc (estimate from the struct, not a measured
  number).
- The big optional consumers are the effects delay lines (chorus/echo/
  reverb) and RAM sample loading — all feature-gated in config, and on
  ESP32-S3 steerable into PSRAM via `ram_caps_delay`/`ram_caps_sample`.
- Patches and ROM PCM live in flash/rodata.

Working conclusion for the leaf: **a 16-32 oscillator, effects-light AMY
configuration is comfortably a few tens of KB of RAM and well under one
ESP32-S3 core at 44.1 kHz**; the 120-osc Alles datapoint on strictly weaker
silicon is the existence proof. §5.3 builds the combined budget.

---

## 2. What the tutti side supplies (and doesn't, yet)

The leaf profile's foundation, restated from the hhhs/tutti corpus in one
paragraph each, because §3's design decisions hang off them:

- **The contract floor.** `hhhs-dag` (post-split) is the embeddable
  contract: entry identity (`entry_hash`, blake3), the `DagRead`/`Growth`/
  `DagDelta`/`DagStore` traits, `AppendOutcome`'s defer-never-reject
  admission, mandatory dep blake3 only (`hhhs-ecosystem-design.md` §1.1,
  `hhhs-reorg-spec-and-migration.md` §A.1). The leaf is *specified* as "a
  `DagRead` impl with a window, not a fork of the kernel" (ecosystem §3),
  with `DagDelta::appended_since → None` as the pre-built "I've compacted
  past that" escape hatch (dag.rs:228-235 per the reorg spec).
- **The windowed store (M3) is unbuilt.** The bounded-window `DagRead` —
  "a suffix window + compacted view… `entries_topo` with the window's
  entries" — is specified (reorg §A.6.3) and explicitly deferred at n=0
  (reorg §C.4). Its perf gate: **steady-state leaf RAM ≤ 64 KB at W≤256**
  (`performance-benchmark-suite.md` §2, §7.2). M3 gates M4 (on-device
  measurement). Nothing about this design changes that sequencing; §5 and
  §7 route around it where possible.
- **The fold.** tutti-core's `OpLanguage::fold` over `FoldCtx` combinators
  (add-wins set with holders, causal register, owner-gated objects) is the
  deterministic read: equal verified op-sets ⇒ identical views
  (`tutti-crate-architecture.md` §3.1-3.2). A leaf runs a *small* fold over
  its window — "the window is the world" (ecosystem §3).
- **Continuous facets.** The §8 doctrine: "the log stores the description;
  the renderer performs it" — durable sparse control points/generator
  params with an interpolation kind, leased presence frames for live
  gesture, promotion (record-arm) as the deliberate boundary
  (`eventually-consistent-pitchsets.md` §8).
- **Device transports.** A leaf link implements `SyncStream`, not
  `Transport`: USB-CDC serial first, BLE GATT second, with a fuller peer as
  courier/archive — signed ops down (filtered by the leaf's interest),
  the leaf's freshly signed ops up, RBSR over the same framed link with a
  shrunken budget, presence leases for gesture
  (`transport-agnostic-direct-peering-design.md` §7).
- **Device controls.** A device is a keypair, hence an author, hence a
  channel; device-locked write is owner-gating with the device as owner;
  add-policy and remove-policy are independent axes
  (`eventually-consistent-pitchsets.md` §3.1). This is the vocabulary §6
  uses for "who may assert which oscillators."

Honesty inventory carried forward: `hhhs-dag` is **not no_std today** and a
`std` feature is deliberately unshipped until the alloc-only subset actually
compiles (reorg §A.1.3); `entries_topo()` clones full history and the
streaming `for_each_topo` default method is planned-not-landed (ecosystem
§3); `ReachIndex` is Θ(N²) and must never run on a leaf (perf suite §3:
~158 KB at N=100); Ed25519 verify is modeled ms-class on 240 MHz silicon,
unmeasured (perf suite §2: `< 6 ms (model)`).

---

## 3. The core mapping: two framings, one verdict

tutti carries musical **intent** as a verifiable, eventually-consistent
op-DAG whose read is a deterministic fold: pitch-sets with provenance,
channels, registers, continuous facets. AMY renders audio from **events
applied to hidden synth state**. Two ways to connect them:

### 3.1 Framing (a): AMY as a render target

The leaf holds a windowed `DagRead`, folds it to its view, and **projects
view *changes* into AMY events**: the fold's `Revision{added, retracted,
at}` diffs become note-ons/note-offs and parameter updates, pushed into
AMY's delta queue; AMY renders blocks against local time. The shape already
exists in walkie as the MIDI-out edge: `sync_toggle_notes` "diffs the
current set against the sounding set and sends offs for removed, ons for
added" (`src/web/midi.rs:123-134`, per
`eventually-consistent-pitchsets.md` §9.2) — AMY replaces the DIN cable
with an in-process C call, and the vision's own words for the edge apply
verbatim: "reconciliation upstream, events downstream."

### 3.2 Framing (b): AMY events as a tutti `OpLanguage`

Define an "amy" domain: `type Op = amy_event` (or its wire string),
`type View = the synth state`, `fold` = replay the events. The room's log
*is* the performance; every knob twist is a signed op; any peer replays the
same events into the same AMY.

### 3.3 The verdict: (a), with (b)'s one honest kernel absorbed

Framing (b) fails the substrate contract on its own terms, and the failure
is instructive because it is *exactly* the failure the paradigm was built
to escape:

1. **AMY's alphabet does not commute — contract property (c) dies.** AMY
   wire messages are imperative deltas over receiver state: two concurrent
   `v0f440` / `v0f330` have no union, only an order; a partial breakpoint
   update (`bp0=",,,,0.2"`, docs/api.md) is *defined* relative to whatever
   the breakpoint list currently is; `S` (reset) is a global imperative.
   An op-set of AMY events has splice semantics, not merge semantics —
   "two histories of an event stream have a splice, which is to say a lie"
   (`eventually-consistent-pitchsets.md` §1.2). To make the fold
   deterministic you would have to embed AMY's entire delta-application
   semantics, ordered by… what? The events' `time` fields are local
   sysclock milliseconds — wall-clock, which the substrate demotes to
   "display/tiebreak-of-last-resort" (`ops.rs:196-199`).
2. **The "View = synth state" is not a function of the op-set.** AMY's
   sounding state includes envelope positions, oscillator phases, and
   voice-stealing outcomes — all functions of *render-local time* and
   allocation history, not of the event set. Property (b) (equal op-sets ⇒
   identical views) is unattainable for the thing (b) wants to replicate.
3. **It re-imports the stuck note.** A note-on op with a lost note-off op
   is precisely MIDI's confession (§0 of the vision) — except now the
   stuck note is *durable, signed, and replicated*. The whole point of the
   holding/lease split is that nothing sounds that isn't state.
4. **It bloats the log with performance data.** Continuous inflection at
   signed-op granularity is the §11 "expression bandwidth wants to destroy
   the log" failure, verbatim.

Framing (a) gets every one of these right by construction: the shared
object stays the hot set + facets (commuting, verifiable, mergeable); AMY
control state becomes a **lens over the fold** — and a pure function
composed after a deterministic fold is itself deterministic (§7.3 of the
vision), so **two leaves with equal verified op-sets converge on
byte-identical AMY parameter state** (same notes sounding, same patches,
same envelope *shapes* — with only render-local time and phase differing,
which is §1.3's structure/performance split holding its line exactly where
it was drawn).

**But (b) contains one honest kernel worth keeping.** A *subset* of AMY's
control surface is not performance at all — it is declarative timbre
configuration: wave, algorithm, ratio, envelope shapes, filter setup, patch
choice. That subset is register-shaped, and it deserves to be tutti state —
not as raw `amy_event` ops, but as **domain facets**: a causal register per
(channel, parameter), or coarser, a register holding an AMY **patch string**
(AMY itself stores every built-in patch as a wire string, README.md:162 —
the value type already exists and is human-readable). "This room's lead
voice is DX7 patch 128 with this bp0" becomes a reconciling, attributed,
history-scrubbable document; the leaf's edge compiles the resolved register
into `K`/`u`/`A`/`B` messages whenever it changes. Concurrent edits to
*different* parameters merge as different registers; concurrent edits to
the same parameter resolve by causal maxima like any register — automation-
editing semantics for free, per §8.1 of the vision. This is (b)'s ambition
delivered inside (a)'s architecture: **the description is shared; the
performance is local; the imperative wire exists only at the edge, inside
one device, where it can never race anything.**

### 3.4 The concrete mapping table

| tutti (fold output / facet) | AMY (event fields) | notes |
|---|---|---|
| `Revision.added(degree)` | note-on: `synth`/`voices` + `midi_note` (float) + `velocity` | AMY's synth layer does voice allocation + stealing; the leaf never manages oscs for notes |
| `Revision.retracted(degree)` | note-off: same address, `velocity = 0` | the edge owns lifecycle: a playing-notes map mirrors §9.2's `MidiOutputManager` |
| `TunedDegree` × room tuning | **fractional `midi_note`** or `freq_coefs[COEF_CONST]` in Hz | AMY takes float notes natively (amy.h:543) — a 4096-degree `.scl` renders *exactly*, no MPE channel rotation, no bend-range negotiation; strictly better than the MIDI edge (§9.2's honest limits) |
| `pitch_authors` (author-as-channel) | author → AMY `synth` index (or pan/timbre lens) | provenance becomes audible: each author a timbre, or a position in the stereo field |
| weight facet (§2.3 of the vision) | `velocity` / `amp_coefs` | the probabilistic hot set gets a loudness |
| timbre/patch registers (§3.3) | `K` patch, `u` memory-patch string, `w/o/I/G/R` | causal register → compiled setup messages on change |
| durable automation facets (§8.1) | `A`/`B` breakpoint strings + `T`/`X` EG types | §4 in full |
| presence leases (voice pitch, knob drag) | direct coef updates: `f`/`F`/`a`/`Q` at lease-frame rate | never enters the log; lease expiry ⇒ the edge issues the off/decay — the anti-stuck-note discipline applied to the synth |
| panic / bridge death | `S` reset (osc / all / AMY) | All-Notes-Off analog; fail-to-silence on watchdog |
| room-wide dynamics/FX registers | per-bus `h/k/M/x/V` | global effects as causal registers — one knob, room-consensus semantics |

One deliberate non-mapping: AMY's sequencer (`H` ticks, 48 PPQ, tempo) is
**not** driven from tutti state in this design. It is the natural landing
zone for the vision's still-open temporal coordinate (§5.2 gap 1), and
wiring it before that design exists would smuggle in a tempo ontology the
time layer hasn't earned (§11 of the vision). Near-term, the leaf schedules
AMY events "now" (plus a small fixed offset, §6.2); score-time waits.

---

## 4. Continuous facets ↔ AMY envelopes

The §8 doctrine — sparse control points and generator parameters in the
log, evaluation at the renderer — was written without knowing its renderer.
AMY turns out to be an almost suspiciously exact fit, because AMY's own
control model is *already* "generators, not streams":

| tutti §8 concept | AMY realization | fit |
|---|---|---|
| sparse control points `{t, value}` with an interpolation kind | breakpoint lists: `(time_ms, value)` pairs, ≤24 per EG, two EGs per osc (amy.h:238-242) | direct — a durable facet's points compile to a `bp0`/`bp1` string |
| the INTERPOLATION axis ("control points carry their interpolation kind; the audio thread evaluates", vision §8.2) | `eg_type` ∈ {RC/analog, linear, DX7, exponential} per EG (docs/api.md `T`/`X`) | a coarse but real interpolation vocabulary; richer curves (spline knots) decimate to ≤24 breakpoints at the edge |
| generators: "an ADSR is four numbers plus a trigger rule; an LFO is shape, rate, phase" (vision §8.2) | envelopes trigger on note-on/off; LFOs are oscillators routed via `mod_source`; both are a handful of parameters | direct — the facet stores exactly what AMY consumes |
| the blend lens (crossfade of two authors' curves as a derived view, §8.2) | ControlCoefficients: up to 9 scaled sources *summed* per parameter (docs/synth.md) | AMY's coef vector can realize simple blends in-engine (two envelope sources with weights); richer blends evaluate fold-side and emit one compiled curve |
| presence-tier live gesture (lease frames, never history) | direct coef updates (`f`/`F`/`a`/`Q`) — small wire messages, applied at the next block | the fader sweep rides leases at ~10-30 Hz; AMY's per-block parameter smoothing (`mod_synthinfo`'s `last_*` interpolators) covers the gaps |
| promotion (record-arm): decimate the gesture to breakpoints and sign | the same data, re-emitted once as a `bp` string on the durable tier | the §8.1 knife maps to a representation change, not a code path change |

**What the INTERPOLATION axis buys over raw MIDI/events.** MIDI CC is 7-bit
samples at wire rate whose meaning lives in the receiver: a lost CC "leaves
the filter wherever it happened to be, forever" (vision §8.1). The
tutti→AMY path ships the *function*, not its samples:

1. **Loss-immunity.** A missed lease frame decays (1.5 s lease); a missed
   durable breakpoint is repaired by anti-entropy. The filter's trajectory
   is state; there is nothing a dropped packet can strand.
2. **Bandwidth inversion.** Kilobytes of breakpoints replace a continuous
   stream; AMY then evaluates the curve at audio rate *locally, for free* —
   the "store kilobytes; render megahertz" line (vision §8) is literally
   AMY's envelope interpolator running at 44.1 kHz.
3. **Deterministic curve equality.** Equal op-sets ⇒ equal breakpoint sets
   ⇒ equal curves *as functions* on every leaf. MIDI can't state this
   property, let alone check it (`sync_root` can).
4. **Mergeable, attributed editing.** Concurrent edits to different
   breakpoints merge as content-keyed adds; same-point conflicts resolve
   causally; every point knows its author. Automation editing inherits the
   room's collaboration semantics instead of a DAW's lock.
5. **Expressive headroom.** MIDI CC is one 7-bit lane; an AMY EG has 24
   typed breakpoints, four curve families, and a 9-source modulation
   matrix per parameter. The facet vocabulary can grow into that headroom
   (per-holding envelopes: *this held degree* swells; MIDI has no address
   for that fact — a per-note envelope facet rides the holding itself).

**The time-origin wrinkle, faced (again).** AMY envelopes are note-on-
relative — `note_on_clock` is stamped from local sysclock, which is derived
from samples rendered on the local crystal (api.c:237-256). So a durable
envelope facet anchored to a holding's onset is deterministic *per leaf*
but not phase-aligned *across* leaves: two leaves that learned of a holding
20 ms apart run its swell 20 ms apart, and their crystals drift besides.
This is not a bug to fix; it is §1.3's structure/performance split holding
exactly its stated line — "do not pretend a transatlantic LFO is in phase"
(vision §8.2). §6.2 discusses what a mesh can and cannot promise about it.

---

## 5. The minimal leaf stack and the ESP32-S3 budget

### 5.1 The stack, layer by layer

```
        fuller peer (phone / laptop / Tauri desktop)
        — archive, gossip proxy, courier (transport doc §7)
                     │
       USB-CDC serial │ BLE GATT        ← SyncStream, not Transport:
       (length-framed frames)             framed duplex only
                     │
   ┌─────────────────┴─────────────────────────────────────┐
   │ ESP32-S3 leaf firmware                                │
   │                                                       │
   │  link framing (COBS/length-prefix)                    │
   │      │                                                │
   │  ingress verify: Ed25519 + topic binding              │
   │  (verify once, at ingress — contract (a))             │
   │      │                                                │
   │  windowed DagRead impl (M3): bounded suffix W≤256     │
   │  + compacted current view; appended_since→None        │
   │  past the window; own log head for signing continuity │
   │      │                                                │
   │  leaf fold (subset of tutti-core's kit):              │
   │  add-wins degree set + tuning register +              │
   │  timbre/facet registers — "the window is the world"   │
   │      │                                                │
   │  Revision diff → AMY edge (the §3.4 table):           │
   │  playing-notes map, patch compiler, lease applier,    │
   │  fail-to-silence watchdog                             │
   │      │                                                │
   │  AMY: amy_add_event / amy_add_message                 │
   │  → delta queue → block render (256 @ 44.1 kHz,        │
   │    fixed-point, core 1) → I2S DAC                     │
   └───────────────────────────────────────────────────────┘
```

Core split: protocol core (core 0) runs the radio/link stack, ingress
verify, the fold, and the diff→AMY edge; AMY renders on core 1 (its
`platform.multicore` support does this natively on ESP32). The fold runs at
*revision* rate — an op arrived, refold the window — not at block rate; the
edge pushes AMY events with `time = now + small offset` and AMY's own delta
queue handles sample-accurate application. Nothing musical ever waits on
the radio.

### 5.2 Language and build reality

Three honest observations that soften the scariest-looking blocker and
harden a subtler one:

- **`no_std` is not the near-term gate.** On ESP32-S3 the standard Rust
  target (`xtensa-esp32s3-espidf`) runs **with std** over ESP-IDF/newlib.
  `hhhs-dag`'s std-only posture (Mutex, `catch_unwind`, `HashMap`,
  `Arc<dyn Fn>` — reorg §A.1.3) is compatible with an IDF-hosted leaf
  today. The deliberate "no `std` feature until the alloc-only subset
  genuinely compiles" rule (ecosystem §4.2) stays correct — a bare-metal or
  RISC-V `no_std` leaf remains future work — but it does not block an S3
  leaf.
- **The genuine kernel gates are the windowed store and the streaming
  read.** M3 is unbuilt (n=0, deliberately deferred), and
  `entries_topo()`'s full-history `Vec<Entry>` clone is exactly what a
  bounded device cannot afford; the planned `for_each_topo` default method
  (ecosystem §3) should land with or before the windowed store. And the
  leaf fold must use tutti-core's lazy `Reach` shape, never `ReachIndex`
  (Θ(N²), ~158 KB at N=100 — perf suite §3).
- **AMY stays C; the boundary is the wire string.** AMY compiles anywhere
  as C; the Rust leaf links it and, crucially, can drive it through
  `amy_add_message(char*)` rather than the `amy_event` struct. The ASCII
  wire protocol is AMY's *stable* surface (patches are stored in it; Alles
  and Tulip speak it across process and network boundaries), whereas the
  struct layout changed shape across 1.0→1.1 (docs/upgrading.md). Strings
  as FFI: no bindgen churn, no layout coupling, ~256 B max per message.
  The cost — ASCII formatting on a 240 MHz core — is noise at musical
  event rates.

### 5.3 The RAM/CPU budget, honestly

Device envelope: ESP32-S3 = 512 KB on-chip SRAM + optional 2-8 MB PSRAM,
dual Xtensa LX7 @ 240 MHz. After IDF + the radio stack, budget **~300 KB
usable internal RAM** (the perf suite's own budgeting line, §3 there).

| component | RAM | basis |
|---|---|---|
| IDF + BLE (or WiFi) stack + link buffers | ~80-120 KB | platform reality; BLE is lighter than WiFi |
| AMY: 16-32 oscs, effects off, no RAM samples | ~15-40 KB | ~300-500 B/osc state (amy.h:609-675, estimate) + delta pool + block buffers (256×2ch×4 B×few) + stack |
| AMY: chorus/reverb/echo delay lines | 0 (off) or PSRAM | feature-gated; `ram_caps_delay` steers to SPIRAM if wanted |
| tutti window: W≤256 ops | **≤ 64 KB** (target) | the M3 bench gate (perf suite §2); ≤512 B/op retained |
| verify + fold scratch (lazy reach over W, decoded ops) | ~10-30 KB | O(W+E) adjacency, not Θ(N²) |
| AMY edge state (playing-notes map, compiled patch cache) | ~2-4 KB | bounded by sounding-set size |
| **total** | **~170-260 KB** | fits ~300 KB with headroom; PSRAM is the escape valve for effects/samples |

CPU:

- **AMY render**: 16-32 oscillators is a fraction of one core — the
  120-osc Alles configuration ran on strictly weaker silicon (alles
  README.md:13), and `amy_get_render_load()` + the overload failsafe give
  a measured number and a safety net from day one.
- **Ed25519 verify**: modeled `< 6 ms/op` on-device (perf suite §2,
  unmeasured). At musical op rates (a few ops/second) this is trivial on
  the protocol core. The pressure case is **anti-entropy catch-up**: a
  rejoining leaf verifying a burst of ops. Mitigation is already in the
  contract — `SessionBudget` bounds frames/rounds, and verify runs on the
  protocol core so the audio core never notices. A 100-op repair at ~6 ms
  ≈ 0.6 s of background verify: acceptable for a device whose *local*
  playing was never interrupted.
- **The fold**: no-cache doctrine says refold per revision; window-bounded
  W≤256 with a lazy reach is small-milliseconds-class work at revision
  rate. (The M4 micro-probe replaces this sentence with a measured
  number.)

Latency, end to end: local gesture → op signed → fold → AMY event → next
block boundary → I2S ≈ **one-digit ms + AMY's 5.8 ms block + DAC
pipeline** — hardware-latency local rendering, which is precisely the leaf
profile's promise ("local-first is performance-first", vision §6.6).
Remote structure arrives at radio + convergence latency and is *scheduled
into the next block*, not phase-corrected — see §6.2.

**Where the pressure actually is**, ranked: (1) the M3 windowed store not
existing — everything else in this table is configuration; (2) RAM
contention between the radio stack, AMY effects, and the DAG window — all
three have knobs (BLE over WiFi, features off/PSRAM, W); (3) verify bursts
during repair — budgeted, off-audio-core; (4) AMY voice count — the
*least* binding constraint, comfortably sized for a leaf that renders a
room's pitch-set rather than a symphony.

---

## 6. The vision: a distributed, eventually-consistent, verifiable synthesizer mesh

### 6.1 What it is, and the foil that proves the gap

N ESP32 leaves in a room (or a gallery, or three cities) each hold a
windowed replica of the same tutti room, converge on the same sounding
state, and each renders it locally through AMY. The mesh's "patch cables
are signed ops" (vision §6.6) — and now the nodes have speakers.

The instructive foil is **Alles** — the AMY authors' own mesh synth, so
the comparison is family, not strawman. Alles is "a many-speaker
distributed mesh synthesizer" — hundreds of ESP32 speakers, 120 oscillators
each, controlled by AMY wire messages over WiFi **UDP multicast** from a
host (alles README.md:13-49). Its own documentation states its trade-offs
plainly: a **fixed ~1000 ms scheduling latency** so that messages "arrive
to every synth… in time to play in perfect sync" despite WiFi jitter;
clock sync by host-supplied `time` deltas ("if you never send a time
parameter, you're at the mercy of WiFi jitter"); "UDP multicast is
naturally 'lossy'… reliability can sometimes go as low as 70%"; and "your
host should be the main 'sequencer' and keep track of performance state
and future events" (alles README.md:87-115).

Read as a systems statement: Alles is MIDI's architecture at mesh scale —
ephemeral anonymous events, state implicit in receivers, one authoritative
host, loss as a permanent lie. The tutti mesh keeps Alles's render
architecture (AMY on every node, local evaluation) and replaces its
delivery architecture:

| | Alles | tutti + AMY mesh |
|---|---|---|
| shared object | none — an event stream from the host | the hot set + facets: replicated, verifiable state |
| loss | permanent divergence (~70% floor cases) | repaired by anti-entropy; convergence checkable via `sync_root`/`ops_root` |
| authority | the host is the sequencer and the state | none — every leaf is an author; determinism, not consensus (vision §6.4) |
| authorship | none (a `client_id` is an address, not an identity) | Ed25519 per device; a leaf's channel is its keypair; forgery = signature break |
| offline | silence | keep playing locally; union on rejoin (the §4.1 scenario, with speakers) |
| timing | fixed 1 s latency buys sample-tight sync | structure converges eventually; onsets are render-local (see below) — with Alles's own mechanism available as an opt-in (§6.2) |

What is genuinely compelling and *new* here: a gallery grid that survives
any subset losing power and heals when it returns; harmony whose provenance
is audible (per-author synths/panning, §3.4); device-scoped control from
the constraint algebra — each leaf **owner-gates its own timbre channel**
(only the device's key configures its synth) while the room's union
pitch-set stays **shared-clear** (anyone may silence, killing only what
they observed), and an audience can hold an attenuated capability ("add
only, degrees 0-11, expires at close", vision §3.1) — all with zero
coordination, because every policy is a deterministic function of the
op-set. And the mesh is *verifiable* in a sense no synthesizer network has
ever been: two leaves can prove they hold the same music, and every
sounding fact is attributable to a key.

### 6.2 What is hard, said precisely

- **Convergence is not simultaneity.** Two leaves learn of an add at
  different times (gossip spread through the courier topology, then block
  quantization), so a chord change ripples across the room over ~tens of
  ms on a good LAN link and worse over BLE hops. For *texture* — the
  hot-set music walkie actually makes today, chords and scenes rather than
  drum patterns — this ripple is musically benign, even spatially
  interesting. For *rhythm* it is disqualifying, and this design does not
  pretend otherwise: "phase-accurate ensemble timing still needs a
  Link-class clock layer" (vision §11, verbatim).
- **Nobody is authoritative for timing, by design.** The substrate offers
  determinism about *what* is sounding, never consensus about *when*
  (vision §6.4). The upgrade path is exactly Alles's mechanism, opted
  into: a shared clock layer (Link-class, or Alles-style host time-deltas
  over the courier link) plus AMY's `time`/`latency_ms` scheduling — ops
  could then carry an intended onset in a shared coordinate and every leaf
  schedules the AMY event for the same clock instant, trading latency for
  simultaneity precisely as Alles trades 1000 ms for sample-tight sync.
  That wants the vision's open temporal coordinate (§5.2 gap 1) and is
  deliberately out of scope here; the seam (AMY's delta queue keyed by ms)
  is already the right shape to receive it.
- **Clocks drift.** AMY's sysclock is derived from samples rendered on the
  local crystal (api.c:237-247); independent leaves drift by crystal
  tolerance (order tens of ppm — seconds per day). Envelope and LFO phase
  across leaves diverges even for simultaneously-learned holdings. Fine
  for ambience; another reason rhythm needs the explicit clock layer.
- **The courier topology is a tree, not a mesh, at first.** §7 of the
  transport doc makes leaves `SyncStream` clients of fuller peers; leaf ↔
  leaf direct (ESP-NOW, WiFi iroh) is future. A courier's death partitions
  its leaves — which the substrate tolerates (they keep playing, repair
  later), but an installation should run ≥2 fuller peers.
- **Grow-only vs. flash.** Leaves prune by design and "lean on fuller
  nodes to remember, which works only as long as *someone* is a full
  node" (vision §11). An installation's archive is the curator's laptop,
  and that is an operational commitment, not a footnote.

---

## 7. Concrete next experiments, risks, open questions

### 7.1 The experiment ladder (cheapest-decisive first)

1. **The desktop-AMY tutti leaf** — *the decisive cheap one; no hardware,
   no M3.* AMY compiles on desktop with miniaudio out (README.md). Link it
   into a small native binary (or the Tauri host) that joins a walkie room
   as an ordinary peer, subscribes to `Revision` diffs of the fold, and
   drives AMY via `amy_add_message` per the §3.4 table. Acceptance: the
   room's chord is audible; toggling degrees from a phone starts/stops
   AMY voices with no stuck notes across partition/rejoin (kill the
   network mid-chord, keep playing locally, reconnect, hear the union).
   This proves the entire fold→AMY edge — lifecycle map, velocity,
   fail-to-silence — on the real substrate with zero embedded risk.
2. **The microtonal payoff demo** (rides exp 1). Set the room to a non-12
   `.scl`; render degrees as fractional `midi_note`/Hz. One evening of
   work, and it demonstrates the concrete advantage over the MIDI edge:
   exact 31-EDO through a $6 chip's engine, no MPE, no bend-range
   negotiation (contrast: vision §9.2's honest MPE limits).
3. **The timbre register** (rides exp 1). Add one causal-register facet
   carrying an AMY patch (`K` number first; a `u` memory-patch wire
   string later). Two desktop leaves flip timbre convergently; scrub the
   register's history. This validates §3.3's "description shared,
   performance local" claim with real merge semantics.
4. **The dumb S3 leaf** — *hardware bring-up decoupled from M3.* Stock AMY
   firmware on an ESP32-S3 dev board + I2S DAC; the exp-1 desktop bridge
   forwards its compiled AMY wire messages over USB-CDC serial. The leaf
   verifies nothing and holds no DAG — it is a remote AMY, exactly Alles's
   trust model over a wire — but it retires the audio/I2S/build risk and
   gives the M4 micro-probe a host board. (This is also the honest
   fallback product if M3 slips: a *rendering* leaf beats no leaf.)
5. **The verifying leaf** — gated on M3 (+ M4 for numbers). Move ingress
   verify, the windowed store, and the small fold onto the S3 per §5.1;
   the leaf signs its own ops (its keys = its channel) and syncs over the
   same serial link via the standard driver with a shrunken
   `SyncLimits`. Every *(model)* number in the perf suite §2 leaf column
   becomes a measurement here.
6. **The two-leaf convergence demo.** Two verifying leaves + one phone;
   partition a leaf, play into both sides, rejoin, and listen to the
   union reconcile — §4.1 of the vision ("the chord that crosses the
   ocean") performed by hardware. This is the demo that makes "eventual
   consonance" audible, and the first true instance of the §6 mesh.

### 7.2 Risks

- **M3 is the long pole and it is not this design's to schedule.** The
  windowed store is specified (reorg §A.6.3), deferred at n=0, and gates
  the verifying leaf. This design adds the *second* concrete consumer
  pressure for it, but experiments 1-4 are deliberately M3-free.
- **AMY API drift.** `amy_event`'s layout changed across 1.0→1.1
  (docs/upgrading.md) and the project moves fast. Mitigation: pin a rev
  (the hhhs git-pin discipline applies), drive via wire strings not
  structs, and keep the edge compiler in one module.
- **AMY's delta pool under op storms.** `add_delta_to_queue` *drops*
  deltas when the pool can't grow (amy.c:584-588). A repair burst that
  fans into hundreds of AMY events could silently lose parameter updates.
  The edge must therefore diff against the *fold*, not replay history —
  which framing (a) does by construction (a rejoining leaf emits the
  net delta between sounding sets, not the interim ops) — and rate-limit
  compiled setup messages.
- **RAM contention** (radio × AMY effects × window): all knobs exist
  (§5.3) but nobody has turned them together; the M4 probe exists to find
  the surprise.
- **Rust-on-Xtensa build friction** (espup toolchain, IDF versioning) is
  real, unglamorous, and front-loadable via experiment 4's board.
- **The social risk transfers from the vision intact**: shared-clear
  silencing in a physical installation invites retract wars; the
  constraint algebra can express stricter policies per channel, but
  choosing them is curation, not code (vision §11).

### 7.3 Open questions

1. **Where does the edge's velocity come from before the weight facet
   ships?** (Constant, or author-count from `holders` — audible chorus
   for co-held degrees. Cheap to prototype in exp 1.)
2. **Author→synth assignment policy**: stable hash of `AuthorId` into
   AMY's synth indices, or a room register mapping authors to timbres?
   The latter is more musical and more tutti (it's state); start with the
   hash.
3. **Per-holding envelope facets** (§4's headroom): does a swell attach to
   the holding (dies with it) or to the degree (survives re-adds)?
   Content-keying answers differently for each; needs a musical decision
   before a schema one.
4. **The shared clock layer**: Alles-style host deltas over the courier
   link is a weekend; Link-class peer clock sync is a project. Which does
   the first rhythm experiment deserve — and does it wait for the
   temporal-coordinate design (vision §5.2 gap 1) or inform it?
5. **Leaf↔leaf transport** beyond the courier tree: ESP-NOW is
   RAM-cheap and mesh-shaped; real iroh-on-leaf is gated by RAM, not
   radio (transport doc §7). Unexplored.
6. **Does the timbre register hold wire strings or typed parameters?**
   Strings are AMY-native and human-readable but opaque to merge
   (register-atomic); typed per-parameter registers merge finer but need
   a schema. Start atomic (exp 3), split when a real concurrent-edit
   pain shows up.

---

## 8. Summary

AMY is the right renderer for the leaf because it is the §8 doctrine
implemented in C: breakpoint generators, typed interpolation, a modulation
matrix, and block-rendered local evaluation, already proven at 120 voices
on weaker silicon than the target. The right coupling is **AMY as a render
target** — the leaf's windowed fold projected into AMY events at the edge —
because AMY's imperative, receiver-state wire is exactly the shape tutti
exists to supersede as a *shared* object, while being a perfectly good
*local* one; the one register-shaped slice of AMY's surface (timbre/patch
configuration) enters tutti as domain facets rather than raw events. The
ESP32-S3 budget closes with headroom on paper (~170-260 KB of a ~300 KB
envelope) and hinges on one unbuilt piece — the M3 windowed store — which
experiments 1-4 deliberately route around, so the fold→AMY edge, the
microtonal payoff, and the hardware bring-up can all be proven now. The
mesh that results is Alles with the missing organ transplanted in:
speakers that converge on the same music, can prove it, and know who
played what.
