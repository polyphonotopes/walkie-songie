# Change: Add Voice Input Mode

> **Status (2026-07-30): desktop implementation superseded by
> `pivot-to-tauri-iroh`.** The user workflow and local WebAudio/SwiftF0
> processing remain relevant. Tuning, pitch projection, shared voice presence,
> and durable state now follow the validated native-runtime design in the pivot.

## Why
Musicians need a natural way to input pitches by singing. The current system only has P2P connectivity but no way to actually share musical content. Voice input lets users sing a tone, see real-time feedback with a closeness indicator, and commit it to a shared pitch class set. The environment involves phone mics in rooms with speaker playback, requiring adaptive noise handling.

## What Changes
- Add microphone pitch detection using Rust crate via wasm AudioWorklet
- Implement dynamic noise gate that adapts to ambient levels
- Implement press-hold-release UI pattern for pitch capture
- Support SCL (Scala) tuning files stored as per-room text CRDT
- Display pitch feedback with note name and cents deviation (closeness indicator)
- Add local pitch class state using yrs YMap (per-peer sets)
- Sync active pitch classes to room peers via yrs
- Configurable room combination method (union, intersection, extensible to transformations)

## Impact
- Affected specs: NEW `voice-input`, NEW `pitch-state-sync`
- Affected code: New modules for audio, tuning, yrs sync, web UI
- Architecture: Core library (platform-agnostic) + dominator web app (prototype)
- Dependencies:
  - `pitch` crate (BCF - fast feedback) + `pyin-rs` crate (pYIN - accurate commit)
  - `yrs` / `ywasm` (CRDT sync)
  - `dominator` + `futures-signals` (web UI)
  - Web Audio API AudioWorklet (browser)
- Estimated wasm size: ~120-170KB (BCF ~5KB, pYIN ~50-80KB, yrs ~50KB, dominator ~20KB)
