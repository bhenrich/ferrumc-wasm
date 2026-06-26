//! Hex fixtures: parse a human-written hex string into bytes, render bytes back
//! to hex, and compare two byte runs with a readable diff.
//!
//! Protocol fixtures are stored and reviewed as hex (see `fixtures/` at the repo
//! root), so the harness needs a forgiving parser (whitespace between bytes is
//! ignored) and a comparison that pinpoints the first differing offset rather
//! than dumping two opaque blobs.

use std::fmt;

/// Why parsing a hex string failed.
///
/// Both variants carry enough context to point at the offending input. The enum
/// is `#[non_exhaustive]`: new failure modes may be added without a breaking
/// change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HexError {
    /// The input held an odd number of hex digits, so the final byte is
    /// incomplete. `digits` is the total digit count seen (whitespace excluded).
    #[error("hex string has an odd number of digits ({digits})")]
    OddDigits {
        /// Total non-whitespace hex digits parsed.
        digits: usize,
    },

    /// A character that is neither whitespace nor a hex digit was encountered.
    #[error("invalid hex digit {ch:?} at byte offset {offset}")]
    InvalidDigit {
        /// The offending character.
        ch: char,
        /// Its byte offset within the original input string.
        offset: usize,
    },
}

/// Parses a hex string into bytes, ignoring ASCII whitespace between digits.
///
/// Accepts upper- and lower-case digits and any whitespace layout (spaces,
/// tabs, newlines) so multi-line fixtures parse cleanly. Returns
/// [`HexError::InvalidDigit`] on a non-hex character and [`HexError::OddDigits`]
/// when the digit count is odd.
pub fn parse_hex(text: &str) -> Result<Vec<u8>, HexError> {
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut high: Option<u8> = None;
    let mut digits = 0usize;
    for (offset, ch) in text.char_indices() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        let Some(nibble) = ch.to_digit(16) else {
            return Err(HexError::InvalidDigit { ch, offset });
        };
        digits += 1;
        let nibble = nibble as u8;
        match high.take() {
            None => high = Some(nibble),
            Some(hi) => out.push((hi << 4) | nibble),
        }
    }
    if high.is_some() {
        return Err(HexError::OddDigits { digits });
    }
    Ok(out)
}

/// Renders bytes as a contiguous lower-case hex string with no separators.
pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` is infallible; the `Result` is discarded.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A readable difference between two byte runs.
///
/// Produced by [`hex_diff`] and [`HexFixture::diff`] only when the two runs are
/// not equal, so holding a `HexDiff` always means a real mismatch. Its
/// [`Display`](fmt::Display) renders both runs as hex plus the first differing
/// offset, which is what assertions surface on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexDiff {
    expected: Vec<u8>,
    actual: Vec<u8>,
    first_diff: Option<usize>,
}

impl HexDiff {
    /// The expected (reference) bytes.
    pub fn expected(&self) -> &[u8] {
        &self.expected
    }

    /// The actual (observed) bytes.
    pub fn actual(&self) -> &[u8] {
        &self.actual
    }

    /// Offset of the first byte that differs within the shared prefix, or
    /// `None` when the runs share a prefix but differ only in length.
    pub fn first_diff(&self) -> Option<usize> {
        self.first_diff
    }
}

impl fmt::Display for HexDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "byte mismatch:")?;
        writeln!(
            f,
            "  expected ({} bytes): {}",
            self.expected.len(),
            to_hex(&self.expected)
        )?;
        write!(
            f,
            "  actual   ({} bytes): {}",
            self.actual.len(),
            to_hex(&self.actual)
        )?;
        match self.first_diff {
            Some(offset) => write!(
                f,
                "\n  first difference at offset {offset}: expected {:#04x}, actual {:#04x}",
                self.expected[offset], self.actual[offset]
            ),
            None => write!(
                f,
                "\n  shared prefix matches; lengths differ ({} vs {})",
                self.expected.len(),
                self.actual.len()
            ),
        }
    }
}

impl std::error::Error for HexDiff {}

/// Compares two byte runs, returning `None` when equal or a [`HexDiff`]
/// otherwise.
///
/// By convention `expected` is the reference value and `actual` is what was
/// observed; the diff labels them accordingly.
pub fn hex_diff(expected: &[u8], actual: &[u8]) -> Option<HexDiff> {
    if expected == actual {
        return None;
    }
    let first_diff = expected.iter().zip(actual.iter()).position(|(a, b)| a != b);
    Some(HexDiff {
        expected: expected.to_vec(),
        actual: actual.to_vec(),
        first_diff,
    })
}

/// An owned run of bytes loaded from a hex string (or raw bytes) for use as a
/// test fixture.
///
/// Construct one with [`HexFixture::parse`] (from hex text) or
/// [`HexFixture::from_bytes`] (from bytes), then compare observed output against
/// it with [`HexFixture::diff`] / [`HexFixture::verify_eq`].
///
/// ```
/// use ferrumc_testkit::HexFixture;
///
/// let fixture = HexFixture::parse("00 ff 10").expect("valid hex");
/// assert_eq!(fixture.as_bytes(), &[0x00, 0xff, 0x10]);
/// assert!(fixture.verify_eq(&[0x00, 0xff, 0x10]).is_ok());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexFixture {
    bytes: Vec<u8>,
}

impl HexFixture {
    /// Builds a fixture from raw bytes.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    /// Parses a fixture from a hex string, ignoring ASCII whitespace.
    ///
    /// See [`parse_hex`] for the accepted format and error conditions.
    pub fn parse(text: &str) -> Result<Self, HexError> {
        Ok(Self {
            bytes: parse_hex(text)?,
        })
    }

    /// Borrows the fixture bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the fixture, returning its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Renders the fixture as a lower-case hex string (no separators).
    pub fn to_hex(&self) -> String {
        to_hex(&self.bytes)
    }

    /// The number of bytes in the fixture.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// `true` when the fixture holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Compares `actual` against this fixture, returning `None` when equal or a
    /// [`HexDiff`] otherwise.
    pub fn diff(&self, actual: &[u8]) -> Option<HexDiff> {
        hex_diff(&self.bytes, actual)
    }

    /// Returns `Ok(())` when `actual` matches this fixture, otherwise an `Err`
    /// carrying the [`HexDiff`]. Intended for `assert`-style use in tests via
    /// `.unwrap()` / `?` without panicking from this crate's own code.
    pub fn verify_eq(&self, actual: &[u8]) -> Result<(), HexDiff> {
        match self.diff(actual) {
            Some(diff) => Err(diff),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hex_diff, parse_hex, to_hex, HexError, HexFixture};

    #[test]
    fn parses_with_mixed_case_and_whitespace() {
        let bytes = parse_hex(" 00\tFf  10\n a B ").expect("valid hex");
        assert_eq!(bytes, vec![0x00, 0xFF, 0x10, 0xAB]);
    }

    #[test]
    fn parses_empty_input_to_no_bytes() {
        assert_eq!(parse_hex("   \n\t ").expect("empty"), Vec::<u8>::new());
    }

    #[test]
    fn rejects_odd_digit_count() {
        assert_eq!(parse_hex("abc"), Err(HexError::OddDigits { digits: 3 }));
    }

    #[test]
    fn rejects_non_hex_character_with_offset() {
        // 'g' sits at byte offset 2 of the input.
        assert_eq!(
            parse_hex("00g0"),
            Err(HexError::InvalidDigit { ch: 'g', offset: 2 })
        );
    }

    #[test]
    fn to_hex_round_trips_through_parse() {
        let bytes = [0x00u8, 0x01, 0x7f, 0x80, 0xff];
        let rendered = to_hex(&bytes);
        assert_eq!(rendered, "00017f80ff");
        assert_eq!(parse_hex(&rendered).expect("round-trip"), bytes);
    }

    #[test]
    fn diff_is_none_when_equal() {
        assert!(hex_diff(&[1, 2, 3], &[1, 2, 3]).is_none());
    }

    #[test]
    fn diff_reports_first_differing_offset() {
        let diff = hex_diff(&[1, 2, 3], &[1, 9, 3]).expect("differs");
        assert_eq!(diff.first_diff(), Some(1));
        assert!(diff.to_string().contains("offset 1"));
    }

    #[test]
    fn diff_reports_length_mismatch_with_shared_prefix() {
        let diff = hex_diff(&[1, 2], &[1, 2, 3]).expect("differs");
        assert_eq!(diff.first_diff(), None);
        assert!(diff.to_string().contains("lengths differ"));
    }

    #[test]
    fn fixture_verify_eq_ok_and_err() {
        let fixture = HexFixture::parse("0a0b").expect("valid");
        assert!(fixture.verify_eq(&[0x0a, 0x0b]).is_ok());
        assert!(fixture.verify_eq(&[0x0a, 0x0c]).is_err());
        assert_eq!(fixture.len(), 2);
        assert!(!fixture.is_empty());
        assert_eq!(fixture.to_hex(), "0a0b");
    }
}
