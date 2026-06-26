//! [`PacketScript`]: an ordered, directional record of wire bytes with a
//! record/replay API and a simple text transcript format.
//!
//! A script captures a flow as a sequence of [`ScriptEntry`] values, each a
//! ([`Direction`], bytes) pair. Scripts can be serialized to a line-oriented
//! transcript (one entry per line) and parsed back, so a failing exchange can be
//! captured to a file and replayed deterministically later. [`ScriptedClient`]
//! (see [`crate::ScriptedClient`]) records its traffic into one of these.

use ferrumc_proto::Direction;

use crate::hex::{hex_diff, parse_hex, to_hex, HexDiff, HexError};

/// One entry in a [`PacketScript`]: a direction paired with the wire bytes that
/// travelled that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptEntry {
    direction: Direction,
    bytes: Vec<u8>,
}

impl ScriptEntry {
    /// Builds an entry from a direction and its bytes.
    pub fn new(direction: Direction, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            direction,
            bytes: bytes.into(),
        }
    }

    /// Builds a [`Direction::Serverbound`] entry (client to server).
    pub fn serverbound(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(Direction::Serverbound, bytes)
    }

    /// Builds a [`Direction::Clientbound`] entry (server to client).
    pub fn clientbound(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(Direction::Clientbound, bytes)
    }

    /// The direction this entry travelled.
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The wire bytes of this entry.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Why a transcript string could not be parsed back into a [`PacketScript`].
///
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TranscriptError {
    /// A line began with a direction marker other than `S` or `C`.
    #[error("line {line}: unknown direction marker {marker:?} (expected 'S' or 'C')")]
    UnknownDirection {
        /// The 1-based line number.
        line: usize,
        /// The offending marker character.
        marker: char,
    },

    /// A line's hex payload failed to parse.
    #[error("line {line}: {source}")]
    Hex {
        /// The 1-based line number.
        line: usize,
        /// The underlying hex parse error.
        #[source]
        source: HexError,
    },
}

/// Why two [`PacketScript`]s were not equal.
///
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScriptMismatch {
    /// The two scripts held a different number of entries.
    #[error("entry count differs: expected {expected}, got {actual}")]
    Length {
        /// Entry count of the expected script.
        expected: usize,
        /// Entry count of the actual script.
        actual: usize,
    },

    /// Entry `index` travelled in different directions.
    #[error("entry {index}: direction differs (expected {expected:?}, got {actual:?})")]
    Direction {
        /// Index of the differing entry.
        index: usize,
        /// Direction in the expected script.
        expected: Direction,
        /// Direction in the actual script.
        actual: Direction,
    },

    /// Entry `index` carried different bytes.
    #[error("entry {index}: {diff}")]
    Bytes {
        /// Index of the differing entry.
        index: usize,
        /// The byte-level difference.
        diff: HexDiff,
    },
}

/// An ordered, directional record of wire bytes.
///
/// Append entries with [`record`](Self::record) /
/// [`record_serverbound`](Self::record_serverbound) /
/// [`record_clientbound`](Self::record_clientbound), step through them with
/// [`replay`](Self::replay), serialize with [`to_transcript`](Self::to_transcript)
/// and parse back with [`from_transcript`](Self::from_transcript).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PacketScript {
    entries: Vec<ScriptEntry>,
}

impl PacketScript {
    /// Creates an empty script.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry.
    pub fn record(&mut self, entry: ScriptEntry) {
        self.entries.push(entry);
    }

    /// Appends a [`Direction::Serverbound`] entry (client to server).
    pub fn record_serverbound(&mut self, bytes: impl Into<Vec<u8>>) {
        self.entries.push(ScriptEntry::serverbound(bytes));
    }

    /// Appends a [`Direction::Clientbound`] entry (server to client).
    pub fn record_clientbound(&mut self, bytes: impl Into<Vec<u8>>) {
        self.entries.push(ScriptEntry::clientbound(bytes));
    }

    /// Borrows the recorded entries in order.
    pub fn entries(&self) -> &[ScriptEntry] {
        &self.entries
    }

    /// The number of recorded entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no entries have been recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns a cursor that yields the entries in recorded order.
    pub fn replay(&self) -> Replay<'_> {
        Replay {
            entries: &self.entries,
            pos: 0,
        }
    }

    /// Serializes the script to a line-oriented transcript.
    ///
    /// Each entry becomes one line: `S <hex>` for serverbound or `C <hex>` for
    /// clientbound, where `<hex>` is the lower-case byte rendering (empty for a
    /// zero-byte entry). [`from_transcript`](Self::from_transcript) parses the
    /// result back to an equal script.
    pub fn to_transcript(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            let marker = match entry.direction {
                Direction::Serverbound => 'S',
                Direction::Clientbound => 'C',
            };
            out.push(marker);
            out.push(' ');
            out.push_str(&to_hex(&entry.bytes));
            out.push('\n');
        }
        out
    }

    /// Parses a transcript produced by [`to_transcript`](Self::to_transcript).
    ///
    /// Blank lines and lines whose first non-whitespace character is `#` are
    /// ignored, so transcripts may be commented. The direction marker is `S`/`s`
    /// for serverbound or `C`/`c` for clientbound; the remainder of the line is
    /// hex (whitespace within it is ignored).
    pub fn from_transcript(text: &str) -> Result<Self, TranscriptError> {
        let mut entries = Vec::new();
        for (idx, raw) in text.lines().enumerate() {
            let line = idx + 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut chars = trimmed.chars();
            let Some(marker) = chars.next() else {
                continue;
            };
            let direction = match marker {
                'S' | 's' => Direction::Serverbound,
                'C' | 'c' => Direction::Clientbound,
                other => {
                    return Err(TranscriptError::UnknownDirection {
                        line,
                        marker: other,
                    })
                }
            };
            let bytes = parse_hex(chars.as_str())
                .map_err(|source| TranscriptError::Hex { line, source })?;
            entries.push(ScriptEntry { direction, bytes });
        }
        Ok(Self { entries })
    }

    /// Compares this script (the actual) against `expected`, returning `Ok(())`
    /// when they match or a [`ScriptMismatch`] describing the first divergence.
    pub fn verify_eq(&self, expected: &PacketScript) -> Result<(), ScriptMismatch> {
        if self.entries.len() != expected.entries.len() {
            return Err(ScriptMismatch::Length {
                expected: expected.entries.len(),
                actual: self.entries.len(),
            });
        }
        for (index, (actual, want)) in self.entries.iter().zip(&expected.entries).enumerate() {
            if actual.direction != want.direction {
                return Err(ScriptMismatch::Direction {
                    index,
                    expected: want.direction,
                    actual: actual.direction,
                });
            }
            if let Some(diff) = hex_diff(&want.bytes, &actual.bytes) {
                return Err(ScriptMismatch::Bytes { index, diff });
            }
        }
        Ok(())
    }
}

/// A forward cursor over a [`PacketScript`]'s entries, yielded by
/// [`PacketScript::replay`].
///
/// Implements [`Iterator`], so entries can be stepped through or collected.
pub struct Replay<'a> {
    entries: &'a [ScriptEntry],
    pos: usize,
}

impl Replay<'_> {
    /// The number of entries not yet yielded.
    pub fn remaining(&self) -> usize {
        self.entries.len() - self.pos
    }

    /// `true` when every entry has been yielded.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
}

impl<'a> Iterator for Replay<'a> {
    type Item = &'a ScriptEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.entries.get(self.pos)?;
        self.pos += 1;
        Some(entry)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining();
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Replay<'_> {}

#[cfg(test)]
mod tests {
    use ferrumc_proto::Direction;

    use super::{PacketScript, ScriptEntry, ScriptMismatch, TranscriptError};

    fn sample() -> PacketScript {
        let mut script = PacketScript::new();
        script.record_serverbound(vec![0x00, 0x01, 0x02]);
        script.record_clientbound(vec![0xff]);
        script.record(ScriptEntry::serverbound(Vec::new())); // empty body
        script
    }

    #[test]
    fn record_then_replay_yields_recorded_entries() {
        let script = sample();
        let replayed: Vec<&ScriptEntry> = script.replay().collect();
        assert_eq!(replayed.len(), 3);
        assert_eq!(replayed[0].direction(), Direction::Serverbound);
        assert_eq!(replayed[0].bytes(), &[0x00, 0x01, 0x02]);
        assert_eq!(replayed[1].direction(), Direction::Clientbound);
        assert_eq!(replayed[1].bytes(), &[0xff]);
        assert_eq!(replayed[2].bytes(), &[] as &[u8]);
    }

    #[test]
    fn replay_remaining_counts_down() {
        let script = sample();
        let mut replay = script.replay();
        assert_eq!(replay.remaining(), 3);
        let _ = replay.next();
        assert_eq!(replay.remaining(), 2);
        let _ = replay.by_ref().count();
        assert!(replay.is_empty());
    }

    #[test]
    fn transcript_round_trips_to_equal_script() {
        let script = sample();
        let text = script.to_transcript();
        let parsed = PacketScript::from_transcript(&text).expect("parse");
        assert_eq!(parsed, script);
    }

    #[test]
    fn transcript_ignores_blank_and_comment_lines() {
        let text = "# a comment\n\nS 0001\n   # indented comment\nC ff\n";
        let parsed = PacketScript::from_transcript(text).expect("parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.entries()[0].bytes(), &[0x00, 0x01]);
        assert_eq!(parsed.entries()[1].direction(), Direction::Clientbound);
    }

    #[test]
    fn transcript_rejects_unknown_direction() {
        let err = PacketScript::from_transcript("S 00\nX ff\n").expect_err("bad marker");
        assert_eq!(
            err,
            TranscriptError::UnknownDirection {
                line: 2,
                marker: 'X'
            }
        );
    }

    #[test]
    fn transcript_rejects_bad_hex_with_line_number() {
        let err = PacketScript::from_transcript("S 0g\n").expect_err("bad hex");
        assert!(matches!(err, TranscriptError::Hex { line: 1, .. }));
    }

    #[test]
    fn verify_eq_detects_length_direction_and_byte_diffs() {
        let base = sample();
        assert!(base.verify_eq(&sample()).is_ok());

        let mut shorter = PacketScript::new();
        shorter.record_serverbound(vec![0x00, 0x01, 0x02]);
        assert!(matches!(
            shorter.verify_eq(&base),
            Err(ScriptMismatch::Length { .. })
        ));

        let mut wrong_dir = PacketScript::new();
        wrong_dir.record_clientbound(vec![0x00, 0x01, 0x02]);
        wrong_dir.record_clientbound(vec![0xff]);
        wrong_dir.record_serverbound(Vec::new());
        assert!(matches!(
            wrong_dir.verify_eq(&base),
            Err(ScriptMismatch::Direction { index: 0, .. })
        ));

        let mut wrong_bytes = PacketScript::new();
        wrong_bytes.record_serverbound(vec![0x00, 0x01, 0x03]);
        wrong_bytes.record_clientbound(vec![0xff]);
        wrong_bytes.record_serverbound(Vec::new());
        assert!(matches!(
            wrong_bytes.verify_eq(&base),
            Err(ScriptMismatch::Bytes { index: 0, .. })
        ));
    }
}
