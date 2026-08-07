//! Scale and chord matching for the info panel.
//!
//! Provides pitch class set matching against common scales and chords.

use std::collections::HashSet;

// ============ Enharmonic Naming ============

/// Note names using sharps
const SHARP_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// Note names using flats
const FLAT_NAMES: [&str; 12] = [
    "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
];

/// Get the appropriate note name for a scale root based on the scale type.
/// Uses music theory conventions for key signatures.
fn enharmonic_root_name(root: u8, scale_type: &str) -> &'static str {
    // Determine if this scale type typically uses flats or sharps
    // Based on circle of fifths conventions
    let use_flats = match root {
        // Natural notes - context dependent
        0 => false,  // C - sharps (or neutral)
        2 => false,  // D - sharps
        4 => false,  // E - sharps
        5 => true,   // F - flats (Bb in key signature)
        7 => false,  // G - sharps
        9 => false,  // A - sharps
        11 => false, // B - sharps
        // Accidentals - prefer flats for flat keys, sharps for sharp keys
        1 => {
            // C#/Db - Db for most scales, C# for some
            matches!(
                scale_type,
                "major" | "minor" | "dorian" | "mixolydian" | "lydian"
            )
        }
        3 => true, // D#/Eb - always Eb
        6 => {
            // F#/Gb - Gb for Gb major, F# for others
            scale_type == "major"
        }
        8 => true,  // G#/Ab - always Ab
        10 => true, // A#/Bb - always Bb
        _ => false,
    };

    if use_flats {
        FLAT_NAMES[root as usize]
    } else {
        SHARP_NAMES[root as usize]
    }
}

// ============ Scale & Chord Database ============

/// Rotate a 12-bit scale bitset by n semitones
fn rotate_bits(bits: u16, n: u8) -> u16 {
    let n = n % 12;
    let mask = 0xFFF; // 12 bits
    ((bits << n) | (bits >> (12 - n))) & mask
}

/// Normalize a PCS bitset to its canonical (lowest) rotation.
/// This lets us identify when two scales are modes of each other.
fn normalize_pcs(bits: u16) -> u16 {
    let mut min = bits;
    let mut rotated = bits;
    for _ in 1..12 {
        rotated = rotate_bits(rotated, 1);
        if rotated < min {
            min = rotated;
        }
    }
    min
}

/// A scale or chord definition for matching
struct PcsDef {
    /// Bitset at C (root = 0)
    bits: u16,
    /// Display name (without root)
    name: &'static str,
    /// Category for styling
    category: &'static str,
    /// Priority for deduplication (lower = preferred)
    priority: u8,
}

/// All scales and chords we want to match against.
/// Only one representative per unique PCS shape is included.
const PCS_DEFS: &[PcsDef] = &[
    // ============ Scales ============
    // Diatonic (only major - modes are same PCS)
    PcsDef {
        bits: 0b101010110101,
        name: "major",
        category: "diatonic",
        priority: 1,
    },
    // Harmonic minor (distinct PCS from major)
    PcsDef {
        bits: 0b100110101101,
        name: "harmonic minor",
        category: "minor",
        priority: 2,
    },
    // Altered/jazz (same PCS as melodic minor, prefer "altered" name)
    PcsDef {
        bits: 0b101010101101,
        name: "altered",
        category: "altered",
        priority: 3,
    },
    // Pentatonics (major pent = relative minor pent, same PCS)
    PcsDef {
        bits: 0b001010010101,
        name: "major pentatonic",
        category: "pentatonic",
        priority: 4,
    },
    // Symmetric/exotic
    PcsDef {
        bits: 0b010101010101,
        name: "whole tone",
        category: "symmetric",
        priority: 5,
    },
    PcsDef {
        bits: 0b011011011011,
        name: "diminished",
        category: "symmetric",
        priority: 5,
    },
    // Enigmatic
    PcsDef {
        bits: 0b110101010011,
        name: "enigmatic",
        category: "exotic",
        priority: 6,
    },
    // Blues
    PcsDef {
        bits: 0b010011101001,
        name: "blues",
        category: "blues",
        priority: 4,
    },
    // ============ Chords - Triads ============
    PcsDef {
        bits: 0b000010010001,
        name: "major",
        category: "chord",
        priority: 10,
    }, // C E G = 0,4,7
    PcsDef {
        bits: 0b000010001001,
        name: "minor",
        category: "chord",
        priority: 10,
    }, // C Eb G = 0,3,7
    PcsDef {
        bits: 0b000001001001,
        name: "dim",
        category: "chord",
        priority: 11,
    }, // C Eb Gb = 0,3,6
    PcsDef {
        bits: 0b000100010001,
        name: "aug",
        category: "chord",
        priority: 11,
    }, // C E G# = 0,4,8
    PcsDef {
        bits: 0b000010100001,
        name: "sus4",
        category: "chord",
        priority: 12,
    }, // C F G = 0,5,7
    PcsDef {
        bits: 0b000010000101,
        name: "sus2",
        category: "chord",
        priority: 12,
    }, // C D G = 0,2,7
    // ============ Chords - 7ths ============
    PcsDef {
        bits: 0b100010010001,
        name: "maj7",
        category: "chord",
        priority: 10,
    }, // C E G B = 0,4,7,11
    PcsDef {
        bits: 0b010010010001,
        name: "7",
        category: "chord",
        priority: 10,
    }, // C E G Bb = 0,4,7,10
    PcsDef {
        bits: 0b010010001001,
        name: "m7",
        category: "chord",
        priority: 10,
    }, // C Eb G Bb = 0,3,7,10
    PcsDef {
        bits: 0b010001001001,
        name: "m7b5",
        category: "chord",
        priority: 11,
    }, // C Eb Gb Bb = 0,3,6,10
    PcsDef {
        bits: 0b001001001001,
        name: "dim7",
        category: "chord",
        priority: 11,
    }, // C Eb Gb Bbb = 0,3,6,9
    PcsDef {
        bits: 0b100010001001,
        name: "mMaj7",
        category: "chord",
        priority: 11,
    }, // C Eb G B = 0,3,7,11
    PcsDef {
        bits: 0b010010010101,
        name: "9",
        category: "chord",
        priority: 13,
    }, // C D E G Bb = 0,2,4,7,10
];

/// Find all scales/chords that contain the given pitch classes.
/// Returns (formatted_name, category, is_exact) tuples with proper enharmonic naming.
/// Deduplicates by unique (normalized_pcs, root) to avoid showing modes.
/// `is_exact` is true when the active pitches exactly match the PCS (not just a subset).
pub fn find_matching_scale_names(active_pitches: &[u8]) -> Vec<(String, String, bool)> {
    if active_pitches.is_empty() {
        return Vec::new();
    }

    // Convert active pitches to a bitset
    let mut active_bits: u16 = 0;
    for &pc in active_pitches {
        active_bits |= 1 << pc;
    }

    // Collect all matches with their priority and exactness
    // (name, category, priority, normalized_pcs, root, is_exact)
    let mut matches: Vec<(String, String, u8, u16, u8, bool)> = Vec::new();

    // Check each PCS definition at each of the 12 rotations
    for pcs_def in PCS_DEFS {
        for root in 0u8..12 {
            // Rotate the PCS to this root
            let rotated = rotate_bits(pcs_def.bits, root);

            // Check if active pitches are a subset of this PCS
            if (active_bits & rotated) == active_bits {
                let root_name = enharmonic_root_name(root, pcs_def.name);
                let full_name = format!("{} {}", root_name, pcs_def.name);
                let normalized = normalize_pcs(rotated);
                let is_exact = active_bits == rotated;
                matches.push((
                    full_name,
                    pcs_def.category.to_string(),
                    pcs_def.priority,
                    normalized,
                    root,
                    is_exact,
                ));
            }
        }
    }

    // Sort by: exact matches first, then priority (lower first), then by root (C first)
    matches.sort_by(|a, b| {
        b.5.cmp(&a.5) // exact first (true > false, so reverse)
            .then(a.2.cmp(&b.2))
            .then(a.4.cmp(&b.4))
    });

    // Deduplicate by (normalized_pcs, root) - keep first (highest priority)
    let mut seen: HashSet<(u16, u8)> = HashSet::new();
    let mut results: Vec<(String, String, bool)> = Vec::new();

    for (name, category, _priority, normalized, root, is_exact) in matches {
        let key = (normalized, root);
        if !seen.contains(&key) {
            seen.insert(key);
            results.push((name, category, is_exact));
        }
    }

    results
}
