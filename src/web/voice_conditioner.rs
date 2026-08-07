//! Voice conditioner for isolating singing voice in noisy environments.
//!
//! Combines adaptive noise estimation, VAD, AGC, hysteresis gating,
//! and reference level calibration for use in noisy bar environments
//! with musical feedback.

/// Voice conditioner configuration.
#[derive(Clone, Debug)]
pub struct VoiceConditionerConfig {
    /// Time constant for noise floor EMA (seconds).
    pub noise_floor_tau: f32,
    /// Gate open threshold above noise floor (dB).
    pub gate_open_db: f32,
    /// Gate close threshold above noise floor (dB).
    pub gate_close_db: f32,
    /// Gate hold time before closing (seconds).
    pub gate_hold_time: f32,
    /// AGC target level (dBFS).
    pub agc_target_dbfs: f32,
    /// AGC attack time (seconds).
    pub agc_attack: f32,
    /// AGC release time (seconds).
    pub agc_release: f32,
    /// Reference level boost range (dB) - signals within this get boosted.
    pub ref_boost_range_db: f32,
    /// Reference level attenuation threshold (dB) - signals below this get attenuated.
    pub ref_attenuate_threshold_db: f32,
    /// Reference level upward adaptation rate (0-1, per frame).
    pub ref_adapt_up_rate: f32,
    /// Reference level downward decay rate (0-1, per frame).
    pub ref_decay_down_rate: f32,
    /// Initial activation threshold (dBFS) - requires strong signal to start.
    pub initial_activation_db: f32,
}

impl Default for VoiceConditionerConfig {
    fn default() -> Self {
        Self {
            noise_floor_tau: 1.0,             // 1 second time constant
            gate_open_db: 9.0,                // Open at noise + 9dB
            gate_close_db: 3.0,               // Close at noise + 3dB (6dB hysteresis)
            gate_hold_time: 0.05,             // 50ms hold before closing
            agc_target_dbfs: -12.0,           // Target -12dBFS
            agc_attack: 0.01,                 // 10ms attack
            agc_release: 0.3,                 // 300ms release
            ref_boost_range_db: 3.0,          // Boost if within 3dB of reference
            ref_attenuate_threshold_db: 12.0, // Attenuate if >12dB below reference
            ref_adapt_up_rate: 0.1,           // Slow upward adaptation
            ref_decay_down_rate: 0.001,       // Very slow downward decay
            initial_activation_db: -24.0,     // Require strong signal (-24dBFS) to activate
        }
    }
}

/// Output from voice conditioner processing.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ConditionerOutput {
    /// Whether the gate is open (voice detected).
    pub gate_open: bool,
    /// Conditioned samples (after AGC, if gate open).
    pub samples: Vec<f32>,
    /// Current RMS level in dB.
    pub rms_db: f32,
    /// Current noise floor estimate in dB.
    pub noise_floor_db: f32,
    /// Confidence modifier from reference level comparison (0.5 to 1.5).
    pub confidence_modifier: f32,
    /// Whether reference level has been calibrated.
    pub reference_calibrated: bool,
}

/// Voice conditioner state.
#[allow(dead_code)]
pub struct VoiceConditioner {
    config: VoiceConditionerConfig,
    sample_rate: f32, // Kept for potential future use (bandpass filter)

    // Noise floor estimation
    noise_floor_db: f32,
    noise_floor_alpha: f32,

    // Gate state
    gate_open: bool,
    gate_hold_samples: usize,
    gate_hold_counter: usize,

    // AGC state
    agc_gain_db: f32,
    agc_attack_alpha: f32,
    agc_release_alpha: f32,

    // Reference level calibration
    reference_level_db: Option<f32>,

    // VAD helpers
    prev_zcr: f32,

    // Initial activation - requires strong signal to "wake up"
    activated: bool,
}

impl VoiceConditioner {
    /// Create a new voice conditioner.
    pub fn new(sample_rate: f32) -> Self {
        Self::with_config(sample_rate, VoiceConditionerConfig::default())
    }

    /// Create with custom configuration.
    pub fn with_config(sample_rate: f32, config: VoiceConditionerConfig) -> Self {
        // Calculate EMA alpha from time constant: alpha = 1 - exp(-1 / (tau * sample_rate / frame_size))
        // Assuming ~2048 sample frames at given sample rate
        let frame_rate = sample_rate / 2048.0;
        let noise_floor_alpha = 1.0 - (-1.0 / (config.noise_floor_tau * frame_rate)).exp();

        // AGC alphas
        let agc_attack_alpha = 1.0 - (-1.0 / (config.agc_attack * frame_rate)).exp();
        let agc_release_alpha = 1.0 - (-1.0 / (config.agc_release * frame_rate)).exp();

        let gate_hold_samples = (config.gate_hold_time * frame_rate) as usize;

        Self {
            config,
            sample_rate,
            noise_floor_db: -60.0, // Start with quiet assumption
            noise_floor_alpha,
            gate_open: false,
            gate_hold_samples,
            gate_hold_counter: 0,
            agc_gain_db: 0.0,
            agc_attack_alpha,
            agc_release_alpha,
            reference_level_db: None,
            prev_zcr: 0.0,
            activated: false, // Requires strong signal to activate
        }
    }

    /// Reset state (call when starting new voice session).
    pub fn reset(&mut self) {
        self.gate_open = false;
        self.gate_hold_counter = 0;
        self.agc_gain_db = 0.0;
        self.reference_level_db = None;
        self.activated = false; // Require re-activation on new session
        // Keep noise floor estimate - it's useful across sessions
    }

    /// Process a frame of audio samples.
    pub fn process(&mut self, samples: &[f32]) -> ConditionerOutput {
        if samples.is_empty() {
            return ConditionerOutput {
                gate_open: false,
                samples: vec![],
                rms_db: -100.0,
                noise_floor_db: self.noise_floor_db,
                confidence_modifier: 1.0,
                reference_calibrated: self.reference_level_db.is_some(),
            };
        }

        // Calculate RMS energy
        let rms = self.calculate_rms(samples);
        let rms_db = self.linear_to_db(rms);

        // Calculate zero-crossing rate for VAD
        let zcr = self.calculate_zcr(samples);

        // VAD decision (energy + zero-crossing heuristic)
        let vad_voice = self.vad_decision(rms_db, zcr);

        // Update noise floor (only when gate is closed and low energy)
        if !self.gate_open && !vad_voice {
            self.noise_floor_db = self.noise_floor_db * (1.0 - self.noise_floor_alpha)
                + rms_db * self.noise_floor_alpha;
        }

        // Check for initial activation - requires strong signal to "wake up"
        if !self.activated {
            if rms_db > self.config.initial_activation_db && vad_voice {
                self.activated = true;
            } else {
                // Not activated yet - return early with gate closed
                return ConditionerOutput {
                    gate_open: false,
                    samples: vec![0.0; samples.len()],
                    rms_db,
                    noise_floor_db: self.noise_floor_db,
                    confidence_modifier: 1.0,
                    reference_calibrated: false,
                };
            }
        }

        // Gate decision with hysteresis
        let open_threshold = self.noise_floor_db + self.config.gate_open_db;
        let close_threshold = self.noise_floor_db + self.config.gate_close_db;

        let _prev_gate_open = self.gate_open;

        if !self.gate_open {
            // Gate is closed - check if we should open
            if rms_db > open_threshold && vad_voice {
                self.gate_open = true;
                self.gate_hold_counter = self.gate_hold_samples;
            }
        } else {
            // Gate is open - check if we should close
            if rms_db > close_threshold && vad_voice {
                // Still above close threshold - reset hold counter
                self.gate_hold_counter = self.gate_hold_samples;
            } else {
                // Below threshold - count down hold
                if self.gate_hold_counter > 0 {
                    self.gate_hold_counter -= 1;
                } else {
                    self.gate_open = false;
                }
            }
        }

        // Calculate confidence modifier from reference level
        let confidence_modifier = self.calculate_confidence_modifier(rms_db);

        // Apply AGC if gate is open
        let output_samples = if self.gate_open {
            self.apply_agc(samples, rms_db)
        } else {
            vec![0.0; samples.len()] // Gate closed - output silence
        };

        ConditionerOutput {
            gate_open: self.gate_open,
            samples: output_samples,
            rms_db,
            noise_floor_db: self.noise_floor_db,
            confidence_modifier,
            reference_calibrated: self.reference_level_db.is_some(),
        }
    }

    /// Set reference level from external confidence signal.
    /// Call this when pitch detector reports high confidence.
    pub fn calibrate_reference(&mut self, rms_db: f32, pitch_confidence: f64) {
        if pitch_confidence < 0.5 {
            return; // Not confident enough to calibrate
        }

        match self.reference_level_db {
            None => {
                // First calibration - set directly
                self.reference_level_db = Some(rms_db);
            }
            Some(ref_db) => {
                if rms_db > ref_db {
                    // Louder signal with high confidence - adapt upward
                    let new_ref = ref_db + (rms_db - ref_db) * self.config.ref_adapt_up_rate;
                    self.reference_level_db = Some(new_ref);
                }
                // Don't adapt downward from confident signals
            }
        }
    }

    /// Decay reference level slowly (call each frame).
    pub fn decay_reference(&mut self) {
        if let Some(ref_db) = self.reference_level_db {
            // Very slow decay toward noise floor
            let target = self.noise_floor_db + self.config.gate_open_db;
            if ref_db > target {
                let new_ref = ref_db - (ref_db - target) * self.config.ref_decay_down_rate;
                self.reference_level_db = Some(new_ref);
            }
        }
    }

    /// Calculate RMS energy of samples.
    fn calculate_rms(&self, samples: &[f32]) -> f32 {
        let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
        (sum_sq / samples.len() as f32).sqrt()
    }

    /// Calculate zero-crossing rate.
    fn calculate_zcr(&mut self, samples: &[f32]) -> f32 {
        let mut crossings = 0;
        for i in 1..samples.len() {
            if (samples[i] >= 0.0) != (samples[i - 1] >= 0.0) {
                crossings += 1;
            }
        }
        let zcr = crossings as f32 / samples.len() as f32;

        // Smooth ZCR
        let smoothed = self.prev_zcr * 0.7 + zcr * 0.3;
        self.prev_zcr = smoothed;
        smoothed
    }

    /// VAD decision based on energy and zero-crossing rate.
    fn vad_decision(&self, rms_db: f32, zcr: f32) -> bool {
        // Voice typically has ZCR between 0.02-0.15 per sample at 48kHz
        // (roughly 50-150 crossings per 10ms)
        let zcr_voice_like = zcr > 0.01 && zcr < 0.20;

        // Must be above noise floor
        let above_noise = rms_db > self.noise_floor_db + 3.0;

        above_noise && zcr_voice_like
    }

    /// Calculate confidence modifier based on reference level.
    fn calculate_confidence_modifier(&self, rms_db: f32) -> f32 {
        match self.reference_level_db {
            None => 1.0, // No reference yet - neutral
            Some(ref_db) => {
                let diff = ref_db - rms_db; // Positive if current is quieter

                if diff <= self.config.ref_boost_range_db {
                    // Within boost range (close to or louder than reference)
                    // Boost: 1.0 to 1.3
                    let boost = 1.0 + 0.3 * (1.0 - diff / self.config.ref_boost_range_db).max(0.0);
                    boost.min(1.3)
                } else if diff > self.config.ref_attenuate_threshold_db {
                    // Way below reference - attenuate
                    // Attenuate: 0.5 to 0.8
                    let excess = diff - self.config.ref_attenuate_threshold_db;
                    let attenuation = 0.8 - (excess / 12.0).min(0.3);
                    attenuation.max(0.5)
                } else {
                    // In between - slight attenuation
                    let t = (diff - self.config.ref_boost_range_db)
                        / (self.config.ref_attenuate_threshold_db - self.config.ref_boost_range_db);
                    1.0 - t * 0.2 // 1.0 down to 0.8
                }
            }
        }
    }

    /// Apply AGC to samples.
    fn apply_agc(&mut self, samples: &[f32], rms_db: f32) -> Vec<f32> {
        // Calculate desired gain
        let desired_gain_db = self.config.agc_target_dbfs - rms_db;

        // Smooth gain changes (fast attack, slow release)
        let alpha = if desired_gain_db < self.agc_gain_db {
            self.agc_attack_alpha // Reduce gain quickly
        } else {
            self.agc_release_alpha // Increase gain slowly
        };

        self.agc_gain_db = self.agc_gain_db * (1.0 - alpha) + desired_gain_db * alpha;

        // Limit gain range
        let gain_db = self.agc_gain_db.clamp(-20.0, 30.0);
        let gain_linear = self.db_to_linear(gain_db);

        // Apply gain
        samples.iter().map(|s| s * gain_linear).collect()
    }

    /// Convert linear amplitude to dB.
    fn linear_to_db(&self, linear: f32) -> f32 {
        if linear <= 1e-10 {
            -100.0
        } else {
            20.0 * linear.log10()
        }
    }

    /// Convert dB to linear amplitude.
    fn db_to_linear(&self, db: f32) -> f32 {
        10.0_f32.powf(db / 20.0)
    }

    /// Get current noise floor estimate.
    #[allow(dead_code)]
    pub fn noise_floor_db(&self) -> f32 {
        self.noise_floor_db
    }

    /// Get current reference level (if calibrated).
    #[allow(dead_code)]
    pub fn reference_level_db(&self) -> Option<f32> {
        self.reference_level_db
    }

    /// Check if gate is currently open.
    #[allow(dead_code)]
    pub fn is_gate_open(&self) -> bool {
        self.gate_open
    }
}
