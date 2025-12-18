# Change: Add Dynamic Voice Conditioner

## Why
The app will be used in noisy bar environments where audio from the mic feeds into a music generator, creating feedback loops. We need intelligent audio preprocessing that isolates the user's singing voice from ambient bar noise and musical feedback, ensuring only intentional vocal input triggers pitch detection. Additionally, users need better visual feedback showing what the system is hearing in real-time.

## What Changes
- **MODIFIED** Dynamic Noise Gate requirement → upgraded to full Voice Conditioner with:
  - Adaptive noise floor estimation (learns ambient level)
  - Voice Activity Detection (VAD) to distinguish singing from music/noise
  - Automatic Gain Control (AGC) to normalize voice level for consistent pitch detection
  - Spectral filtering to focus on human voice frequencies
  - Hysteresis gating to prevent flutter at threshold boundary
- **ADDED** Pitch Confidence Indicator - a circling dot overlay on the keyboard showing:
  - Real-time detected pitch position (continuous, not snapped)
  - Confidence level via dot appearance (size, opacity, brightness)
  - Smooth animation as pitch slides between notes
- **ADDED** Continuous Pitch Tracking Display for smooth, responsive visual feedback

## Impact
- Affected specs: `voice-input`, `pitch-keyboard-ui`
- Affected code: `src/web/audio.rs`, `src/web/app.rs`, new `src/web/voice_conditioner.rs`
- Dependencies: May require WebAudio AnalyserNode or additional DSP
