//! Data structures and methods for Cartesian Points in 3D.
use crate::types::morton::MortonKey;

pub type PointType = f64;

/// A 3D cartesian point, described by coordinate, a unique global index, and the Morton Key for
/// the octree node in which it lies. Each Point as an associated 'base key', which is its matching
/// Morton encoding at the lowest possible level of discretization (DEEPEST_LEVEL), and an 'encoded key'
/// specifiying its encoding at a given level of discretization. Points also have associated data
#[repr(C)]
#[derive(Clone, Debug, Default)]
pub struct Point {
    /// Physical coordinate in Cartesian space.
    pub coordinate: [PointType; 3],

    /// Global unique index.
    pub global_idx: usize,

    /// Key at finest level of encoding.
    pub base_key: MortonKey,

    /// Key at a given level of encoding, strictly an ancestor of 'base_key'.
    pub encoded_key: MortonKey,

    /// Data associated with this Point.
    pub data: Vec<PointType>,
}

/// Vector of **Points**.
pub type Points = Vec<Point>;
