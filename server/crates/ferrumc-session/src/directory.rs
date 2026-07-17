//! Validated logical-shard routing directory.
//!
//! A logical [`ShardId`] is a spatial target. A directory entry is a separate
//! runtime registration that may cover either that one target or every target
//! in one world/dimension. Mutating a live registration requires its current
//! [`ShardLease`], so delayed workers cannot replace or remove a newer endpoint.

use core::fmt;
use std::collections::BTreeMap;
use std::sync::Arc;

use ferrumc_core::{DimensionId, WorldId};
use ferrumc_sim::ShardId;

/// The canonical set of logical shard targets covered by one runtime endpoint.
///
/// Exact coverage deterministically takes precedence over world coverage during
/// resolution. This permits a single current world owner while individual
/// logical shards are transferred to dedicated endpoints later.
///
/// Raw coordinate pairs cannot be used as coverage:
///
/// ```compile_fail
/// use ferrumc_session::ShardCoverage;
///
/// let _ = ShardCoverage::exact((0, 0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ShardCoverage {
    /// One canonical logical shard.
    Exact(ShardId),
    /// Every logical shard in one typed world and dimension.
    World {
        /// The covered world.
        world: WorldId,
        /// The covered dimension within `world`.
        dimension: DimensionId,
    },
}

impl ShardCoverage {
    /// Creates coverage for exactly `shard`.
    pub const fn exact(shard: ShardId) -> Self {
        Self::Exact(shard)
    }

    /// Creates world-covering routing for one typed world and dimension.
    pub const fn world(world: WorldId, dimension: DimensionId) -> Self {
        Self::World { world, dimension }
    }

    /// Returns whether this coverage contains `target`.
    pub fn contains(self, target: ShardId) -> bool {
        match self {
            Self::Exact(shard) => shard == target,
            Self::World { world, dimension } => {
                world == target.world() && dimension == target.dimension()
            }
        }
    }
}

impl fmt::Display for ShardCoverage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(shard) => write!(formatter, "exact {shard}"),
            Self::World { world, dimension } => {
                write!(formatter, "world {world}, dimension {dimension}")
            }
        }
    }
}

/// A monotonically allocated directory-registration generation.
///
/// Generations are never reused within a directory, including after removal,
/// which prevents an old lease from matching a later registration by ABA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardGeneration(u64);

impl ShardGeneration {
    /// Returns the nonzero generation number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ShardGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Stable identity of one registration lineage.
///
/// A checked replacement preserves this id while advancing the generation.
/// Removing and later re-registering the same coverage allocates a new id, so
/// data-plane bindings can distinguish worker rotation from an unrelated ABA
/// registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardRegistrationId(u64);

impl ShardRegistrationId {
    /// Returns the lineage value allocated within its directory instance.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ShardRegistrationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug)]
struct DirectoryIdentity;

/// The capability token for one directory registration generation.
///
/// A lease is a cloneable validation token bound to the directory that created
/// it, not an RAII guard: dropping it does not unregister anything.
/// [`ShardDirectory::replace`] and [`ShardDirectory::remove`] accept only the
/// currently active lease.
#[derive(Debug, Clone)]
pub struct ShardLease {
    directory: Arc<DirectoryIdentity>,
    coverage: ShardCoverage,
    registration_id: ShardRegistrationId,
    generation: ShardGeneration,
}

impl ShardLease {
    /// Returns the registration coverage this lease belongs to.
    pub const fn coverage(&self) -> ShardCoverage {
        self.coverage
    }

    /// Returns the stable identity preserved by authorized replacements.
    pub const fn registration_id(&self) -> ShardRegistrationId {
        self.registration_id
    }

    /// Returns the exact generation this lease authorizes.
    pub const fn generation(&self) -> ShardGeneration {
        self.generation
    }
}

impl PartialEq for ShardLease {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.directory, &other.directory)
            && self.coverage == other.coverage
            && self.registration_id == other.registration_id
            && self.generation == other.generation
    }
}

impl Eq for ShardLease {}

/// A classified directory mutation or lease-validation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ShardDirectoryError {
    /// An unconditional registration attempted to overwrite live coverage.
    #[error("{coverage} is already registered at generation {generation}")]
    RegistrationOccupied {
        /// The coverage that already has a live endpoint.
        coverage: ShardCoverage,
        /// The generation left active by the rejected operation.
        generation: ShardGeneration,
    },

    /// A lease created by a different directory instance was supplied.
    #[error("lease for {coverage} belongs to a different shard directory")]
    ForeignLease {
        /// The coverage named by the foreign lease.
        coverage: ShardCoverage,
    },

    /// A replacement, removal, or lookup supplied a lease that is no longer current.
    #[error(
        "stale lease for {coverage}: requested generation {lease_generation}, current generation {current_generation:?}"
    )]
    StaleLease {
        /// The registration coverage named by the stale lease.
        coverage: ShardCoverage,
        /// The generation supplied by the rejected operation.
        lease_generation: ShardGeneration,
        /// The live generation, or `None` if the coverage is unregistered.
        current_generation: Option<ShardGeneration>,
    },

    /// This directory exhausted all nonzero `u64` registration generations.
    #[error("this shard directory's generation space is exhausted")]
    GenerationExhausted,
}

/// A borrowed current directory registration.
#[derive(Debug, Clone)]
pub struct ShardDirectoryEntry<'a, T> {
    lease: ShardLease,
    endpoint: &'a T,
}

impl<'a, T> ShardDirectoryEntry<'a, T> {
    /// Returns the current registration lease.
    pub fn lease(&self) -> ShardLease {
        self.lease.clone()
    }

    /// Returns the stable identity of this registration lineage.
    pub const fn registration_id(&self) -> ShardRegistrationId {
        self.lease.registration_id()
    }

    /// Returns the registered endpoint value.
    pub const fn endpoint(&self) -> &'a T {
        self.endpoint
    }
}

/// A directory resolution from a logical target to its current endpoint.
#[derive(Debug, Clone)]
pub struct ResolvedShard<'a, T> {
    target: ShardId,
    entry: ShardDirectoryEntry<'a, T>,
}

impl<'a, T> ResolvedShard<'a, T> {
    /// Returns the canonical logical shard that was resolved.
    pub const fn target(&self) -> ShardId {
        self.target
    }

    /// Returns the exact or world coverage that matched the target.
    pub const fn coverage(&self) -> ShardCoverage {
        self.entry.lease.coverage()
    }

    /// Returns the matched registration's current lease.
    pub fn lease(&self) -> ShardLease {
        self.entry.lease()
    }

    /// Returns the stable identity of the matched registration lineage.
    pub const fn registration_id(&self) -> ShardRegistrationId {
        self.entry.registration_id()
    }

    /// Returns the matched endpoint.
    pub const fn endpoint(&self) -> &'a T {
        self.entry.endpoint()
    }
}

#[derive(Debug)]
struct StoredEntry<T> {
    registration_id: ShardRegistrationId,
    generation: ShardGeneration,
    endpoint: T,
}

/// A deterministic, lease-validated routing directory.
///
/// The directory stores at most one endpoint for each [`ShardCoverage`].
/// [`resolve`](Self::resolve) checks exact coverage first and then the matching
/// world/dimension entry. It never enumerates the full logical target domain
/// covered by a world entry.
#[derive(Debug)]
pub struct ShardDirectory<T> {
    identity: Arc<DirectoryIdentity>,
    entries: BTreeMap<ShardCoverage, StoredEntry<T>>,
    last_generation: u64,
}

impl<T> ShardDirectory<T> {
    /// Creates an empty directory whose first registration receives generation 1.
    pub fn new() -> Self {
        Self {
            identity: Arc::new(DirectoryIdentity),
            entries: BTreeMap::new(),
            last_generation: 0,
        }
    }

    /// Returns the number of registered runtime endpoints.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the directory has no registered endpoints.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Registers `endpoint` for vacant `coverage`.
    ///
    /// # Errors
    ///
    /// Returns [`ShardDirectoryError::RegistrationOccupied`] without changing
    /// the active endpoint when coverage is already registered, or
    /// [`ShardDirectoryError::GenerationExhausted`] without inserting an entry
    /// if no fresh generation remains.
    pub fn register(
        &mut self,
        coverage: ShardCoverage,
        endpoint: T,
    ) -> Result<ShardLease, ShardDirectoryError> {
        if let Some(current) = self.entries.get(&coverage) {
            return Err(ShardDirectoryError::RegistrationOccupied {
                coverage,
                generation: current.generation,
            });
        }
        let generation = self.next_generation()?;
        self.entries.insert(
            coverage,
            StoredEntry {
                registration_id: ShardRegistrationId(generation.get()),
                generation,
                endpoint,
            },
        );
        self.last_generation = generation.get();
        Ok(ShardLease {
            directory: Arc::clone(&self.identity),
            coverage,
            registration_id: ShardRegistrationId(generation.get()),
            generation,
        })
    }

    /// Replaces the endpoint authorized by `lease` and advances its generation.
    ///
    /// # Errors
    ///
    /// Returns [`ShardDirectoryError::ForeignLease`] for another directory's
    /// lease, [`ShardDirectoryError::StaleLease`] if `lease` is absent or no
    /// longer current, or [`ShardDirectoryError::GenerationExhausted`] if a new
    /// generation cannot be allocated. Every failure leaves the live entry unchanged.
    pub fn replace(
        &mut self,
        lease: &ShardLease,
        endpoint: T,
    ) -> Result<ShardLease, ShardDirectoryError> {
        self.validate_lease(lease)?;
        let generation = self.next_generation()?;
        let Some(current) = self.entries.get_mut(&lease.coverage) else {
            return Err(self.stale_error(lease));
        };
        current.generation = generation;
        current.endpoint = endpoint;
        self.last_generation = generation.get();
        Ok(ShardLease {
            directory: Arc::clone(&self.identity),
            coverage: lease.coverage,
            registration_id: lease.registration_id,
            generation,
        })
    }

    /// Removes and returns the endpoint authorized by `lease`.
    ///
    /// The generation high-water mark is retained, so re-registering the same
    /// coverage cannot make this lease current again.
    ///
    /// # Errors
    ///
    /// Returns [`ShardDirectoryError::ForeignLease`] or
    /// [`ShardDirectoryError::StaleLease`] without changing the directory if
    /// `lease` belongs elsewhere, is absent, or is no longer current.
    pub fn remove(&mut self, lease: &ShardLease) -> Result<T, ShardDirectoryError> {
        self.validate_lease(lease)?;
        self.entries
            .remove(&lease.coverage)
            .map(|entry| entry.endpoint)
            .ok_or_else(|| self.stale_error(lease))
    }

    /// Resolves `target` with deterministic exact-over-world precedence.
    pub fn resolve(&self, target: ShardId) -> Option<ResolvedShard<'_, T>> {
        let exact = ShardCoverage::Exact(target);
        let coverage = if self.entries.contains_key(&exact) {
            exact
        } else {
            let world = ShardCoverage::World {
                world: target.world(),
                dimension: target.dimension(),
            };
            self.entries.contains_key(&world).then_some(world)?
        };
        let entry = self.current(coverage)?;
        Some(ResolvedShard { target, entry })
    }

    /// Returns the current entry for exactly `coverage`, without applying
    /// fallback resolution.
    pub fn current(&self, coverage: ShardCoverage) -> Option<ShardDirectoryEntry<'_, T>> {
        self.entries
            .get(&coverage)
            .map(|entry| ShardDirectoryEntry {
                lease: ShardLease {
                    directory: Arc::clone(&self.identity),
                    coverage,
                    registration_id: entry.registration_id,
                    generation: entry.generation,
                },
                endpoint: &entry.endpoint,
            })
    }

    /// Returns the entry named by `lease` only if that exact generation is current.
    ///
    /// # Errors
    ///
    /// Returns [`ShardDirectoryError::ForeignLease`] or
    /// [`ShardDirectoryError::StaleLease`] for a foreign, absent, or superseded
    /// lease.
    pub fn get(
        &self,
        lease: &ShardLease,
    ) -> Result<ShardDirectoryEntry<'_, T>, ShardDirectoryError> {
        self.validate_lease(lease)?;
        self.current(lease.coverage)
            .ok_or_else(|| self.stale_error(lease))
    }

    /// Iterates over current registrations in deterministic coverage order.
    pub fn registrations(&self) -> impl ExactSizeIterator<Item = ShardDirectoryEntry<'_, T>> + '_ {
        self.entries
            .iter()
            .map(|(&coverage, entry)| ShardDirectoryEntry {
                lease: ShardLease {
                    directory: Arc::clone(&self.identity),
                    coverage,
                    registration_id: entry.registration_id,
                    generation: entry.generation,
                },
                endpoint: &entry.endpoint,
            })
    }

    fn validate_lease(&self, lease: &ShardLease) -> Result<(), ShardDirectoryError> {
        if !Arc::ptr_eq(&self.identity, &lease.directory) {
            return Err(ShardDirectoryError::ForeignLease {
                coverage: lease.coverage,
            });
        }
        match self.entries.get(&lease.coverage) {
            Some(current)
                if current.registration_id == lease.registration_id
                    && current.generation == lease.generation =>
            {
                Ok(())
            }
            _ => Err(self.stale_error(lease)),
        }
    }

    fn stale_error(&self, lease: &ShardLease) -> ShardDirectoryError {
        ShardDirectoryError::StaleLease {
            coverage: lease.coverage,
            lease_generation: lease.generation,
            current_generation: self
                .entries
                .get(&lease.coverage)
                .map(|entry| entry.generation),
        }
    }

    fn next_generation(&self) -> Result<ShardGeneration, ShardDirectoryError> {
        self.last_generation
            .checked_add(1)
            .map(ShardGeneration)
            .ok_or(ShardDirectoryError::GenerationExhausted)
    }
}

impl<T> Default for ShardDirectory<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_exhaustion_never_partially_registers() {
        let mut directory = ShardDirectory::<()>::new();
        directory.last_generation = u64::MAX;

        assert_eq!(
            directory.register(
                ShardCoverage::world(WorldId::new(0), DimensionId::new(0)),
                (),
            ),
            Err(ShardDirectoryError::GenerationExhausted),
        );
        assert!(directory.is_empty());
    }

    #[test]
    fn generation_exhaustion_never_partially_replaces() {
        let coverage = ShardCoverage::world(WorldId::new(0), DimensionId::new(0));
        let mut directory = ShardDirectory::new();
        let lease = directory.register(coverage, "current").expect("register");
        directory.last_generation = u64::MAX;

        assert_eq!(
            directory.replace(&lease, "candidate"),
            Err(ShardDirectoryError::GenerationExhausted),
        );
        assert_eq!(
            directory
                .current(coverage)
                .expect("current entry")
                .endpoint(),
            &"current",
        );
        assert_eq!(
            directory.current(coverage).expect("current entry").lease(),
            lease,
        );
    }
}
