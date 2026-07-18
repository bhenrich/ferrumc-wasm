//! Typed world operations submitted through the bounded host command buffer.

use crate::{BlockPos, FacadeError, PlayerId, Vec3};

/// Maximum byte length of a plain-text player message.
pub const MAX_MESSAGE_BYTES: usize = 4_096;

/// One exact block-state write request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetBlockOperation {
    pos: BlockPos,
    block_state_id: u32,
}

impl SetBlockOperation {
    /// Creates an exact block-state write request.
    pub const fn new(pos: BlockPos, block_state_id: u32) -> Self {
        Self {
            pos,
            block_state_id,
        }
    }

    /// Returns the block position.
    pub const fn pos(self) -> BlockPos {
        self.pos
    }

    /// Returns the opaque registry block-state identifier.
    pub const fn block_state_id(self) -> u32 {
        self.block_state_id
    }
}

/// One player teleport request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TeleportOperation {
    player: PlayerId,
    position: Vec3,
}

impl TeleportOperation {
    /// Creates a teleport after rejecting non-finite coordinates.
    pub fn new(player: PlayerId, position: Vec3) -> Result<Self, FacadeError> {
        if !position.x.is_finite() || !position.y.is_finite() || !position.z.is_finite() {
            return Err(FacadeError::InvalidInput {
                resource: "teleport position",
                reason: "all coordinates must be finite",
            });
        }
        Ok(Self { player, position })
    }

    /// Returns the player to teleport.
    pub const fn player(self) -> PlayerId {
        self.player
    }

    /// Returns the requested position.
    pub const fn position(self) -> Vec3 {
        self.position
    }
}

/// One plain-text player message request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageOperation {
    player: PlayerId,
    message: String,
}

impl MessageOperation {
    /// Creates a bounded plain-text message.
    pub fn new(player: PlayerId, message: impl Into<String>) -> Result<Self, FacadeError> {
        let message = message.into();
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(FacadeError::LimitExceeded {
                resource: "message",
                len: message.len(),
                max: MAX_MESSAGE_BYTES,
            });
        }
        Ok(Self { player, message })
    }

    /// Returns the message recipient.
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns the plain `UTF-8` message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// A packaging-independent world operation queued for host validation.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WorldOperation {
    /// Set one block state.
    SetBlock(SetBlockOperation),
    /// Teleport one player.
    Teleport(TeleportOperation),
    /// Send one plain-text message.
    Message(MessageOperation),
}
