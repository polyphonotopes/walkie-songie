## Context
The app operates in noisy bar environments where:
- Ambient noise levels are high and variable (conversations, music, clinking)
- Audio from the mic feeds into a music generator, creating intentional feedback loops
- Users sing/hum into their phone to input pitches
- False triggers from ambient sound would disrupt the musical experience

The existing Dynamic Noise Gate spec is underspecified for this use case. We need a more sophisticated audio conditioning pipeline.

## Goals
- Reliably isolate singing voice from bar noise and musical feedback
- Automatically adapt to changing noise floor without manual calibration
- Provide stable, normalized audio to pitch detector for consistent results
- Fast gate attack (~5ms) so voice onset isn't missed
- Smooth gate release (~50-100ms) to handle natural voice variation

## Non-Goals
- Full noise cancellation/removal (we just need gating, not cleaning)
- Multi-voice separation (single user per device)
- Speech recognition (just pitch detection)

## Decisions

### 1. Noise Floor Estimation: Exponential Moving Average
**Decision**: Use EMA with slow decay (τ ~1s) on RMS energy during "quiet" periods
**Alternatives considered**:
- Median filtering: More robust to outliers but higher memory/compute
- Spectral minimum tracking: Better but complex for MVP
**Rationale**: EMA is simple, low-latency, and adequate for slowly-varying bar noise

### 2. Voice Activity Detection: Energy + Zero-Crossing + Pitch Confidence
**Decision**: Multi-feature VAD combining:
- RMS energy above adaptive threshold
- Zero-crossing rate in speech range (typical: 50-150 per 10ms for speech)
- Pitch detector confidence (if pitch is detected with high confidence, likely voice)
**Alternatives considered**:
- ML-based VAD (WebRTC VAD, Silero): More accurate but adds dependency/latency
- Pure energy threshold: Too many false positives from music
**Rationale**: Combining simple features gives good discrimination without ML overhead

### 3. Automatic Gain Control: Peak-following with soft knee
**Decision**: Track peak envelope with fast attack (~10ms) / slow release (~300ms), apply gain to target -12dBFS
**Alternatives considered**:
- RMS-based AGC: Smoother but slower response
- Hard limiter: Distorts on peaks
**Rationale**: Peak-following handles dynamic singing better

### 4. Spectral Filtering: Optional bandpass
**Decision**: Optional 80Hz-2kHz bandpass to reject low rumble and high-frequency noise
**Rationale**: Human voice fundamentals are typically 80-400Hz, but harmonics extend higher. 2kHz captures enough for pitch detection while rejecting sibilants and high-frequency noise.

### 5. Hysteresis Gate
**Decision**: Open threshold at noise_floor + 9dB, close threshold at noise_floor + 3dB
**Rationale**: 6dB hysteresis prevents rapid flutter when signal hovers near threshold

### 6. Reference Level Calibration (Trust Anchor)
**Decision**: Capture RMS level on first confident pitch detection as "reference level", use it to modulate confidence scoring
**How it works**:
- When user starts singing and first confident pitch is detected, capture that RMS as reference
- Subsequent signals within ~3dB of reference get confidence boost (likely intentional close-mic singing)
- Signals >12dB below reference get confidence attenuation (likely ambient bleed)
- Reference adapts upward slowly when louder confident signals detected (user moved closer)
- Reference decays downward very slowly (prevents ambient from lowering the bar)
**Alternatives considered**:
- Fixed reference: Doesn't adapt to user distance from mic
- Pure AGC without reference: Normalizes everything equally, loses "close singing" discrimination
**Rationale**: The first confident signal establishes what "real singing" sounds like for this session. This creates a trust anchor that discriminates against quieter ambient sounds even if they have some pitch content (like background music).

## Architecture

```
[Mic] → [Bandpass Filter] → [RMS Energy] ──→ [VAD Decision]
                                 │                  │
                                 ↓                  ↓
                         [Noise Estimator] ──→ [Gate] → [AGC] → [Pitch Detector]
                                 │                              ↓
                                 │                        [Confidence]
                                 ↓                              │
                         [Reference Level] ←── (first confident) ←┘
                                 │
                                 ↓
                         [Confidence Modifier] → Final confidence for UI
```

## Risks / Trade-offs
- **Too aggressive gating** → Cuts off quiet singing
  - Mitigation: Tunable sensitivity, expose "voice sensitivity" slider
- **Too permissive gating** → Music bleeds through
  - Mitigation: Use pitch confidence as secondary VAD signal
- **AGC pumping** → Noticeable volume swings
  - Mitigation: Slow release time, optional bypass

## Open Questions
- Should we expose any tuning parameters to the user (sensitivity slider)?
- Do we need a "calibrate noise floor" button, or is auto-adaptation sufficient?
- Should the conditioner run in AudioWorklet alongside pitch detection, or in main thread?
