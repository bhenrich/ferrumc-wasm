//! The spawn-protection policy: a small, pure decision the plugin owns and the
//! host (or application) consults before a block edit is applied.
//!
//! A [`SpawnProtect`] is a copyable value built from a centre column and a
//! radius. It answers two questions with no side effects: whether a block is
//! inside the protected square ([`SpawnProtect::is_protected`]) and whether an
//! edit there should be vetoed for an actor with or without bypass
//! ([`SpawnProtect::vetoes`]). The same value round-trips through the plugin's
//! private storage via [`SpawnProtect::to_bytes`] / [`SpawnProtect::from_bytes`],
//! which is how the plugin persists its configuration.

use ferrumc_math::BlockPos;

/// The default protection radius, in blocks, seeded into storage when a plugin
/// has no stored configuration yet.
pub const DEFAULT_RADIUS: i32 = 16;

/// The fixed byte length of an encoded [`SpawnProtect`] in plugin storage.
///
/// Three big-endian `i32`s: the centre `x`, the centre `z`, and the radius.
pub const ENCODED_LEN: usize = 12;

/// A spawn-protection policy over a square region of the world.
///
/// The protected region is the axis-aligned square of `x`/`z` columns within
/// [`radius`](SpawnProtect::radius) blocks (Chebyshev distance) of a centre
/// column; the `y` coordinate is ignored, so protection spans the full column
/// height. A radius of zero (or less) disables protection entirely, which is the
/// default when the plugin is not configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnProtect {
    /// The protected centre's block `x`.
    center_x: i32,
    /// The protected centre's block `z`.
    center_z: i32,
    /// The protection radius in blocks (Chebyshev). `<= 0` disables protection.
    radius: i32,
}

impl SpawnProtect {
    /// Builds a policy protecting the square of `radius` blocks around the centre
    /// column `(center_x, center_z)`.
    ///
    /// A non-positive `radius` yields a disabled policy that protects nothing.
    pub const fn new(center_x: i32, center_z: i32, radius: i32) -> Self {
        Self {
            center_x,
            center_z,
            radius,
        }
    }

    /// A disabled policy, centred at the origin with a zero radius, that protects
    /// nothing.
    pub const fn disabled() -> Self {
        Self::new(0, 0, 0)
    }

    /// Returns the protected centre column as `(x, z)` block coordinates.
    pub const fn center(self) -> (i32, i32) {
        (self.center_x, self.center_z)
    }

    /// Returns the protection radius in blocks.
    pub const fn radius(self) -> i32 {
        self.radius
    }

    /// Returns whether this policy protects anything (a positive radius).
    pub const fn is_enabled(self) -> bool {
        self.radius > 0
    }

    /// Returns whether the column containing `pos` lies inside the protected
    /// square.
    ///
    /// Always `false` for a disabled policy.
    pub fn is_protected(self, pos: BlockPos) -> bool {
        if self.radius <= 0 {
            return false;
        }
        let dx = (pos.x() - self.center_x).abs();
        let dz = (pos.z() - self.center_z).abs();
        dx <= self.radius && dz <= self.radius
    }

    /// Returns whether an edit at `pos` should be **vetoed**.
    ///
    /// An edit is vetoed when its column is protected and the actor does not hold
    /// the bypass permission (`has_bypass == false`). A bypassing actor is never
    /// vetoed, and nothing is vetoed outside the protected square.
    pub fn vetoes(self, pos: BlockPos, has_bypass: bool) -> bool {
        !has_bypass && self.is_protected(pos)
    }

    /// Encodes this policy into its fixed [`ENCODED_LEN`]-byte storage form.
    pub fn to_bytes(self) -> [u8; ENCODED_LEN] {
        let mut bytes = [0u8; ENCODED_LEN];
        bytes[0..4].copy_from_slice(&self.center_x.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.center_z.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.radius.to_be_bytes());
        bytes
    }

    /// Decodes a policy from its storage form, or `None` if `bytes` is not
    /// exactly [`ENCODED_LEN`] bytes long.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; ENCODED_LEN] = bytes.try_into().ok()?;
        let center_x = i32::from_be_bytes(bytes[0..4].try_into().ok()?);
        let center_z = i32::from_be_bytes(bytes[4..8].try_into().ok()?);
        let radius = i32::from_be_bytes(bytes[8..12].try_into().ok()?);
        Some(Self::new(center_x, center_z, radius))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_within_the_square_only() {
        let guard = SpawnProtect::new(8, 8, 16);
        assert!(guard.is_enabled());
        // Centre and edges (inclusive) are protected.
        assert!(guard.is_protected(BlockPos::new(8, 63, 8)));
        assert!(guard.is_protected(BlockPos::new(8 + 16, 200, 8 - 16)));
        // Just outside on either axis is not.
        assert!(!guard.is_protected(BlockPos::new(8 + 17, 63, 8)));
        assert!(!guard.is_protected(BlockPos::new(8, 63, 8 - 17)));
    }

    #[test]
    fn disabled_policy_protects_nothing() {
        let guard = SpawnProtect::disabled();
        assert!(!guard.is_enabled());
        assert!(!guard.is_protected(BlockPos::new(0, 0, 0)));
        assert!(!guard.vetoes(BlockPos::new(0, 0, 0), false));
        // A zero radius is disabled even when explicitly centred.
        assert!(!SpawnProtect::new(8, 8, 0).is_protected(BlockPos::new(8, 64, 8)));
    }

    #[test]
    fn veto_respects_bypass_and_protection() {
        let guard = SpawnProtect::new(0, 0, 4);
        let inside = BlockPos::new(2, 64, -2);
        let outside = BlockPos::new(100, 64, 0);
        // Inside + no bypass -> vetoed; bypass lifts it.
        assert!(guard.vetoes(inside, false));
        assert!(!guard.vetoes(inside, true));
        // Outside is never vetoed, bypass or not.
        assert!(!guard.vetoes(outside, false));
        assert!(!guard.vetoes(outside, true));
    }

    #[test]
    fn config_round_trips_through_bytes() {
        let guard = SpawnProtect::new(-128, 4096, 24);
        let restored = SpawnProtect::from_bytes(&guard.to_bytes()).expect("round-trips");
        assert_eq!(guard, restored);
        assert_eq!(guard.center(), (-128, 4096));
        assert_eq!(guard.radius(), 24);
    }

    #[test]
    fn from_bytes_rejects_wrong_length() {
        assert_eq!(SpawnProtect::from_bytes(&[]), None);
        assert_eq!(SpawnProtect::from_bytes(&[0u8; ENCODED_LEN - 1]), None);
        assert_eq!(SpawnProtect::from_bytes(&[0u8; ENCODED_LEN + 1]), None);
    }
}
