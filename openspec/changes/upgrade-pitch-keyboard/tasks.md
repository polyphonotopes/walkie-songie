## 1. Setup
- [x] 1.1 Vendor all-around-keyboard.esm.min.js (18KB) to assets/
- [x] 1.2 Configure trunk to copy assets/ to dist

## 2. Integration
- [x] 2.1 Add ESM script tag to index.html (type="module")
- [x] 2.2 Create wasm-bindgen bindings for all-around-keyboard DOM API
- [x] 2.3 Replace pitch_grid with all_around_keyboard component wrapper

## 3. Features
- [x] 3.1 Wire click/tap events from keyboard to toggle_pitch
- [x] 3.2 Reflect active pitch classes via keysPress/keysRelease (pressed state)
- [x] 3.3 Reflect detected pitch via notesLight/notesDim (lit state)
- [x] 3.4 Update notes-in-octave and raised-notes when tuning changes
- [x] 3.5 Implement raised-notes heuristic (12-TET standard, others pie mode)
- [x] 3.6 Style keyboard with compact layout, overlay pitch info, active pitches list

## 4. Verification
- [ ] 4.1 Manual test: click keys toggles pitch state
- [ ] 4.2 Manual test: voice commit highlights correct key
- [ ] 4.3 Manual test: changing tuning updates keyboard
