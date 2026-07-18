#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Packaging-independent author API for `FerrumC` plugins.
//!
//! Host adapters implement one call-scoped services object. Context accessors
//! check the plugin's granted capabilities and lend narrowly scoped facade
//! wrappers over that object. This keeps the author contract identical across
//! packaging modes without exposing an adapter's `ABI` handles or server
//! internals.

mod command;
mod context;
mod declaration;
mod error;
mod event;
mod operation;
mod plugin;
mod services;

pub use command::{
    CommandArgument, CommandArgumentValue, CommandDefinition, CommandInvocation, CommandNode,
    CommandNodeKind, HandlerId, IntegerBounds, MAX_COMMAND_ARGUMENTS, MAX_COMMAND_INVOCATION_BYTES,
    MAX_COMMAND_NAME_BYTES, MAX_COMMAND_NODES, MAX_COMMAND_TEXT_BYTES,
};
pub use context::{EventContext, LoadContext, UnloadContext};
pub use declaration::{
    PluginDeclaration, PluginVersion, MAX_PLUGIN_ID_BYTES, MAX_PLUGIN_NAME_BYTES,
};
pub use error::{CommandError, DeclarationError, FacadeError, PluginError};
pub use event::{
    BlockDecision, BlockEvent, BlockPlaceEvent, ChatAttempt, Event, EventDecision, EventKind,
    Feedback, InteractHand, InteractTarget, InteractionAttempt, MoveEvent, PlaceAttempt,
    PlayerEvent, MAX_CHAT_BYTES, MAX_FEEDBACK_BYTES,
};
pub use operation::{
    MessageOperation, SetBlockOperation, TeleportOperation, WorldOperation, MAX_MESSAGE_BYTES,
};
pub use plugin::Plugin;
pub use services::{
    CommandRegistrations, DiagnosticLevel, Diagnostics, EventSubscriptions, HostServices,
    NamespacedStorage, PermissionQueries, TimerId, Timers, WorldOperations, WorldView,
    MAX_DIAGNOSTIC_BYTES, MAX_STORAGE_KEYS, MAX_STORAGE_KEY_BYTES, MAX_STORAGE_KEY_LIST_BYTES,
    MAX_STORAGE_VALUE_BYTES,
};

/// Stable player, entity, and tick value types used by the SDK.
pub use ferrumc_core::{EntityId, PlayerId, Tick};
/// Typed coordinate and vector values used by the SDK.
pub use ferrumc_math::{BlockPos, ChunkPos, Direction, Vec3};
/// Validated permission nodes and tri-state resolution outcomes.
pub use ferrumc_permission::{PermissionNode, Resolution};
/// Established capability assignments shared by both packaging adapters.
pub use ferrumc_plugin_api::{Capability, CapabilityError, CapabilityManifest};
