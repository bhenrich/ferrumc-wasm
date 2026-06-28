#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! The greeter sample plugin: a demonstration of the in-process event surface.
//!
//! [`GreeterPlugin`] exercises every event the milestone adds, in one small
//! plugin, the same way [`ferrumc-plugin-spawn-protect`] and
//! [`ferrumc-plugin-block-rules`] exercise the block-decision surface:
//!
//! - **Greets** a joining player with a [`WorldIntent::Message`] from its
//!   [`on_event`](GreeterPlugin::on_event) handler (subscribed to
//!   [`EventKind::PlayerJoin`]).
//! - **Filters** chat: [`before_chat`](GreeterPlugin::before_chat) returns
//!   [`PluginEventDecision::Deny`] (dropping the line and showing the sender a
//!   reason) when the message contains a configured banned word, otherwise
//!   [`Allow`](PluginEventDecision::Allow).
//! - **Observes** movement and interaction: it logs each
//!   [`PluginEvent::PlayerMove`] and each
//!   [`before_interact`](GreeterPlugin::before_interact) right-click, then lets
//!   them proceed (movement cannot be vetoed; the interaction is allowed).
//!
//! It is an `rlib`-only, in-process plugin (the event surface does not cross the
//! C ABI), so it keeps `#![forbid(unsafe_code)]`. The host grants it exactly the
//! capabilities it requests: [`ReceiveEvents`](Capability::ReceiveEvents) for the
//! join/move notifications, [`SubmitIntents`](Capability::SubmitIntents) for the
//! greeting message, and [`VetoEvents`](Capability::VetoEvents) for the chat
//! filter.

use ferrumc_core::{PluginId, TextComponent};
use ferrumc_plugin_api::{
    Capability, CapabilityManifest, ChatAttempt, EventContext, EventKind, InteractAttempt, Plugin,
    PluginError, PluginEvent, PluginEventDecision, PluginMetadata, SetupContext, Version,
    WorldIntent,
};

/// The plugin's stable identifier.
pub const PLUGIN_ID: &str = "greeter";

/// The plugin's human-readable display name.
pub const PLUGIN_NAME: &str = "Greeter";

/// The message a joining player is greeted with.
const WELCOME_MESSAGE: &str = "Welcome to FerrumC! Be nice in chat.";

/// The default word the chat filter drops a message for (matched
/// case-insensitively as a substring).
pub const DEFAULT_BANNED_WORD: &str = "badword";

/// The feedback shown to a player whose message the filter dropped.
const FILTERED_MESSAGE: &str = "Your message was blocked by the chat filter.";

/// A sample plugin: greets joiners, filters a banned word from chat, and logs
/// movement and interaction.
///
/// Construct the default (banned word [`DEFAULT_BANNED_WORD`]) with
/// [`GreeterPlugin::new`], or set a custom word with
/// [`GreeterPlugin::with_banned_word`].
pub struct GreeterPlugin {
    /// The lowercase banned word the chat filter matches as a substring.
    banned_word: String,
}

impl GreeterPlugin {
    /// Builds the plugin with the default banned word ([`DEFAULT_BANNED_WORD`]).
    pub fn new() -> Self {
        Self::with_banned_word(DEFAULT_BANNED_WORD)
    }

    /// Builds the plugin with a custom banned word (matched case-insensitively).
    pub fn with_banned_word(word: impl Into<String>) -> Self {
        Self {
            banned_word: word.into().to_lowercase(),
        }
    }

    /// Returns the plugin's stable [`PluginId`].
    pub fn id() -> PluginId {
        PluginId::new(PLUGIN_ID)
    }

    /// Returns the capability manifest the plugin requires: event delivery, the
    /// intent sink (for the greeting), and the event-veto capability (for the chat
    /// filter).
    pub const fn capabilities() -> CapabilityManifest {
        CapabilityManifest::empty()
            .with(Capability::ReceiveEvents)
            .with(Capability::SubmitIntents)
            .with(Capability::VetoEvents)
    }

    /// Returns whether `message` trips the chat filter.
    fn is_filtered(&self, message: &str) -> bool {
        message.to_lowercase().contains(&self.banned_word)
    }
}

impl Default for GreeterPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for GreeterPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            Self::id(),
            PLUGIN_NAME,
            Version::new(0, 1, 0),
            Self::capabilities(),
        )
        .with_description("Greets joiners, filters a banned chat word, and logs move/interact")
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        // Subscribe to the observe-only notifications. Chat and interact are
        // delivered through the before_* decision hooks, not subscriptions.
        let events = ctx.events()?;
        events.subscribe(EventKind::PlayerJoin);
        events.subscribe(EventKind::PlayerMove);
        Ok(())
    }

    fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
        match event {
            PluginEvent::PlayerJoin { player } => {
                if let Ok(sink) = ctx.sink() {
                    let _ = sink.submit(WorldIntent::Message {
                        player: *player,
                        message: TextComponent::text(WELCOME_MESSAGE),
                    });
                }
            }
            PluginEvent::PlayerMove { player, from, to } => {
                // Observe-only: movement cannot be vetoed through this surface.
                tracing::debug!(
                    %player,
                    ?from,
                    ?to,
                    "greeter observed a player crossing into a new block"
                );
            }
            _ => {}
        }
    }

    fn before_chat(
        &mut self,
        ev: &ChatAttempt,
        _ctx: &mut EventContext<'_>,
    ) -> PluginEventDecision {
        if self.is_filtered(ev.message()) {
            tracing::debug!(player = %ev.player(), "greeter filtered a chat message");
            PluginEventDecision::Deny {
                message: Some(TextComponent::text(FILTERED_MESSAGE)),
            }
        } else {
            PluginEventDecision::Allow
        }
    }

    fn before_interact(
        &mut self,
        ev: &InteractAttempt,
        _ctx: &mut EventContext<'_>,
    ) -> PluginEventDecision {
        // Observe-only in this sample: log the right-click and let it proceed.
        tracing::debug!(
            player = %ev.player(),
            hand = ?ev.hand(),
            target = ?ev.target(),
            "greeter observed an interaction"
        );
        PluginEventDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use ferrumc_core::{DimensionId, PlayerId};
    use ferrumc_math::{BlockPos, ChunkPos, Direction, Vec3};
    use ferrumc_permission::{PermissionNode, Resolution};
    use ferrumc_plugin_api::{
        CommandSink, IntentError, InteractHand, InteractTarget, PermissionApi, PluginStorageApi,
        StorageError, WorldView,
    };

    use super::*;

    /// An in-memory storage facade for one namespace.
    #[derive(Default)]
    struct MapStorage {
        map: RefCell<HashMap<String, Vec<u8>>>,
    }
    impl PluginStorageApi for MapStorage {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(self.map.borrow().get(key).cloned())
        }
        fn put(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
            self.map.borrow_mut().insert(key.to_owned(), value.to_vec());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<bool, StorageError> {
            Ok(self.map.borrow_mut().remove(key).is_some())
        }
        fn keys(&self) -> Result<Vec<String>, StorageError> {
            Ok(self.map.borrow().keys().cloned().collect())
        }
    }

    struct NullWorld;
    impl WorldView for NullWorld {
        fn dimension(&self) -> DimensionId {
            DimensionId::new(0)
        }
        fn is_chunk_loaded(&self, _chunk: ChunkPos) -> bool {
            false
        }
        fn block_state_id(&self, _pos: BlockPos) -> Option<u32> {
            None
        }
        fn player_position(&self, _player: PlayerId) -> Option<Vec3> {
            None
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        intents: Vec<WorldIntent>,
    }
    impl CommandSink for RecordingSink {
        fn submit(&mut self, intent: WorldIntent) -> Result<(), IntentError> {
            self.intents.push(intent);
            Ok(())
        }
    }

    struct NullPermissions;
    impl PermissionApi for NullPermissions {
        fn has_permission(&self, _player: PlayerId, _node: &PermissionNode) -> bool {
            false
        }
        fn resolve(&self, _player: PlayerId, _node: &PermissionNode) -> Resolution {
            Resolution::Unset
        }
    }

    fn event_ctx<'a>(
        sink: &'a mut RecordingSink,
        world: &'a NullWorld,
        perms: &'a NullPermissions,
        storage: &'a MapStorage,
    ) -> EventContext<'a> {
        EventContext::new(CapabilityManifest::all(), world, sink, perms, storage)
    }

    #[test]
    fn greets_a_joining_player() {
        let mut plugin = GreeterPlugin::new();
        let mut sink = RecordingSink::default();
        let (world, perms, storage) = (NullWorld, NullPermissions, MapStorage::default());
        let mut ctx = event_ctx(&mut sink, &world, &perms, &storage);
        plugin.on_event(
            &PluginEvent::PlayerJoin {
                player: PlayerId::offline("Steve"),
            },
            &mut ctx,
        );
        assert_eq!(sink.intents.len(), 1, "join produced a greeting message");
        assert!(matches!(sink.intents[0], WorldIntent::Message { .. }));
    }

    #[test]
    fn filters_the_banned_word_case_insensitively() {
        let mut plugin = GreeterPlugin::new();
        let mut sink = RecordingSink::default();
        let (world, perms, storage) = (NullWorld, NullPermissions, MapStorage::default());
        let mut ctx = event_ctx(&mut sink, &world, &perms, &storage);
        let decision = plugin.before_chat(
            &ChatAttempt::new(PlayerId::offline("Steve"), "this has a BadWord in it"),
            &mut ctx,
        );
        assert!(
            matches!(decision, PluginEventDecision::Deny { message: Some(_) }),
            "the banned word is dropped with feedback"
        );
    }

    #[test]
    fn allows_a_clean_message() {
        let mut plugin = GreeterPlugin::new();
        let mut sink = RecordingSink::default();
        let (world, perms, storage) = (NullWorld, NullPermissions, MapStorage::default());
        let mut ctx = event_ctx(&mut sink, &world, &perms, &storage);
        let decision = plugin.before_chat(
            &ChatAttempt::new(PlayerId::offline("Steve"), "hello everyone"),
            &mut ctx,
        );
        assert_eq!(decision, PluginEventDecision::Allow);
    }

    #[test]
    fn interaction_is_observed_and_allowed() {
        let mut plugin = GreeterPlugin::new();
        let mut sink = RecordingSink::default();
        let (world, perms, storage) = (NullWorld, NullPermissions, MapStorage::default());
        let mut ctx = event_ctx(&mut sink, &world, &perms, &storage);
        let decision = plugin.before_interact(
            &InteractAttempt::new(
                PlayerId::offline("Steve"),
                InteractHand::Main,
                InteractTarget::Block {
                    pos: BlockPos::new(1, 2, 3),
                    face: Direction::Up,
                },
            ),
            &mut ctx,
        );
        assert_eq!(decision, PluginEventDecision::Allow);
    }

    #[test]
    fn custom_banned_word_is_honoured() {
        let mut plugin = GreeterPlugin::with_banned_word("frobnicate");
        let mut sink = RecordingSink::default();
        let (world, perms, storage) = (NullWorld, NullPermissions, MapStorage::default());
        let mut ctx = event_ctx(&mut sink, &world, &perms, &storage);
        assert!(plugin
            .before_chat(
                &ChatAttempt::new(PlayerId::offline("Steve"), "please do not FROBNICATE"),
                &mut ctx,
            )
            .is_deny());
        // The default word is not configured here, so it passes through.
        assert_eq!(
            plugin.before_chat(
                &ChatAttempt::new(PlayerId::offline("Steve"), "badword"),
                &mut ctx,
            ),
            PluginEventDecision::Allow
        );
    }
}
