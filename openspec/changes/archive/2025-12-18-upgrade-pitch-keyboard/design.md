## Context
The app needs a more intuitive pitch class toggle interface. The all-around-keyboard web component is owned by micahscopes (same author), renders circular piano keyboards with d3.js, and supports variable notes-per-octave.

## Goals / Non-Goals
- Goals: Replace flat button grid with circular keyboard, support microtonal tunings
- Non-Goals: Custom keyboard styling beyond basic theme matching, multi-octave display

## Decisions

### JS bundling approach
- Decision: Vendor `all-around-keyboard.esm.min.js` (18KB) in `assets/` directory
- Alternatives considered:
  - UMD bundle: Works but ESM is cleaner for modern browsers
  - CDN link: Requires network, less reliable long-term
- Rationale: ESM is the modern standard, component is self-contained with no external deps

### DOM interop
- Decision: Use web-sys to query element and call methods via js_sys::Reflect
- Alternatives considered:
  - wasm-bindgen JS snippets: Cleaner but requires js/ folder setup
  - gloo crate: Adds dependency for minimal gain
- Rationale: Direct web-sys is sufficient for the small API surface

## Risks / Trade-offs
- Web component may have styling conflicts → mitigated with shadow DOM scoping

## Open Questions
- Single octave or multi-octave display? (default to single for now)
