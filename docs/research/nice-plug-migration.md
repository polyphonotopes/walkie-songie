# Migrating walkie-songie's plugin from nih-plug to nice-plug

Research report, 2026-08. No code changed; this is the integration/migration plan.

---

## 1. Where nice-plug lives

nice-plug is **not vendored as a source checkout** anywhere under `/laboratory` — it is consumed
from crates.io. The full crate sources are available locally in the cargo registry:

| Crate | Version | Local source |
|---|---|---|
| `nice-plug` | 0.2.2 | `/home/micah/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/nice-plug-0.2.2` |
| `nice-plug-core` | 0.2.0 | `.../nice-plug-core-0.2.0` |
| `nice-plug-derive` | 0.1.2 | `.../nice-plug-derive-0.1.2` |
| `nice-plug-egui` | 0.3.0 | `.../nice-plug-egui-0.3.0` (egui **0.35**, egui-baseview 0.6) |
| `nice-plug-xtask` | 0.1.1 | `.../nice-plug-xtask-0.1.1` |

Upstream: <https://codeberg.org/RustAudio/nice-plug> — a community-led fork/successor of
Robbert van der Helm's nih-plug (authors: Robbert van der Helm + Billy Messenger / BillyDM),
now "the recommended toolkit for Rust audio plugin developers" per its README. ISC licensed.
Requires **rustc ≥ 1.87, edition 2024** (walkie is already edition 2024 / rust-version 1.97.1).

### Local example plugins (the real references)

- **`/laboratory/polyphonotopes-2025/polyphonotopic-transformers/possibly-solfege/`** —
  **best reference**. Same author, same archetype as walkie's plugin: MIDI-only note-effect
  (no audio IO), `#[derive(Params)]` with `#[persist]` fields, a hand-rolled `Editor` wrapping
  `create_egui_editor`, `nice_export_clap!` + `nice_export_vst3!`. Its `Cargo.toml` carries the
  key comment: *"egui is used directly for the editor UI; nice-plug-egui does not re-export it.
  Must match the version nice-plug-egui/egui-baseview resolve to (0.35)."* And `editor.rs:105`:
  *"nice-plug-egui wraps the update in a CentralPanel and passes the `ui`."*
- **`/laboratory/polyphonotopes-2025/polyphonotopic-transformers/pcs-operations/`** —
  six headless MIDI plugins in one cdylib; shows the multi-plugin form of
  `nice_export_clap!(A, B, …)` and `#[id]`-style `IntParam`s.
- **`/laboratory/musical-graphs-app/`** — advanced reference: custom `Editor` impl hosting a
  Bevy app in a baseview child window (`src/plugin_embedded.rs`), plus a real standalone binary
  via `nice_plug::wrapper::standalone::nice_export_standalone::<P>()` (`src/standalone.rs`),
  and a feature-gating pattern where `standalone = ["nice-plug/standalone", …]` so plain
  VST3/CLAP builds don't drag in cpal.
- **xtask wiring**: `/laboratory/polyphonotopes-2025/xtask/` (`nice-plug-xtask = "0.1.1"`,
  `fn main() -> nice_plug_xtask::Result<()> { nice_plug_xtask::main() }`) with the
  `xtask = "run --package xtask --"` alias in `/laboratory/polyphonotopes-2025/.cargo/config.toml`.

---

## 2. The nice-plug programming model

Architecturally it **is** nih-plug — same module layout (`wrapper/{clap,vst3,standalone}`,
`params`, `buffer`, `midi`, `editor`), same trait names, same derive. Deltas are called out below.

### Definition & export
- Implement `Plugin` (in `nice-plug-core/src/plugin.rs`) plus per-format traits
  `ClapPlugin` (`CLAP_ID`, `CLAP_DESCRIPTION`, `CLAP_FEATURES`, optional
  `CLAP_POLY_MODULATION_CONFIG`, `fn remote_controls(...)`) and `Vst3Plugin`
  (`VST3_CLASS_ID: [u8;16]`, `VST3_SUBCATEGORIES`).
- Export with `nice_export_clap!(P)`, `nice_export_vst3!(P)` (accept multiple plugins), and
  `nice_export_standalone::<P>()` from a `main()` for a standalone binary.
- **Formats**: CLAP always; VST3 behind the default `vst3` feature; standalone behind the
  `standalone` feature (cpal + JACK backends, full CLI). **No AU** — same as nih-plug.
- **Licensing win**: VST3 is implemented on the permissive `vst3` crate v0.3 (+`widestring`),
  not the GPLv3 `vst3-sys` that nih-plug uses, so shipping VST3 no longer imposes GPLv3.
- Other crate features: `assert_process_allocs` (default; aborts on allocation in `process()`
  in debug builds), `zstd` (compressed state), `simd` (nightly), `standalone`,
  `tracing-subscriber` (default; nice-plug logs via tracing, macros are `nice_log!`,
  `nice_warn!`, `nice_error!`, `nice_dbg!` — re-exported as `nice_plug::log`).

### The Plugin trait (`nice-plug-core-0.2.0/src/plugin.rs`)
```rust
pub trait Plugin: Default + Send + 'static {
    const NAME / VENDOR / URL / EMAIL / VERSION: &'static str;
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout];   // AudioIOLayout::const_default()
    const MIDI_INPUT / MIDI_OUTPUT: MidiConfig;          // None | Basic | MidiCCs
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;
    const HARD_REALTIME_ONLY: bool = false;
    type SysExMessage: SysExMessage;                     // () if unused
    type BackgroundTask: Send;                           // () if unused
    fn task_executor(&mut self) -> TaskExecutor<Self> { … }
    fn params(&self) -> Arc<dyn Params>;
    fn editor(&mut self, async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> { None }
    fn filter_state(state: &mut PluginState) {}          // NEW: state migrations
    fn initialize(&mut self, &AudioIOLayout, &BufferConfig, &mut impl InitContext<Self>) -> bool;
    fn reset(&mut self) {}
    fn process(&mut self, &mut Buffer, &mut AuxiliaryBuffers,
               &mut impl ProcessContext<Self>) -> ProcessStatus;   // f32 buffers
    fn deactivate(&mut self) {}
    fn track_info_updated(&mut self, info: TrackInfo) {} // NEW
    fn setup_logger() -> Option<bool> { None }           // NEW
}
```
`ProcessStatus::{Error, Normal, Tail(u32), KeepAlive}` unchanged. MIDI in/out via
`context.next_event()` / `context.send_event(NoteEvent::NoteOn { timing, voice_id, channel,
note, velocity })` — **field-for-field identical to nih-plug** (verified against
`nice-plug-core-0.2.0/src/midi.rs` and the possibly-solfege `process()`).

### Parameters
Identical to nih-plug: `#[derive(Params)]` with `#[id = "…"]` on `FloatParam` / `IntParam` /
`BoolParam` / `EnumParam<T>`, `#[persist = "…"]` on any Serde-serializable field, `#[nested]`
for groups/arrays. Ranges (`FloatRange`/`IntRange`), smoothing (`Smoother`,
`SmoothingStyle`), formatters, callbacks all carried over. `PersistentField` is implemented
for `std::sync::Mutex<T>`, `RwLock`, `parking_lot` types, `AtomicRefCell`, atomics,
`AtomicCell`, and `Arc<…>` of these (`nice-plug-core-0.2.0/src/params/persist.rs`) — walkie's
`Mutex<String>` persist field works as-is. State is the same human-readable JSON
(`PluginState`), now with a `version` field + `filter_state()` hook for migrations and an
optional `zstd` feature.

### GUI / editor story
Modular `Editor` trait over **baseview** windows; any baseview-capable toolkit works.
First-party adapters: **nice-plug-egui** (egui 0.35 via egui-baseview 0.6; `opengl` default
or `wgpu` feature), nice-plug-iced, WIP slint, third-party vizia-plug. So **egui is fully
supported — walkie's editor ports rather than being rewritten**, with these API deltas vs
`nih_plug_egui`:

| nih_plug_egui (walkie's pinned rev 28b149e) | nice-plug-egui 0.3.0 |
|---|---|
| re-exports `egui` (0.31.1 in walkie's lock) | does **not** re-export; add direct `egui = "0.35"` dep |
| `EguiState::from_size(u32, u32)` | `EguiState::from_size(LogicalSize::new(f32, f32))` (`nice_plug::editor::dpi`) |
| `create_egui_editor(state, user, build, update)` | `create_egui_editor(state, user, EguiNiceSettings, build, update)` — settings adds window title + `GraphicsConfig` |
| `build: Fn(&Context, &mut T)` | `build: Fn(&Context, &mut ExtraOutputCommands, &mut T)` |
| `update: Fn(&Context, &ParamSetter, &mut T)` — you open your own `CentralPanel` | `update: Fn(&mut Ui, &ParamSetter, &mut ExtraOutputCommands, &mut T)` — adapter already wraps a `CentralPanel` and hands you the `Ui` |
| `Editor::spawn -> Box<dyn Any + Send>` | `-> Box<dyn Any>` (no `Send`; matches baseview `WindowHandle`) |
| `Editor::size() -> (u32, u32)` | `-> Size` (dpi type; `egui_state.size().into()`) |
| `set_scale_factor(f32)` | `set_scale_factor(f64)` |

State sharing with the audio thread is unchanged: clone `Arc<Params>` + `Arc<Mutex<…>>` /
atomics into the closures; `ParamSetter` over `GuiContext` for automated changes;
`AsyncExecutor<P>` for realtime-safe background tasks.

### Threading / realtime model
Same as nih-plug: `process()` takes `&mut self` on the audio thread; the editor lives on the
host's UI thread in a baseview child window; `assert_process_allocs` (default on) aborts on
allocation inside `process()` in debug builds; `BackgroundTask` values must be `Send` and
heap-allocation-free; denormals are handled by the wrapper.

---

## 3. Walkie-songie's current plugin

Files: `/laboratory/walkie-songie/src/plugin/{mod.rs (678), editor.rs (302), params.rs (78)}`,
exports in `src/lib.rs:21-28`, stub bin `src/plugin_main.rs`, bundler `xtask/`
(`nih_plug_xtask` git). Feature (`Cargo.toml:47`):
`plugin = ["dep:nih_plug", "dep:nih_plug_egui", "dep:crossbeam-channel", "dep:egui_extras", "dep:image"]`
with `nih_plug`/`nih_plug_egui` from git (locked at rev `28b149e`; egui 0.31.1 + 0.30 both in
the lock). `qrcode` is a top-level dep.

**What it does**: a MIDI-only "note effect" utility (no audio IO; `AUDIO_IO_LAYOUTS` has no
channels), CLAP + VST3, `MidiConfig::Basic` both directions:
- `initialize()` spawns a networking thread (crossbeam `NetCommand`/`NetEvent` channels,
  current-thread tokio runtime); `deactivate()` sends `Shutdown`.
- `process()`: reads DAW MIDI in — notes on the *pitch-classes channel* toggle room pitch
  classes, notes on the *voice channel* set a monophonic voice pitch — and drains `NetEvent`
  deltas from the room, emitting NoteOn/NoteOff on three param-configurable channels
  (unified pitch classes, voice pitches, piece pitches).
- **Params**: `#[persist] channel_address: Mutex<String>`, three `BoolParam` enables, three
  `IntParam` MIDI channels (1–16). Nothing uses smoothing or automation-sensitive behavior.
- **Editor** (`editor.rs`): custom `Editor` impl delegating to `create_egui_editor`;
  QR-code texture (qrcode + image → `egui::ColorImage` → `ctx.load_texture`), connection
  status, copy-share-link (`ctx.copy_text`), shuffle/join channel controls. Shares state via
  `Arc<Mutex<usize>>` peers count, `Arc<Mutex<Option<String>>>` peer id, and the
  `Sender<NetCommand>`.
- `type BackgroundTask = NetCommand` is declared but `task_executor()` is never implemented
  and the `AsyncExecutor` is ignored — vestigial; the plugin uses its own thread instead.
- `egui_extras` is in the feature deps but **unused** in `src/plugin/` — droppable.

**Critical finding — the plugin feature is already bit-rotted.** `run_networking_loop`
(`src/plugin/mod.rs:395-676`) is written against **libp2p** (gossipsub/identify/
`SwarmBuilder`) + the yrs `RoomState`, but `libp2p` appears **zero** times in `Cargo.toml`
and `Cargo.lock` — the repo has pivoted to iroh + p2panda/HHHS (`openspec/changes/
pivot-to-tauri-iroh/`,
`archive/2026-08-27-rewrite-p2panda-hhhs-stack/`; yrs is marked "legacy" in
`Cargo.toml:80`). `cargo check --features plugin` cannot compile today. The nice-plug port
therefore has to be paired with (or sequenced before) re-pointing the net thread at the new
`src/net` + room op-log stack.

---

## 4. Concept-by-concept migration map

| Concept | nih-plug (current) | nice-plug | Change in `src/plugin` |
|---|---|---|---|
| Prelude | `nih_plug::prelude::*` | `nice_plug::prelude::*` | import swap |
| Plugin trait | consts + initialize/process/deactivate | **identical signatures** | none (optionally adopt `track_info_updated`, `setup_logger`) |
| Params | `#[derive(Params)]`, `#[id]`, `#[persist]` `Mutex<String>` | identical incl. persist impls | **none** — `params.rs` compiles as-is after import swap |
| `process()` / MIDI | `next_event`/`send_event`, `NoteEvent::{NoteOn,NoteOff}` | identical fields | none |
| Log macros | `nih_log!`, `nih_warn!` | `nice_log!`, `nice_warn!` (tracing-backed) | rename (3 call sites in mod.rs) |
| ClapPlugin / Vst3Plugin | const-for-const | identical | none |
| Export | `nih_export_clap!/vst3!` in `lib.rs` | `nice_export_clap!/nice_export_vst3!` | rename in `src/lib.rs:26-28` |
| Editor trait | `spawn -> Box<dyn Any + Send>`, `size -> (u32,u32)`, `set_scale_factor(f32)` | `Box<dyn Any>`, `-> Size`, `(f64)` | 3 signature tweaks in `editor.rs` |
| egui adapter | `nih_plug_egui` (re-exports egui 0.31) | `nice-plug-egui` 0.3 + direct `egui = "0.35"` | see GUI verdict below |
| State persistence | JSON keyed by `#[id]`/`#[persist]` | same format + `version`/`filter_state` | none; verify old DAW sessions reload (IDs unchanged, so expected-compatible) |
| Background tasks | `AsyncExecutor` (unused) | same | none; optionally simplify `BackgroundTask` to `()` |
| Bundler | `xtask` → `nih_plug_xtask` (git) | `nice-plug-xtask = "0.1.1"` (crates.io) | one-line dep swap in `xtask/Cargo.toml`; same `cargo xtask bundle walkie-songie` UX |
| Formats | CLAP + VST3 | CLAP + VST3 (+ optional standalone) | unchanged; **gains** an easy real standalone for `walkie-songie-plugin` bin; VST3 sheds the GPLv3 `vst3-sys` obligation |
| Toolchain | git deps, edition 2021 upstream | crates.io, edition 2024, rustc ≥1.87 | fine (walkie: edition 2024, rust 1.97.1) |

### The GUI verdict: **port, not rewrite**
nice-plug-egui is the same egui-on-baseview adapter lineage (egui-baseview by BillyDM in both).
`editor.rs` keeps ~90% of its body. The concrete edits:

1. `use nih_plug_egui::{EguiState, create_egui_editor, egui}` →
   `use nice_plug_egui::{EguiState, EguiNiceSettings, create_egui_editor};` + direct `use egui;`
   and `use nice_plug::editor::dpi::{LogicalSize, Size};`
2. `EguiState::from_size(300, 400)` → `EguiState::from_size(LogicalSize::new(300.0, 400.0))`.
3. `create_egui_editor(state, EditorState::default(), |_, _| {}, move |egui_ctx, _setter, state| …)`
   → add `EguiNiceSettings::default()` third arg; build closure `|_ctx, _extra, _state| {}`;
   update closure `move |ui, _setter, _extra, state|` — **delete the
   `egui::CentralPanel::default().show(ctx, …)` wrapper** (the adapter provides it) and
   thread `ui` into `draw_editor`; recover the context via `ui.ctx()` where `copy_text`,
   `load_texture`, `request_repaint` are called.
4. `Editor` impl: drop `+ Send` from spawn's return type, `fn size(&self) -> Size
   { self.egui_state.size().into() }`, `set_scale_factor(&self, _: f64)`.
5. egui 0.31 → 0.35 breakage in this file is tiny: the `egui::ColorImage { size, pixels }`
   struct literal fails (0.35 added `source_size: Vec2` — see
   `.../epaint-0.35.0/src/image.rs:48`); switch to a constructor or set `source_size`.
   `ctx.copy_text`, `load_texture`, `TextureOptions::NEAREST`, `text_edit_singleline`,
   `Key::Enter` all still exist in 0.35.
6. Drop `egui_extras` from the feature (unused); keep `image` + `qrcode` for the QR texture.

Pattern-match everything against
`/laboratory/polyphonotopes-2025/polyphonotopic-transformers/possibly-solfege/src/editor.rs`,
which is exactly this structure already working on nice-plug 0.2.2.

### Feature-gap check
Nothing walkie uses lacks a nice-plug equivalent. In the other direction nice-plug adds
`filter_state` migrations, `track_info_updated`, remote-controls pages, zstd state, and the
permissive VST3 stack — all optional. One soft spot: nice-plug-egui has no `default_fonts`
feature toggle like nih_plug_egui did; fonts come via egui-baseview 0.6 defaults
(possibly-solfege renders text fine with it, so treat as verified in practice).

---

## 5. Staged plan, effort, risks

**Stage 0 — decide the net-thread story (blocker, not nice-plug's fault).** The `plugin`
feature doesn't compile because `run_networking_loop` still speaks libp2p+yrs. Options:
(a) port it to the new iroh-gossip + p2panda/HHHS stack in `src/net` + `src/room` (aligns
with the historical
`openspec/changes/archive/2026-08-27-rewrite-p2panda-hhhs-stack/` plan), or (b) temporarily stub the thread
(keep `NetCommand`/`NetEvent`, return errors) so the nice-plug migration can be validated
in isolation. Recommend (b) first, (a) as its own change.

**Stage 1 — dependency swap (~30 min).** In `Cargo.toml`: replace the two git deps with
`nice-plug = "0.2.2"`, `nice-plug-egui = "0.3.0"`, add `egui = "0.35"` (optional,
plugin-gated), drop `egui_extras`; keep `crossbeam-channel`, `image`. In `xtask/Cargo.toml`:
`nih_plug_xtask` git → `nice-plug-xtask = "0.1.1"`.

**Stage 2 — mechanical rename (~1 h).** `src/lib.rs` export macros; `src/plugin/mod.rs` +
`params.rs` prelude imports and `nih_log!`/`nih_warn!` → `nice_log!`/`nice_warn!`.
Optionally set `type BackgroundTask = ()` and delete the unused `NetCommand` task plumbing.

**Stage 3 — editor port (~2–3 h).** The six edits under "GUI verdict". Compile with
`cargo check --features plugin` against the stub from Stage 0.

**Stage 4 — bundle + host validation (~half a day).** `cargo xtask bundle walkie-songie
--release`; load CLAP + VST3 in Reaper/Bitwig; check editor open/resize/DPI, param
automation, and that a project saved with the nih-plug build reloads (state JSON is
format-compatible; param IDs unchanged).

**Stage 5 — optional wins.** Real standalone: gate `nice-plug/standalone` behind a walkie
`standalone` feature and make `src/plugin_main.rs` call
`nice_export_standalone::<WalkieSongiePlugin>()` (pattern:
`/laboratory/musical-graphs-app/src/standalone.rs`). Adopt `filter_state` if params ever
break compat. Revisit VST3 distribution now that the GPLv3 constraint is gone.

**Effort**: the nih→nice port proper is **about a day** (Stages 1–4). Reviving the
networking thread on the new stack (Stage 0a) is the genuinely large item — days, and it's
really part of the p2panda rewrite, not of this migration.

**Top risks / open questions**
1. **Net-layer bit-rot masks the port**: without Stage 0 the feature can't even be compiled,
   and without Stage 0a it can't be validated end-to-end (MIDI-out from real room deltas).
2. **egui 0.31 → 0.35 behavioral drift**: beyond `ColorImage`, minor layout/font/style
   changes may shift the small 300×400 UI; needs a visual pass in a host (per project rule:
   don't declare visual issues fixed without checking).
3. nice-plug is young (0.2.x; `create_egui_editor` already grew a settings arg between
   adapter versions) — expect some API churn on future bumps; macOS support is "limited
   testing" per README.
4. State-compat assumption (same JSON schema) should be verified with one real saved session
   before deleting the nih-plug branch.

### Sources
- Crate sources: registry paths in section 1; key files
  `nice-plug-core-0.2.0/src/{plugin.rs,params/persist.rs,midi.rs}`,
  `nice-plug-0.2.2/src/{prelude.rs,wrapper/{clap.rs,vst3.rs}}`,
  `nice-plug-egui-0.3.0/src/{lib.rs,editor.rs}`, READMEs.
- Examples: `possibly-solfege/src/{lib.rs,editor.rs}`, `pcs-operations/src/lib.rs`,
  `musical-graphs-app/src/{plugin_embedded.rs,standalone.rs}`, `polyphonotopes-2025/xtask/`.
- Walkie: `src/plugin/{mod,editor,params}.rs`, `src/lib.rs`, `src/plugin_main.rs`,
  `Cargo.toml`, `Cargo.lock` (nih_plug rev `28b149e`, egui 0.31.1), `xtask/Cargo.toml`.
- Upstream: <https://codeberg.org/RustAudio/nice-plug>, <https://docs.rs/nice-plug>.
