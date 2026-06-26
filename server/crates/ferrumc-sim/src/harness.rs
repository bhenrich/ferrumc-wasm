//! A deterministic, wall-clock-free harness for stepping the simulation.

use ferrumc_core::Tick;
use ferrumc_math::ShardPos;

use crate::coordinator::{TickCoordinator, TickRate};
use crate::error::SimError;
use crate::message::{GameInput, GameOutput};
use crate::shard::SimShard;

/// The result of stepping the harness one tick.
///
/// Pairs the [`Tick`] that was just completed with the [`GameOutput`]s the shard
/// produced during it, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct TickOutcome {
    tick: Tick,
    outputs: Vec<GameOutput>,
}

impl TickOutcome {
    /// Returns the tick this outcome was produced on.
    pub const fn tick(&self) -> Tick {
        self.tick
    }

    /// Returns the outputs produced during the tick, in order.
    pub fn outputs(&self) -> &[GameOutput] {
        &self.outputs
    }

    /// Consumes the outcome, returning the owned outputs.
    pub fn into_outputs(self) -> Vec<GameOutput> {
        self.outputs
    }
}

/// Couples a [`TickCoordinator`] with one [`SimShard`] for deterministic,
/// wall-clock-free stepping.
///
/// This is the entry point for driving the simulation in tests (and any
/// deterministic replay): submit inputs, call [`tick`](SimHarness::tick), then
/// inspect the returned [`TickOutcome`] or the shard state. Because nothing here
/// reads a clock or spawns tasks, identical input/`tick` sequences are perfectly
/// reproducible.
#[derive(Debug, Clone)]
pub struct SimHarness {
    coordinator: TickCoordinator,
    shard: SimShard,
}

impl SimHarness {
    /// Creates a harness with a fresh coordinator at the given `rate` and an
    /// empty shard at `shard_pos`.
    pub fn new(rate: TickRate, shard_pos: ShardPos) -> Self {
        Self {
            coordinator: TickCoordinator::new(rate),
            shard: SimShard::new(shard_pos),
        }
    }

    /// Creates a harness from an explicit coordinator and shard.
    ///
    /// Useful for resuming at a non-zero tick or configuring a custom inbox
    /// capacity.
    pub fn from_parts(coordinator: TickCoordinator, shard: SimShard) -> Self {
        Self { coordinator, shard }
    }

    /// Submits an input to the shard inbox for application at the next tick.
    ///
    /// Forwards [`SimShard::enqueue`]'s reject backpressure: returns
    /// [`SimError::InboxFull`] if the inbox is full.
    pub fn submit(&mut self, input: GameInput) -> Result<(), SimError> {
        self.shard.enqueue(input)
    }

    /// Advances exactly one tick: drains and applies the inbox, returning the
    /// completed tick and its outputs.
    ///
    /// Returns [`SimError::TickOverflow`] only if the tick counter would
    /// overflow (see [`TickCoordinator::advance`]); the inbox is left untouched
    /// in that case.
    pub fn tick(&mut self) -> Result<TickOutcome, SimError> {
        let tick = self.coordinator.advance()?;
        let outputs = self.shard.run_tick();
        Ok(TickOutcome { tick, outputs })
    }

    /// Returns the current tick counter value.
    pub const fn current_tick(&self) -> Tick {
        self.coordinator.current()
    }

    /// Returns a shared reference to the shard for state inspection.
    pub const fn shard(&self) -> &SimShard {
        &self.shard
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_math::Vec3;

    use super::*;

    #[test]
    fn tick_outcome_accessors() {
        let outcome = TickOutcome {
            tick: Tick::new(7),
            outputs: vec![GameOutput::PlayerDespawned {
                player: ferrumc_core::PlayerId::offline("x"),
            }],
        };
        assert_eq!(outcome.tick(), Tick::new(7));
        assert_eq!(outcome.outputs().len(), 1);
        assert_eq!(outcome.into_outputs().len(), 1);
    }

    #[test]
    fn from_parts_resumes_tick_and_uses_provided_shard() {
        let coordinator = TickCoordinator::resuming_at(TickRate::VANILLA, Tick::new(41));
        let shard = SimShard::new(ShardPos::new(2, 2));
        let mut harness = SimHarness::from_parts(coordinator, shard);
        assert_eq!(harness.current_tick(), Tick::new(41));
        assert_eq!(harness.shard().shard_pos(), ShardPos::new(2, 2));

        let outcome = harness.tick().expect("tick");
        assert_eq!(outcome.tick(), Tick::new(42));
    }

    #[test]
    fn submit_then_tick_applies_and_advances() {
        let mut harness = SimHarness::new(TickRate::VANILLA, ShardPos::new(0, 0));
        let p = ferrumc_core::PlayerId::offline("frank");
        harness
            .submit(GameInput::PlayerJoin {
                player: p,
                position: Vec3::new(8.0, 70.0, 8.0),
            })
            .expect("room");

        assert_eq!(harness.current_tick(), Tick::ZERO);
        assert_eq!(harness.shard().player_count(), 0);

        let outcome = harness.tick().expect("tick");
        assert_eq!(outcome.tick(), Tick::new(1));
        assert_eq!(harness.current_tick(), Tick::new(1));
        assert_eq!(harness.shard().player_count(), 1);
        assert_eq!(
            outcome.outputs(),
            &[GameOutput::PlayerSpawned {
                player: p,
                position: Vec3::new(8.0, 70.0, 8.0)
            }]
        );
    }
}
