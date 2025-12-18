# Change: Upgrade Pitch Grid to All-Around Keyboard

## Why
The current pitch grid is a flat row of buttons. The all-around-keyboard web component (by micahscopes) provides a circular piano keyboard that's more intuitive for musical interaction and supports variable notes-per-octave for microtonal tunings.

## What Changes
- Add pnpm + package.json for JS dependencies (project convention for JS deps)
- Integrate all-around-keyboard web component into the trunk build
- Replace `pitch_grid` component with wrapper for `<all-around-keyboard>`
- Wire up keyboard events: click to toggle pitch class, reflect active pitches
- Support dynamic notes-in-octave based on current tuning

## Impact
- Affected specs: NEW `pitch-keyboard-ui`
- Affected code: `src/web/components.rs`, `index.html`, new `package.json`
- Build: trunk must copy/bundle JS from node_modules
- Removes parking lot item: "Piano keyboard UI"
