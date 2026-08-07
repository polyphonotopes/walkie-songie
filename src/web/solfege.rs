//! Solfege detection for determining possible mode names of notes.
//!
//! Uses the traditional mode detection: fa=lydian, so=mixolydian, la=aeolian,
//! ti=locrian, do=ionian, re=dorian, mi=phrygian.

/// Mode names in order of degree (starting from ionian on degree 0)
const MODE_NAMES: [&str; 7] = [
    "ionian",     // 0 - do
    "dorian",     // 1 - re
    "phrygian",   // 2 - mi
    "lydian",     // 3 - fa
    "mixolydian", // 4 - so
    "aeolian",    // 5 - la
    "locrian",    // 6 - ti
];

/// Solfege syllables in order of degree
const SOLFEGE: [&str; 7] = ["do", "re", "mi", "fa", "so", "la", "ti"];

/// Major scale intervals: W W H W W W H (2 2 1 2 2 2 1)
/// As pitch classes from C: 0, 2, 4, 5, 7, 9, 11
const MAJOR_INTERVALS: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];

/// Sharp note names (for ascending/sharp contexts)
const SHARP_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// Flat note names (for descending/flat contexts)
const FLAT_NAMES: [&str; 12] = [
    "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
];

/// Unicode symbols for display
pub const BASS_CLEF: char = '\u{1D122}'; // 𝄢
pub const TREBLE_CLEF: char = '\u{1D11E}'; // 𝄞

/// Source of a note (for display purposes)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteSource {
    Voice,         // From voice input (shows 🗣️)
    Piece(String), // From a piece with its emoji
    Toggle,        // From toggle mode (no emoji)
}

impl NoteSource {
    /// Get the emoji to display for this source
    pub fn emoji(&self) -> String {
        match self {
            NoteSource::Voice => "🗣️".to_string(),
            NoteSource::Piece(emoji) => emoji.clone(),
            NoteSource::Toggle => String::new(),
        }
    }
}

/// Result of solfege detection for a specific note
#[derive(Debug, Clone)]
pub struct SolfegeResult {
    /// The note name in sharp notation (e.g., "C#4")
    pub sharp_name: String,
    /// The note name in flat notation (e.g., "Db4")
    pub flat_name: String,
    /// Possible solfege syllables this note could be (based on all matching major scales)
    pub solfeges: Vec<String>,
    /// The source of this note
    pub source: NoteSource,
    /// The absolute pitch (MIDI note number)
    pub pitch: i32,
}

impl SolfegeResult {
    /// Get the preferred note name (uses flat for b-flat notes, sharp otherwise)
    pub fn preferred_name(&self) -> &str {
        let pc = self.pitch.rem_euclid(12) as usize;
        // Use flat for Bb, Eb, Ab, Db, Gb
        if matches!(pc, 1 | 3 | 6 | 8 | 10) {
            &self.flat_name
        } else {
            &self.sharp_name
        }
    }
}

/// Detect possible solfege syllables for a note given the active pitch classes.
/// Returns all possible solfege values this note could be (e.g., ["do", "fa"] if it could be tonic or subdominant)
pub fn detect_solfege_for_note(
    note_pitch: i32,
    active_pitch_classes: &[u8],
    note_source: NoteSource,
) -> SolfegeResult {
    let note_pc = note_pitch.rem_euclid(12) as u8;
    let octave = (note_pitch / 12) - 1; // MIDI octave convention

    let sharp_name = format!("{}{}", SHARP_NAMES[note_pc as usize], octave);
    let flat_name = format!("{}{}", FLAT_NAMES[note_pc as usize], octave);

    let mut solfeges = Vec::new();

    // Create bitset of active pitch classes for subset checking
    let active_set: std::collections::HashSet<u8> = active_pitch_classes.iter().copied().collect();

    // Try each possible major scale (12 tonics)
    for parent_tonic in 0u8..12 {
        // Build the major scale from this tonic
        let scale_pcs: std::collections::HashSet<u8> = MAJOR_INTERVALS
            .iter()
            .map(|&interval| (parent_tonic + interval) % 12)
            .collect();

        // Check if all active notes are a subset of this major scale
        if active_set.is_subset(&scale_pcs) {
            // Find which degree the note is in this scale
            for (degree, &interval) in MAJOR_INTERVALS.iter().enumerate() {
                let scale_note = (parent_tonic + interval) % 12;
                if scale_note == note_pc {
                    let solfege = SOLFEGE[degree].to_string();
                    if !solfeges.contains(&solfege) {
                        solfeges.push(solfege);
                    }
                }
            }
        }
    }

    // Sort for consistent display
    solfeges.sort();

    SolfegeResult {
        sharp_name,
        flat_name,
        solfeges,
        source: note_source,
        pitch: note_pitch,
    }
}

/// Info about the bass (lowest) and treble (highest) notes
#[derive(Debug, Clone)]
pub struct RangeInfo {
    pub bass: Option<SolfegeResult>,
    pub treble: Option<SolfegeResult>,
}

/// Analyze the range of active notes
pub fn analyze_range(
    active_notes: &[(i32, NoteSource)], // (absolute_pitch, source)
    active_pitch_classes: &[u8],
) -> RangeInfo {
    if active_notes.is_empty() {
        return RangeInfo {
            bass: None,
            treble: None,
        };
    }

    // Find lowest and highest
    let (lowest_pitch, lowest_source) = active_notes
        .iter()
        .min_by_key(|(p, _)| p)
        .map(|(p, s)| (*p, s.clone()))
        .unwrap();

    let (highest_pitch, highest_source) = active_notes
        .iter()
        .max_by_key(|(p, _)| p)
        .map(|(p, s)| (*p, s.clone()))
        .unwrap();

    let bass = detect_solfege_for_note(lowest_pitch, active_pitch_classes, lowest_source);
    let treble = detect_solfege_for_note(highest_pitch, active_pitch_classes, highest_source);

    RangeInfo {
        bass: Some(bass),
        treble: Some(treble),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_solfege_c_major_triad() {
        // C4 = MIDI 60, E4 = 64, G4 = 67
        // Active pitch classes: 0, 4, 7 (C, E, G)
        let active_pcs = vec![0, 4, 7];

        let c_result = detect_solfege_for_note(60, &active_pcs, NoteSource::Toggle);
        assert!(c_result.solfeges.contains(&"do".to_string())); // C is do in C major

        let e_result = detect_solfege_for_note(64, &active_pcs, NoteSource::Toggle);
        assert!(e_result.solfeges.contains(&"mi".to_string())); // E is mi in C major
    }
}
