## 1. Core Voice Conditioner

- [x] 1.1 Create `src/web/voice_conditioner.rs` module with VoiceConditioner struct
- [x] 1.2 Implement RMS energy calculation per frame (~2048 samples)
- [x] 1.3 Implement exponential moving average noise floor estimator
- [x] 1.4 Implement zero-crossing rate calculation for VAD
- [x] 1.5 Implement combined VAD decision (energy + ZCR + optional pitch confidence)
- [x] 1.6 Implement hysteresis gate with configurable open/close thresholds
- [x] 1.7 Implement peak-following AGC with fast attack / slow release

## 2. Reference Level Calibration

- [x] 2.1 Add reference_level field to VoiceConditioner (Option<f32> in dB)
- [x] 2.2 Capture reference level on first confident pitch detection
- [x] 2.3 Compute confidence modifier based on current RMS vs reference (+boost if close, -attenuate if quiet)
- [x] 2.4 Implement slow upward adaptation when louder confident signals detected
- [x] 2.5 Implement very slow downward decay (prevent ambient from lowering bar)
- [x] 2.6 Reset reference on session stop/start

## 3. Integration

- [x] 3.1 Integrate VoiceConditioner into audio capture pipeline in `app.rs`
- [x] 3.2 Pass conditioned audio to pitch detector instead of raw samples
- [x] 3.3 Feed pitch confidence back from detector to conditioner for VAD improvement
- [x] 3.4 Apply reference-level confidence modifier to final confidence score

## 4. Optional Enhancements

- [ ] 4.1 Add optional bandpass filter (80Hz-2kHz) for spectral pre-filtering
- [ ] 4.2 Add bypass mode for debugging/quiet environments
- [ ] 4.3 Expose sensitivity slider in UI (if desired)

## 5. Pitch Confidence Indicator UI

- [x] 5.1 Add pitch position + confidence state to app model (continuous Hz, confidence 0-1)
- [x] 5.2 Render SVG/canvas dot overlay on circular keyboard
- [x] 5.3 Calculate angular position from Hz (continuous, not snapped to pitch class)
- [x] 5.4 Style dot based on confidence (size, opacity, color/glow)
- [x] 5.5 Animate dot smoothly using CSS transitions or requestAnimationFrame
- [x] 5.6 Fade out dot when gate closes / no pitch detected

## 6. Testing

- [ ] 6.1 Test with simulated bar noise + singing audio
- [ ] 6.2 Test with music feedback scenario
- [ ] 6.3 Test quiet singing detection
- [ ] 6.4 Test threshold adaptation when noise level changes
- [ ] 6.5 Test reference level calibration behavior
- [ ] 6.6 Test dot indicator responsiveness and smoothness
- [ ] 6.7 Test dot confidence visualization across range
