#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod argument;
mod builder;
mod context;
mod error;
mod result;
mod source;
mod tree;

pub use argument::{ArgumentType, ArgumentValue, ParsedArgs};
pub use builder::{argument, literal, CommandBuilder};
pub use context::CommandContext;
pub use error::CommandError;
pub use result::CommandResult;
pub use source::CommandSource;
pub use tree::CommandTree;
