#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod error;
mod gamemode;
mod ids;
mod text;
mod tick;

pub use error::{Result, ServerError};
pub use gamemode::{GameMode, InvalidGameModeId};
pub use ids::{ConnectionId, DimensionId, EntityId, PlayerId, PluginId, WorldId};
pub use text::{TextColor, TextComponent};
pub use tick::Tick;
