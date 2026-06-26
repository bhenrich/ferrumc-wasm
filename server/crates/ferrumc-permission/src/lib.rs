#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Permission nodes, subjects, grants, and operator levels.
//!
//! The model has four pieces:
//!
//! - [`PermissionNode`] — a validated, dotted-path identifier such as
//!   `ferrumc.command.gamemode`, with wildcard support (`ferrumc.command.*`,
//!   root `*`) and a [`PermissionNode::matches`] test.
//! - [`Grant`] / [`GrantEffect`] — an explicit `Allow` or `Deny` for a node.
//! - [`PermissionSet`] — a most-specific-wins collection of grants that
//!   [`resolves`](PermissionSet::resolve) a node to a tri-state [`Resolution`].
//! - [`Subject`] — an owned holder of a [`PermissionSet`] (plus an optional
//!   [`OperatorLevel`]) with a [`Subject::has_permission`] convenience.
//!
//! ```
//! use ferrumc_permission::{Grant, PermissionNode, Subject};
//!
//! let mut player = Subject::new();
//! player.add_grant(Grant::allow("ferrumc.command.*".parse::<PermissionNode>()?));
//! player.add_grant(Grant::deny("ferrumc.command.stop".parse::<PermissionNode>()?));
//!
//! assert!(player.has_permission(&"ferrumc.command.gamemode".parse()?));
//! assert!(!player.has_permission(&"ferrumc.command.stop".parse()?));
//! assert!(!player.has_permission(&"ferrumc.world.time".parse()?));
//! # Ok::<(), ferrumc_permission::NodeParseError>(())
//! ```

mod error;
mod grant;
mod level;
mod node;
mod subject;

pub use error::{InvalidOperatorLevel, NodeParseError, MAX_NODE_LEN};
pub use grant::{Grant, GrantEffect, PermissionSet, Resolution};
pub use level::OperatorLevel;
pub use node::PermissionNode;
pub use subject::Subject;
