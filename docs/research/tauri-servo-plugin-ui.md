# Tauri + Servo for Walkie-Songie plugin UIs

Status: exploratory research, 2026-08-29. This does not select Servo as a
shipping renderer.

## Short answer

Servo is now a credible application-embedding experiment, and
`tauri-runtime-verso` is a real Tauri 2 runtime built on Verso/Servo. It is not
yet a safe default for Walkie-Songie's CLAP/VST editor.

It should be able to render a reduced Walkie view made from ordinary HTML,
CSS, SVG, JavaScript, and WebAssembly. The complete browser application needs
more validation because it also relies on workers, IndexedDB, WebSockets,
WebRTC data channels, microphone/Web Audio, and Tauri IPC. More importantly,
an audio-plugin editor must attach to the parent window supplied by the DAW;
`tauri-runtime-verso` currently documents ordinary application windows and a
separate `versoview` executable, not cross-platform DAW-owned child-window
embedding.

## Current integration state

- `tauri-runtime-verso` 0.1.0 replaces Tauri's WRY runtime with Verso and
  downloads `versoview` as an external binary.
- Its prebuilt targets are x86-64 Linux, x86-64 Windows, x86-64 macOS, and
  arm64 macOS. Mobile is explicitly unsupported.
- It currently pins Tauri 2.7-era crates and warns that it depends on unstable,
  non-semver-compatible Tauri features. Walkie's desktop shell currently pins
  Tauri 2.11.x, so this is not a drop-in feature switch.
- Its documented security limitation hard-codes the custom-protocol IPC
  `Origin`. It must not load arbitrary websites with privileged capabilities.
- Servo's embedding API is functional and improving, but its own embedding
  tracker remains open and the API continues to receive breaking changes.

Sources:

- <https://github.com/versotile-org/tauri-runtime-verso>
- <https://github.com/servo/servo/issues/30593>
- <https://servo.org/blog/2026/07/31/june-in-servo/>

## Walkie page compatibility

| Walkie requirement | Current assessment | Consequence |
|---|---|---|
| DOM, CSS, SVG, pointer/keyboard input | Plausible | Suitable for a visual smoke test. |
| Rust/WASM and wasm-bindgen glue | Plausible | Servo runs WebAssembly tests, but our exact Trunk bundle still needs a probe. |
| Dedicated Worker replica placement | Plausible but unverified | Run the existing worker placement/handshake acceptance tests in Servo. |
| IndexedDB durable HHHS storage | High risk | Servo's implementation is still experimental and its IndexedDB 3.0 architecture tracker is open; it can block the script event loop. |
| WebSocket rendezvous | Plausible | Servo runs WebSocket WPTs; test binary frames and reconnect behavior. |
| WebRTC data-channel custom Iroh carrier | High risk | The API exists behind Servo's `dom_webrtc_enabled` preference; prove data channels, ICE, and sustained binary traffic before relying on it. |
| Microphone + ScriptProcessor Web Audio | High risk | Walkie currently uses deprecated `ScriptProcessorNode`, not AudioWorklet. Servo has Web Audio support, but permission, device, and sustained callback behavior require measurement. |
| Web MIDI | Avoid in plugin | The plugin bridge owns MIDI natively through nice-plug; the editor should not request Web MIDI. |
| Tauri command/channel IPC | Plausible but security-sensitive | The runtime has examples, but its custom-protocol origin limitation requires a locked local-only CSP/capability surface. |
| DAW-owned child window on Linux/macOS/Windows | Blocking gap | No documented `tauri-runtime-verso` route from a CLAP/VST parent handle to an embedded child view. |

IndexedDB source: <https://github.com/servo/servo/issues/40983>

Servo's current WebRTC WebIDL exposes `createDataChannel`, but WebRTC is behind
a preference and several peer-connection members remain unimplemented:
<https://github.com/servo/servo/blob/main/components/script_bindings/webidls/RTCPeerConnection.webidl>

## Recommended boundary

Keep `BridgeCore` authoritative and renderer-independent:

```text
DAW audio callback <-> fixed bounded MIDI queues <-> BridgeCore worker
                                                   |-- native Iroh + HHHS
                                                   `-- native BLE GATT

plugin editor (egui today; web/Servo experiment later) <-> commands/snapshots
```

The plugin editor should not host the browser's HHHS replica, Iroh endpoint,
WebRTC stack, IndexedDB store, Web Audio capture, or BLE session. A reduced web
editor can render status and issue commands through the same bridge API. This
makes renderer failure non-authoritative and keeps all web-engine activity off
the realtime callback.

## Sensible experiment

1. Keep egui as the shipping nice-plug editor.
2. Build an optional Servo/Verso standalone shell first, using only the reduced
   editor page and native `BridgeCore` IPC.
3. Run a feature probe for WASM startup, pointer/keyboard input, Tauri channels,
   resize/DPI, clean shutdown, and a 30-minute update stream.
4. Separately load the complete Walkie page and record failures for Worker,
   IndexedDB, WebRTC, and microphone audio. This is a browser-compatibility
   experiment, not the plugin architecture.
5. Consider a plugin editor only after Verso can attach to a DAW-provided raw
   parent window on all target platforms without owning the host event loop.

The likely near-term answer is therefore: **Servo can be a valuable standalone
and compatibility experiment, but egui/baseview remains the dependable plugin
editor today.**
