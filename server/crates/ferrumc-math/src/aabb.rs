//! Axis-aligned bounding boxes built from [`Vec3`] corners.

use crate::Vec3;

/// An axis-aligned bounding box (AABB) defined by its minimum and maximum
/// corners.
///
/// The box upholds the invariant `min <= max` on every axis; all constructors
/// normalize their inputs so this always holds, even if the corners are passed
/// swapped. Both [`Aabb::contains`] and [`Aabb::intersects`] treat the surface
/// as **inclusive**: a point lying exactly on a face is contained, and two
/// boxes that only touch along a face or edge are considered intersecting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    min: Vec3,
    max: Vec3,
}

impl Aabb {
    /// Creates a box spanning two corners.
    ///
    /// The corners are normalized per axis, so the result is well-formed even
    /// if `min` and `max` are swapped or mixed. Alias of
    /// [`Aabb::from_corners`].
    pub fn new(min: Vec3, max: Vec3) -> Self {
        Self::from_corners(min, max)
    }

    /// Creates a box from any two opposite corners, taking the per-axis minimum
    /// and maximum so the result is well-formed regardless of corner order.
    pub fn from_corners(a: Vec3, b: Vec3) -> Self {
        Self {
            min: Vec3::new(a.x.min(b.x), a.y.min(b.y), a.z.min(b.z)),
            max: Vec3::new(a.x.max(b.x), a.y.max(b.y), a.z.max(b.z)),
        }
    }

    /// Creates a box centered at `center` extending `half_extents` along each
    /// axis in both directions. Negative half-extents are normalized.
    pub fn from_center_half_extents(center: Vec3, half_extents: Vec3) -> Self {
        Self::from_corners(center - half_extents, center + half_extents)
    }

    /// Returns the minimum corner.
    pub fn min(&self) -> Vec3 {
        self.min
    }

    /// Returns the maximum corner.
    pub fn max(&self) -> Vec3 {
        self.max
    }

    /// Returns `true` if `point` lies inside or on the surface of the box.
    ///
    /// Containment is inclusive on every face (see the type-level docs).
    pub fn contains(&self, point: Vec3) -> bool {
        self.min.x <= point.x
            && point.x <= self.max.x
            && self.min.y <= point.y
            && point.y <= self.max.y
            && self.min.z <= point.z
            && point.z <= self.max.z
    }

    /// Returns `true` if `self` and `other` overlap, including the case where
    /// they only touch along a face or edge.
    pub fn intersects(&self, other: &Aabb) -> bool {
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
            && self.min.z <= other.max.z
            && self.max.z >= other.min.z
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_box() -> Aabb {
        Aabb::new(Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0))
    }

    #[test]
    fn from_corners_normalizes_swapped_corners() {
        let a = Aabb::from_corners(Vec3::new(2.0, 2.0, 2.0), Vec3::ZERO);
        assert_eq!(a.min(), Vec3::ZERO);
        assert_eq!(a.max(), Vec3::new(2.0, 2.0, 2.0));
    }

    #[test]
    fn from_center_half_extents_spans_correctly() {
        let a = Aabb::from_center_half_extents(Vec3::new(1.0, 1.0, 1.0), Vec3::new(2.0, 3.0, 4.0));
        assert_eq!(a.min(), Vec3::new(-1.0, -2.0, -3.0));
        assert_eq!(a.max(), Vec3::new(3.0, 4.0, 5.0));
    }

    #[test]
    fn contains_interior_and_inclusive_faces() {
        let b = unit_box();
        assert!(b.contains(Vec3::new(0.5, 0.5, 0.5)));
        // Faces, edges, and corners are all inclusive.
        assert!(b.contains(Vec3::ZERO));
        assert!(b.contains(Vec3::new(1.0, 1.0, 1.0)));
        assert!(b.contains(Vec3::new(0.0, 0.5, 1.0)));
        // Clearly outside on a single axis.
        assert!(!b.contains(Vec3::new(1.5, 0.5, 0.5)));
        assert!(!b.contains(Vec3::new(0.5, -0.001, 0.5)));
    }

    #[test]
    fn intersects_overlapping_and_touching() {
        let b = unit_box();
        // Genuine volumetric overlap.
        assert!(b.intersects(&Aabb::new(
            Vec3::new(0.5, 0.5, 0.5),
            Vec3::new(2.0, 2.0, 2.0)
        )));
        // Self-intersection.
        assert!(b.intersects(&b));
        // Face touching at x == 1.0 counts as intersecting (inclusive).
        assert!(b.intersects(&Aabb::new(
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(2.0, 1.0, 1.0)
        )));
        // Edge touching at the (1,1,*) edge.
        assert!(b.intersects(&Aabb::new(
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(2.0, 2.0, 1.0)
        )));
    }

    #[test]
    fn intersects_disjoint_is_false() {
        let b = unit_box();
        // Separated by a clear gap on x.
        assert!(!b.intersects(&Aabb::new(
            Vec3::new(1.001, 0.0, 0.0),
            Vec3::new(2.0, 1.0, 1.0)
        )));
        // Overlapping on x and y but disjoint on z.
        assert!(!b.intersects(&Aabb::new(
            Vec3::new(0.0, 0.0, 5.0),
            Vec3::new(1.0, 1.0, 6.0)
        )));
    }
}
