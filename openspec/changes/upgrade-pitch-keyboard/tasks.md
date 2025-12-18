## 1. Setup
- [ ] 1.1 Vendor all-around-keyboard.esm.min.js (18KB) to assets/
- [ ] 1.2 Configure trunk to copy assets/ to dist

## 2. Integration
- [ ] 2.1 Add ESM script tag to index.html (type="module")
- [ ] 2.2 Create wasm-bindgen bindings for all-around-keyboard DOM API
- [ ] 2.3 Replace pitch_grid with all_around_keyboard component wrapper

## 3. Features
- [ ] 3.1 Wire click events from keyboard to toggle_pitch
- [ ] 3.2 Reflect active pitch classes via keysPress/keysRelease
- [ ] 3.3 Update notes-in-octave when tuning changes
- [ ] 3.4 Style the keyboard to fit the app theme

## 4. Verification
- [ ] 4.1 Manual test: click keys toggles pitch state
- [ ] 4.2 Manual test: voice commit highlights correct key
- [ ] 4.3 Manual test: changing tuning updates keyboard
