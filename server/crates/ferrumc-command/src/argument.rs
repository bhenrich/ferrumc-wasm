//! Argument kinds, parsed argument values, and the parsed-argument map.

/// The kind of value an argument node expects, and how to parse it.
///
/// Used both for parsing (consuming raw input into an [`ArgumentValue`]) and for
/// generating suggestion hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentType {
    /// A single whitespace-delimited word, taken verbatim.
    Word,
    /// All remaining input, taken as one string. Only meaningful as the final
    /// argument of a command, since it consumes everything that follows.
    GreedyString,
    /// A signed integer constrained to an inclusive `[min, max]` range.
    Integer {
        /// Inclusive lower bound.
        min: i64,
        /// Inclusive upper bound.
        max: i64,
    },
}

impl ArgumentType {
    /// Builds an [`ArgumentType::Integer`] accepting values in `[min, max]`.
    pub const fn integer(min: i64, max: i64) -> Self {
        Self::Integer { min, max }
    }
}

/// A value parsed from a single command argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentValue {
    /// A string value, produced by [`ArgumentType::Word`] or
    /// [`ArgumentType::GreedyString`].
    String(String),
    /// A range-checked integer, produced by [`ArgumentType::Integer`].
    Integer(i64),
}

impl ArgumentValue {
    /// Returns the string contents if this is a [`ArgumentValue::String`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Integer(_) => None,
        }
    }

    /// Returns the integer value if this is a [`ArgumentValue::Integer`].
    pub const fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            Self::String(_) => None,
        }
    }
}

/// The set of arguments parsed for one dispatched command, keyed by node name.
///
/// Lookups are by the argument node's name as declared in the command tree.
/// Insertion order is preserved, but lookups are by name rather than position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedArgs {
    values: Vec<(String, ArgumentValue)>,
}

impl ParsedArgs {
    /// Creates an empty argument set.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records a parsed value under `name`.
    pub(crate) fn insert(&mut self, name: String, value: ArgumentValue) {
        self.values.push((name, value));
    }

    /// Returns the parsed value for the argument named `name`, if present.
    pub fn get(&self, name: &str) -> Option<&ArgumentValue> {
        self.values
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// Returns the integer value for `name`, if it was parsed as an integer.
    pub fn integer(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(ArgumentValue::as_integer)
    }

    /// Returns the string value for `name`, if it was parsed as a string.
    pub fn string(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(ArgumentValue::as_str)
    }

    /// Returns the number of parsed arguments.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` if no arguments were parsed.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}
