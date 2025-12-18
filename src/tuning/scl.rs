//! Scala (.scl) tuning file parser.
//!
//! SCL format specification: <https://www.huygens-fokker.org/scala/scl_format.html>
//!
//! A minimal implementation supporting the core format:
//! - Comment lines starting with !
//! - Description line
//! - Number of notes
//! - Pitch values (cents or ratios)

use thiserror::Error;

/// Errors that can occur when parsing SCL files.
#[derive(Debug, Error)]
pub enum SclParseError {
    #[error("Empty SCL content")]
    Empty,
    #[error("Missing note count")]
    MissingNoteCount,
    #[error("Invalid note count: {0}")]
    InvalidNoteCount(String),
    #[error("Not enough pitch values (expected {expected}, got {got})")]
    NotEnoughPitches { expected: usize, got: usize },
    #[error("Invalid pitch value: {0}")]
    InvalidPitch(String),
}

/// Parse SCL content into a vector of cents values.
///
/// Returns cents offsets for each pitch class, starting from 0.
/// The first pitch class is always 0 cents (the root).
pub fn parse_scl(content: &str) -> Result<Vec<f64>, SclParseError> {
    let mut lines = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.starts_with('!') && !l.is_empty());

    // First non-comment line is the description (we skip it)
    let _description = lines.next().ok_or(SclParseError::Empty)?;

    // Second non-comment line is the note count
    let count_line = lines.next().ok_or(SclParseError::MissingNoteCount)?;
    let note_count: usize = count_line
        .parse()
        .map_err(|_| SclParseError::InvalidNoteCount(count_line.to_string()))?;

    if note_count == 0 {
        // Edge case: 0 notes means just the root
        return Ok(vec![0.0]);
    }

    // Remaining lines are pitch values
    let mut cents = vec![0.0]; // Start with root at 0 cents

    for line in lines.take(note_count) {
        let cents_value = parse_pitch_value(line)?;
        cents.push(cents_value);
    }

    // Remove the octave (last value should be ~1200 cents for standard scales)
    // The cents vector represents pitch classes within one octave
    if cents.len() > 1 {
        cents.pop(); // Remove the octave
    }

    if cents.is_empty() {
        cents.push(0.0);
    }

    Ok(cents)
}

/// Parse a single pitch value (either cents or ratio).
fn parse_pitch_value(s: &str) -> Result<f64, SclParseError> {
    let s = s.trim();

    // Check if it's a ratio (contains /)
    if s.contains('/') {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() != 2 {
            return Err(SclParseError::InvalidPitch(s.to_string()));
        }
        let num: f64 = parts[0]
            .trim()
            .parse()
            .map_err(|_| SclParseError::InvalidPitch(s.to_string()))?;
        let den: f64 = parts[1]
            .trim()
            .parse()
            .map_err(|_| SclParseError::InvalidPitch(s.to_string()))?;
        if den == 0.0 {
            return Err(SclParseError::InvalidPitch(s.to_string()));
        }
        // Convert ratio to cents: 1200 * log2(ratio)
        Ok(1200.0 * (num / den).log2())
    } else if s.contains('.') {
        // It's a cents value
        s.parse()
            .map_err(|_| SclParseError::InvalidPitch(s.to_string()))
    } else {
        // Integer - could be cents or a whole number ratio
        let val: f64 = s
            .parse()
            .map_err(|_| SclParseError::InvalidPitch(s.to_string()))?;
        // If it's a small integer, treat as ratio; otherwise as cents
        // SCL convention: values without decimals < 5 are usually ratios
        if val < 5.0 && val > 0.0 {
            Ok(1200.0 * val.log2())
        } else {
            Ok(val)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_12tet() {
        let scl = r#"! 12-TET
12-tone equal temperament
12
100.0
200.0
300.0
400.0
500.0
600.0
700.0
800.0
900.0
1000.0
1100.0
1200.0
"#;
        let cents = parse_scl(scl).unwrap();
        assert_eq!(cents.len(), 12);
        assert!((cents[0] - 0.0).abs() < 0.01);
        assert!((cents[1] - 100.0).abs() < 0.01);
        assert!((cents[11] - 1100.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_ratio() {
        let scl = r#"! Just intonation major scale
Just major
7
9/8
5/4
4/3
3/2
5/3
15/8
2/1
"#;
        let cents = parse_scl(scl).unwrap();
        assert_eq!(cents.len(), 7);
        // 9/8 ≈ 203.91 cents
        assert!((cents[1] - 203.91).abs() < 0.1);
        // 3/2 ≈ 701.96 cents
        assert!((cents[4] - 701.96).abs() < 0.1);
    }

    #[test]
    fn test_parse_with_comments() {
        let scl = r#"! This is a comment
! Another comment
Test scale
3
! Inline comments not supported but leading ! works
400.0
700.0
1200.0
"#;
        let cents = parse_scl(scl).unwrap();
        assert_eq!(cents.len(), 3);
    }

    #[test]
    fn test_empty_error() {
        let result = parse_scl("");
        assert!(result.is_err());
    }
}
