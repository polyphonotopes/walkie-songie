//! Tuning system for pitch class representation and quantization.
//!
//! Supports arbitrary tunings via SCL (Scala) files, with 12-TET as default.

mod scl;

use std::f64::consts::LOG2_E;

pub use scl::{parse_scl, SclParseError};

/// A pitch class index within a tuning system.
/// For 12-TET, this is 0-11 (C=0, C#=1, ..., B=11).
/// For other tunings, the range depends on the number of pitch classes per octave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PitchClass(pub u8);

impl PitchClass {
    /// Create a new pitch class with the given index.
    pub fn new(index: u8) -> Self {
        Self(index)
    }

    /// Get the pitch class index.
    pub fn index(&self) -> u8 {
        self.0
    }
}

/// Result of quantizing a frequency to a pitch class.
#[derive(Debug, Clone, Copy)]
pub struct QuantizeResult {
    /// The nearest pitch class
    pub pitch_class: PitchClass,
    /// Deviation from the pitch class center in cents (-50 to +50)
    pub cents_deviation: f64,
    /// The frequency of the pitch class center
    pub center_hz: f64,
    /// The absolute pitch (MIDI-style: octave * pitch_count + pc, where octave 4 = 48 for 12-TET)
    pub absolute_pitch: i32,
}

/// A tuning system defining pitch classes and their frequency ratios.
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Name of the tuning (e.g., "12-TET", or from SCL file)
    pub name: String,
    /// Reference frequency for pitch class 0 at octave 4 (e.g., 261.63 Hz for C4)
    pub reference_hz: f64,
    /// Cents offset for each pitch class from the octave start.
    /// For 12-TET: [0, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 1100]
    pub cents: Vec<f64>,
    /// Note names for each pitch class (optional, for display)
    pub note_names: Vec<String>,
}

impl Default for Tuning {
    fn default() -> Self {
        Self::twelve_tet()
    }
}

impl Tuning {
    /// Create the standard 12-tone equal temperament tuning.
    pub fn twelve_tet() -> Self {
        Self {
            name: "12-TET".to_string(),
            reference_hz: 261.6255653, // C4
            cents: (0..12).map(|i| i as f64 * 100.0).collect(),
            note_names: vec![
                "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }

    /// Create a tuning from parsed SCL data.
    pub fn from_scl(name: String, cents: Vec<f64>) -> Self {
        let num_pitches = cents.len();
        let note_names = (0..num_pitches).map(|i| format!("{}", i)).collect();
        Self {
            name,
            reference_hz: 261.6255653, // C4 as reference
            cents,
            note_names,
        }
    }

    /// Number of pitch classes in this tuning (per octave).
    pub fn pitch_class_count(&self) -> usize {
        self.cents.len()
    }

    /// Get the note name for a pitch class.
    pub fn note_name(&self, pc: PitchClass) -> &str {
        self.note_names
            .get(pc.0 as usize)
            .map(|s| s.as_str())
            .unwrap_or("?")
    }

    /// Get the note name with octave (e.g., "A4").
    pub fn note_name_with_octave(&self, pc: PitchClass, octave: i32) -> String {
        format!("{}{}", self.note_name(pc), octave)
    }

    /// Get the frequency in Hz for a pitch class at a given octave.
    pub fn hz_for_pitch(&self, pc: PitchClass, octave: i32) -> f64 {
        let octave_offset = octave - 4; // Reference is octave 4
        let cents_total = self.cents[pc.0 as usize] + (octave_offset as f64 * 1200.0);
        self.reference_hz * 2.0_f64.powf(cents_total / 1200.0)
    }

    /// Quantize a frequency to the nearest pitch class.
    /// Returns the pitch class, cents deviation, center frequency, and absolute pitch.
    pub fn quantize(&self, hz: f64) -> QuantizeResult {
        let pitch_count = self.pitch_class_count() as i32;

        if hz <= 0.0 {
            return QuantizeResult {
                pitch_class: PitchClass(0),
                cents_deviation: 0.0,
                center_hz: self.reference_hz,
                absolute_pitch: 5 * pitch_count, // C4 for 12-TET = 60
            };
        }

        // Convert Hz to cents relative to reference
        let cents_from_ref = 1200.0 * (hz / self.reference_hz).ln() * LOG2_E;

        // Find octave and position within octave
        let octave_cents = 1200.0;

        // Normalize to find octave
        let mut octave = (cents_from_ref / octave_cents).floor() as i32;
        let mut cents_in_octave = cents_from_ref - (octave as f64 * octave_cents);

        // Handle negative values
        if cents_in_octave < 0.0 {
            cents_in_octave += octave_cents;
            octave -= 1;
        }

        // Find nearest pitch class
        let mut best_pc = 0;
        let mut best_deviation = f64::MAX;

        for (i, &pc_cents) in self.cents.iter().enumerate() {
            let deviation = cents_in_octave - pc_cents;

            if deviation.abs() < best_deviation.abs() {
                best_deviation = deviation;
                best_pc = i;
            }

            // Check if closer to this note in next octave
            let deviation_next = cents_in_octave - (pc_cents + octave_cents);
            if deviation_next.abs() < best_deviation.abs() {
                best_deviation = deviation_next;
                best_pc = i;
            }
        }

        // Clamp deviation to [-50, 50] cents (half step)
        let clamped_deviation = best_deviation.clamp(-50.0, 50.0);

        let pitch_class = PitchClass(best_pc as u8);
        let actual_octave = octave + 4; // octave is relative to reference (octave 4)
        let center_hz = self.hz_for_pitch(pitch_class, actual_octave);

        // Compute absolute pitch (MIDI-style: C4 = 60 for 12-TET)
        // Formula: (octave + 1) * pitch_count + pitch_class_index
        let absolute_pitch = (actual_octave + 1) * pitch_count + best_pc as i32;

        QuantizeResult {
            pitch_class,
            cents_deviation: clamped_deviation,
            center_hz,
            absolute_pitch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_twelve_tet_basics() {
        let tuning = Tuning::twelve_tet();
        assert_eq!(tuning.pitch_class_count(), 12);
        assert_eq!(tuning.note_name(PitchClass(0)), "C");
        assert_eq!(tuning.note_name(PitchClass(9)), "A");
    }

    #[test]
    fn test_hz_for_pitch() {
        let tuning = Tuning::twelve_tet();
        // A4 should be ~440 Hz
        let a4_hz = tuning.hz_for_pitch(PitchClass(9), 4);
        assert!((a4_hz - 440.0).abs() < 0.1);

        // C4 should be reference
        let c4_hz = tuning.hz_for_pitch(PitchClass(0), 4);
        assert!((c4_hz - 261.63).abs() < 0.1);
    }

    #[test]
    fn test_quantize_exact() {
        let tuning = Tuning::twelve_tet();

        // Exact A4 = 440 Hz
        let result = tuning.quantize(440.0);
        assert_eq!(result.pitch_class.0, 9); // A
        assert!(result.cents_deviation.abs() < 1.0);
    }

    #[test]
    fn test_quantize_sharp() {
        let tuning = Tuning::twelve_tet();

        // Slightly sharp A4
        let result = tuning.quantize(450.0);
        assert_eq!(result.pitch_class.0, 9); // Still A
        assert!(result.cents_deviation > 0.0); // Sharp = positive
    }

    #[test]
    fn test_quantize_flat() {
        let tuning = Tuning::twelve_tet();

        // Slightly flat A4
        let result = tuning.quantize(430.0);
        assert_eq!(result.pitch_class.0, 9); // Still A
        assert!(result.cents_deviation < 0.0); // Flat = negative
    }
}
