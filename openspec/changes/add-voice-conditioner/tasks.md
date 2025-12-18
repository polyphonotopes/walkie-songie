## 1. Core Voice Conditioner

- [ ] 1.1 Create `src/web/voice_conditioner.rs` module with VoiceConditioner struct
- [ ] 1.2 Implement RMS energy calculation per frame (~2048 samples)
- [ ] 1.3 Implement exponential moving average noise floor estimator
- [ ] 1.4 Implement zero-crossing rate calculation for VAD
- [ ] 1.5 Implement combined VAD decision (energy + ZCR + optional pitch confidence)
- [ ] 1.6 Implement hysteresis gate with configurable open/close thresholds
- [ ] 1.7 Implement peak-following AGC with fast attack / slow release

## 2. Integration

- [ ] 2.1 Integrate VoiceConditioner into audio capture pipeline in `audio.rs`
- [ ] 2.2 Pass conditioned audio to pitch detector instead of raw samples
- [ ] 2.3 Feed pitch confidence back from detector to conditioner for VAD improvement

## 3. Optional Enhancements

- [ ] 3.1 Add optional bandpass filter (80Hz-2kHz) for spectral pre-filtering
- [ ] 3.2 Add bypass mode for debugging/quiet environments
- [ ] 3.3 Expose sensitivity slider in UI (if desired)

## 4. Pitch Confidence Indicator UI

- [ ] 4.1 Add pitch position + confidence state to app model (continuous Hz, confidence 0-1)
- [ ] 4.2 Render SVG/canvas dot overlay on circular keyboard
- [ ] 4.3 Calculate angular position from Hz (continuous, not snapped to pitch class)
- [ ] 4.4 Style dot based on confidence (size, opacity, color/glow)
- [ ] 4.5 Animate dot smoothly using CSS transitions or requestAnimationFrame
- [ ] 4.6 Fade out dot when gate closes / no pitch detected

## 5. Testing

- [ ] 5.1 Test with simulated bar noise + singing audio
- [ ] 5.2 Test with music feedback scenario
- [ ] 5.3 Test quiet singing detection
- [ ] 5.4 Test threshold adaptation when noise level changes
- [ ] 5.5 Test dot indicator responsiveness and smoothness
- [ ] 5.6 Test dot confidence visualization across range
