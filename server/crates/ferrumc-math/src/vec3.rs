//! A 3-component `f64` vector for positions, velocities, and geometry math.

use core::ops::{Add, Mul, Neg, Sub};

/// A 3-dimensional vector of `f64` components.
///
/// `Vec3` is the floating-point workhorse for positions, velocities, and ray
/// math. It is a plain mathematical value type, so its components are public
/// fields: there is no hidden representation to protect, and exposing `x`, `y`,
/// and `z` directly keeps the arithmetic readable. This is the documented
/// exception to the crate's "no public fields across boundaries" rule.
///
/// Addition, subtraction, scaling, and negation are provided through the
/// standard operator traits ([`Add`], [`Sub`], [`Mul<f64>`](Mul), [`Neg`]);
/// [`Vec3::scale`] is the named equivalent of `* f64`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    /// The x component (east/west axis; east is positive).
    pub x: f64,
    /// The y component (vertical axis; up is positive).
    pub y: f64,
    /// The z component (north/south axis; south is positive).
    pub z: f64,
}

impl Vec3 {
    /// The zero vector, `(0, 0, 0)`.
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);

    /// Creates a vector from its three components.
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    /// Returns `self` with every component multiplied by `factor`.
    #[must_use]
    pub fn scale(self, factor: f64) -> Self {
        Self::new(self.x * factor, self.y * factor, self.z * factor)
    }

    /// Returns the dot product of `self` and `other`.
    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// Returns the squared Euclidean length.
    ///
    /// Cheaper than [`Vec3::length`] because it avoids the square root; prefer
    /// it when only comparing magnitudes.
    pub fn length_squared(self) -> f64 {
        self.dot(self)
    }

    /// Returns the Euclidean length (magnitude) of the vector.
    pub fn length(self) -> f64 {
        self.length_squared().sqrt()
    }

    /// Returns the unit vector pointing in the same direction, or `None` when
    /// the vector has zero length (which has no defined direction).
    pub fn normalize(self) -> Option<Self> {
        let len = self.length();
        if len > 0.0 {
            Some(self.scale(1.0 / len))
        } else {
            None
        }
    }
}

impl Add for Vec3 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl Mul<f64> for Vec3 {
    type Output = Self;

    fn mul(self, rhs: f64) -> Self {
        self.scale(rhs)
    }
}

impl Neg for Vec3 {
    type Output = Self;

    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Floats are compared within a small tolerance to avoid spurious failures
    /// from rounding (and to keep the comparison clear of exact-equality lints).
    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn vec_approx(a: Vec3, b: Vec3) -> bool {
        approx(a.x, b.x) && approx(a.y, b.y) && approx(a.z, b.z)
    }

    #[test]
    fn add_sub_scale_and_neg() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, -1.0, 0.5);
        assert!(vec_approx(a + b, Vec3::new(5.0, 1.0, 3.5)));
        assert!(vec_approx(a - b, Vec3::new(-3.0, 3.0, 2.5)));
        assert!(vec_approx(a.scale(2.0), Vec3::new(2.0, 4.0, 6.0)));
        assert!(vec_approx(a * 2.0, a.scale(2.0)));
        assert!(vec_approx(-a, Vec3::new(-1.0, -2.0, -3.0)));
        assert_eq!(Vec3::default(), Vec3::ZERO);
    }

    #[test]
    fn dot_product() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, 5.0, 6.0);
        // 1*4 + 2*5 + 3*6 = 32
        assert!(approx(a.dot(b), 32.0));
        // Orthogonal axes dot to zero.
        assert!(approx(
            Vec3::new(1.0, 0.0, 0.0).dot(Vec3::new(0.0, 1.0, 0.0)),
            0.0
        ));
    }

    #[test]
    fn length_and_length_squared() {
        let v = Vec3::new(3.0, 4.0, 0.0);
        assert!(approx(v.length_squared(), 25.0));
        assert!(approx(v.length(), 5.0));
        // 3-4-12 -> 13 is a known Pythagorean quadruple.
        assert!(approx(Vec3::new(3.0, 4.0, 12.0).length(), 13.0));
    }

    #[test]
    fn normalize_yields_unit_vector() {
        let n = Vec3::new(0.0, 0.0, 5.0)
            .normalize()
            .expect("non-zero length");
        assert!(vec_approx(n, Vec3::new(0.0, 0.0, 1.0)));
        assert!(approx(n.length(), 1.0));

        let m = Vec3::new(3.0, 4.0, 0.0)
            .normalize()
            .expect("non-zero length");
        assert!(approx(m.length(), 1.0));
        assert!(vec_approx(m, Vec3::new(0.6, 0.8, 0.0)));
    }

    #[test]
    fn normalize_zero_is_none() {
        assert!(Vec3::ZERO.normalize().is_none());
    }
}
