#![forbid(unsafe_code)]

//! Pure block-placement rules for `FerrumC` v2.
//!
//! Given a [`PlacementContext`] (the held item's block-state, the clicked face,
//! the cursor hit position, the player's yaw, and the target block position) and a
//! [`NeighborQuery`] for fence connectivity, [`compute_placement`] resolves the
//! *correct* block-state id to place — the rotation/facing/half a real client
//! expects — instead of the held item's bare default state.
//!
//! The crate is pure: it depends only on [`ferrumc_math`] (coordinates/facing) and
//! [`ferrumc_registry`] (the block-state catalog). It never hardcodes state ids,
//! bit masks, or property multipliers: every rule builds a small property map and
//! defers the encoding to [`ferrumc_registry::block_state::compute_state_id`], so
//! the numbers always track the vendored `blocks.json` snapshot.
//!
//! ## v0 scope
//!
//! Logs/wood (axis), slabs (type), stairs (facing + half, shape always
//! `straight`), torches (floor vs wall), fences (cardinal connectivity),
//! horizontal-facing blocks (the furnace family), and simple full cubes
//! (unchanged). Out of scope (placed as the default/simple state): waterlogging,
//! doors, beds, rails, signs, banners, stair inner/outer corners, redstone,
//! fluids, fence gates, and double-slab merging.

use std::collections::BTreeMap;

use ferrumc_math::{BlockPos, Direction, Vec3};
use ferrumc_registry::block_state::{compute_state_id, state_id_to_block_name};

/// Input context for a single block placement.
///
/// Built by the authoritative simulation from a serverbound `UseItemOn` plus the
/// player's last-known yaw. All coordinates are typed; `cursor_position` is the
/// click point normalised to `0.0..=1.0` within the targeted block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacementContext {
    /// The block-state id the held item places (its default state).
    pub item_block_state: u32,
    /// The face of the targeted block the player clicked.
    pub clicked_face: Direction,
    /// The cursor hit point inside the targeted block, each component in
    /// `0.0..=1.0`. Only the `y` component is read (slab/stair half).
    pub cursor_position: Vec3,
    /// The player's yaw in degrees (vanilla convention: `0` faces `+Z`/south).
    pub player_yaw: f32,
    /// The block position the new block occupies (already stepped off the clicked
    /// face).
    pub position: BlockPos,
}

/// The outcome of [`compute_placement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementResult {
    /// The computed block-state id to write to the world.
    pub state_id: u32,
    /// The block-state id that was requested (the held item's default state),
    /// retained for diagnostics and metric classification.
    pub requested_state: u32,
    /// The rule that produced [`state_id`](PlacementResult::state_id).
    pub rule: PlacementRule,
}

/// Which placement rule produced a [`PlacementResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlacementRule {
    /// A full cube with no placement-derived properties; placed unchanged.
    SimpleCube,
    /// A log/pillar: `axis` derived from the clicked face.
    AxisFromFace,
    /// A horizontal-facing block: `facing` derived from the player's yaw.
    HorizontalFacing,
    /// A slab: `type` (top/bottom) derived from the clicked face + cursor height.
    Slab,
    /// Stairs: `facing` from yaw and `half` from the clicked face + cursor height;
    /// `shape` is always `straight` in v0.
    StairBasic,
    /// A floor torch: placed unchanged (single state).
    TorchFloor,
    /// A wall torch: `facing` is the clicked face (the block name switches to the
    /// wall variant).
    TorchWall,
    /// A fence: cardinal connectivity resolved via the [`NeighborQuery`].
    FenceLike,
}

/// Queries neighbouring blocks for fence connectivity.
///
/// Implementations can read the live world, a staged write buffer, or a test
/// mock. The trait is `Send + Sync` and has no interior mutability, so a placement
/// computation is deterministic.
pub trait NeighborQuery: Send + Sync {
    /// Returns `true` if the block at `position` should connect to a fence named
    /// `fence_block_name` (the same fence, or a solid full cube). Returns `false`
    /// for air, unloaded, or non-connectable neighbours.
    fn is_fence_connectable(&self, position: BlockPos, fence_block_name: &str) -> bool;
}

/// A [`NeighborQuery`] where nothing connects (an isolated placement).
pub struct NoNeighbors;

impl NeighborQuery for NoNeighbors {
    fn is_fence_connectable(&self, _position: BlockPos, _fence_block_name: &str) -> bool {
        false
    }
}

/// Internal classification of a block's placement behaviour, derived from its
/// resource name.
enum Class {
    SimpleCube,
    Axis,
    HorizontalFacing,
    Slab,
    Stair,
    Torch,
    Fence,
}

/// Floor-torch base names that switch to a wall variant on a side click.
const FLOOR_TORCHES: [(&str, &str); 3] = [
    ("torch", "wall_torch"),
    ("soul_torch", "soul_wall_torch"),
    ("redstone_torch", "redstone_wall_torch"),
];

/// The conservative set of 4-way horizontal-facing, single-block blocks v0 rotates
/// toward the player. Deliberately small to avoid mis-handling beds/doors/6-way
/// facers, which stay [`PlacementRule::SimpleCube`].
const HORIZONTAL_FACING: [&str; 5] = [
    "furnace",
    "blast_furnace",
    "smoker",
    "carved_pumpkin",
    "jack_o_lantern",
];

/// Returns the wall-torch variant name for a floor torch, or `None`.
fn wall_torch_variant(name: &str) -> Option<&'static str> {
    FLOOR_TORCHES
        .iter()
        .find(|(floor, _)| *floor == name)
        .map(|(_, wall)| *wall)
}

/// Classifies a block by name into its placement behaviour.
fn classify(name: &str) -> Class {
    if wall_torch_variant(name).is_some() {
        Class::Torch
    } else if name.ends_with("_slab") {
        Class::Slab
    } else if name.ends_with("_stairs") {
        Class::Stair
    } else if name.ends_with("_fence") {
        // `_fence_gate` is out of scope and never ends with `_fence`.
        Class::Fence
    } else if is_axis_block(name) {
        Class::Axis
    } else if HORIZONTAL_FACING.contains(&name) {
        Class::HorizontalFacing
    } else {
        Class::SimpleCube
    }
}

/// Returns `true` for log/wood-style blocks that rotate via an `axis` property.
fn is_axis_block(name: &str) -> bool {
    // Stripped variants also end with these suffixes (e.g. `stripped_oak_log`).
    name.ends_with("_log")
        || name.ends_with("_wood")
        || name.ends_with("_stem")
        || name.ends_with("_hyphae")
}

/// Derives the nearest cardinal [`Direction`] from a player yaw in degrees.
///
/// Uses the vanilla `Direction.fromYaw` convention: yaw `0` faces `+Z` (south),
/// `90` faces `-X` (west), `180` faces `-Z` (north), `270` faces `+X` (east).
/// Always returns a horizontal direction.
#[must_use]
pub fn yaw_to_cardinal(yaw: f32) -> Direction {
    // Vanilla: from2DDataValue(floor(yaw / 90 + 0.5) & 3); 0=S,1=W,2=N,3=E.
    let idx = (f64::from(yaw) / 90.0 + 0.5).floor() as i32 & 3;
    match idx {
        0 => Direction::South,
        1 => Direction::West,
        2 => Direction::North,
        _ => Direction::East,
    }
}

/// Returns the `axis` value (`"x"`/`"y"`/`"z"`) a log placed against `face` takes.
#[must_use]
pub fn axis_for_face(face: Direction) -> &'static str {
    match face {
        Direction::Up | Direction::Down => "y",
        Direction::North | Direction::South => "z",
        Direction::East | Direction::West => "x",
    }
}

/// Returns the lowercase cardinal name for a horizontal `direction`, or the
/// vertical name for up/down (which is never a valid `facing` value, so the caller
/// falls back to the default state).
#[must_use]
pub fn facing_string(direction: Direction) -> &'static str {
    match direction {
        Direction::North => "north",
        Direction::South => "south",
        Direction::West => "west",
        Direction::East => "east",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

/// Returns `true` if `face` is a horizontal (side) face.
fn is_horizontal(face: Direction) -> bool {
    matches!(
        face,
        Direction::North | Direction::South | Direction::East | Direction::West
    )
}

/// Returns the slab/stair half (`"top"`/`"bottom"`) for a click on `face` at
/// cursor height `cursor_y`.
///
/// A top-face click places a bottom half, a bottom-face click a top half, and a
/// side click splits at the cursor's mid-height.
fn vertical_half(face: Direction, cursor_y: f64) -> &'static str {
    match face {
        Direction::Up => "bottom",
        Direction::Down => "top",
        _ => {
            if cursor_y < 0.5 {
                "bottom"
            } else {
                "top"
            }
        }
    }
}

/// Resolves a fence's cardinal connectivity into a concrete state id.
///
/// For each cardinal neighbour the [`NeighborQuery`] reports as connectable, the
/// matching boolean property is set `true`; unset directions and `waterlogged`
/// inherit the (all-false) default. Shared by [`compute_placement`]'s fence rule
/// and the simulation's neighbour-update loop. Returns `None` only if
/// `fence_name` is not a registry block.
#[must_use]
pub fn compute_fence_connection_state(
    fence_name: &str,
    position: BlockPos,
    neighbors: &dyn NeighborQuery,
) -> Option<u32> {
    let mut props: BTreeMap<&str, &str> = BTreeMap::new();
    for dir in [
        Direction::North,
        Direction::South,
        Direction::East,
        Direction::West,
    ] {
        if neighbors.is_fence_connectable(position.offset(dir), fence_name) {
            props.insert(facing_string(dir), "true");
        }
    }
    compute_state_id(fence_name, &props)
}

/// Computes the block-state id to place for `ctx`, applying the rule for the held
/// block's family.
///
/// Returns `None` only when the held item's block-state is not a registry block
/// (so the caller can fall back to its own safe default). For a recognised block
/// whose property encoding somehow fails, the rule degrades gracefully to the
/// held item's default state (still a valid placement) rather than failing.
///
/// # Examples
///
/// ```
/// use ferrumc_math::{BlockPos, Direction, Vec3};
/// use ferrumc_placement::{compute_placement, NoNeighbors, PlacementContext, PlacementRule};
///
/// // An oak log (default state 137 = axis=y) clicked on an east face -> axis=x.
/// let ctx = PlacementContext {
///     item_block_state: 137,
///     clicked_face: Direction::East,
///     cursor_position: Vec3::new(0.0, 0.5, 0.5),
///     player_yaw: 0.0,
///     position: BlockPos::new(0, 64, 0),
/// };
/// let result = compute_placement(&ctx, &NoNeighbors).expect("oak_log is a block");
/// assert_eq!(result.rule, PlacementRule::AxisFromFace);
/// assert_eq!(result.state_id, 136);
/// ```
#[must_use]
pub fn compute_placement(
    ctx: &PlacementContext,
    neighbors: &dyn NeighborQuery,
) -> Option<PlacementResult> {
    let name = state_id_to_block_name(ctx.item_block_state)?;
    let fallback = ctx.item_block_state;

    let (state_id, rule) = match classify(name) {
        Class::SimpleCube => (fallback, PlacementRule::SimpleCube),
        Class::Axis => {
            let mut props = BTreeMap::new();
            props.insert("axis", axis_for_face(ctx.clicked_face));
            (
                compute_state_id(name, &props).unwrap_or(fallback),
                PlacementRule::AxisFromFace,
            )
        }
        Class::HorizontalFacing => {
            // A horizontal-facing block faces the player: the opposite of where
            // the player looks.
            let facing = yaw_to_cardinal(ctx.player_yaw).opposite();
            let mut props = BTreeMap::new();
            props.insert("facing", facing_string(facing));
            (
                compute_state_id(name, &props).unwrap_or(fallback),
                PlacementRule::HorizontalFacing,
            )
        }
        Class::Slab => {
            let mut props = BTreeMap::new();
            props.insert(
                "type",
                vertical_half(ctx.clicked_face, ctx.cursor_position.y),
            );
            (
                compute_state_id(name, &props).unwrap_or(fallback),
                PlacementRule::Slab,
            )
        }
        Class::Stair => {
            // Stairs face the way the player looks (not the opposite).
            let facing = yaw_to_cardinal(ctx.player_yaw);
            let mut props = BTreeMap::new();
            props.insert("facing", facing_string(facing));
            props.insert(
                "half",
                vertical_half(ctx.clicked_face, ctx.cursor_position.y),
            );
            props.insert("shape", "straight");
            (
                compute_state_id(name, &props).unwrap_or(fallback),
                PlacementRule::StairBasic,
            )
        }
        Class::Torch => {
            if is_horizontal(ctx.clicked_face) {
                // A side click becomes a wall torch facing the clicked face.
                let wall = wall_torch_variant(name).unwrap_or(name);
                let mut props = BTreeMap::new();
                props.insert("facing", facing_string(ctx.clicked_face));
                (
                    compute_state_id(wall, &props).unwrap_or(fallback),
                    PlacementRule::TorchWall,
                )
            } else {
                // A top/bottom click keeps the floor torch (single state).
                (fallback, PlacementRule::TorchFloor)
            }
        }
        Class::Fence => (
            compute_fence_connection_state(name, ctx.position, neighbors).unwrap_or(fallback),
            PlacementRule::FenceLike,
        ),
    };

    Some(PlacementResult {
        state_id,
        requested_state: ctx.item_block_state,
        rule,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock neighbour query: every position in its set is connectable.
    struct MockNeighbors {
        connectable: Vec<BlockPos>,
    }

    impl NeighborQuery for MockNeighbors {
        fn is_fence_connectable(&self, position: BlockPos, _fence_block_name: &str) -> bool {
            self.connectable.contains(&position)
        }
    }

    fn ctx(item: u32, face: Direction, cursor_y: f64, yaw: f32) -> PlacementContext {
        PlacementContext {
            item_block_state: item,
            clicked_face: face,
            cursor_position: Vec3::new(0.5, cursor_y, 0.5),
            player_yaw: yaw,
            position: BlockPos::new(0, 64, 0),
        }
    }

    const OAK_LOG: u32 = 137;
    const OAK_SLAB: u32 = 12054;
    const OAK_STAIRS: u32 = 2949;
    const TORCH: u32 = 2401;
    const OAK_FENCE: u32 = 6027;
    const STONE: u32 = 1;
    const FURNACE: u32 = 4359;

    #[test]
    fn yaw_to_cardinal_sectors() {
        // Vanilla fromYaw: 0=S, 90=W, 180=N, 270=E, boundary at +45 rolls forward.
        assert_eq!(yaw_to_cardinal(0.0), Direction::South);
        assert_eq!(yaw_to_cardinal(44.0), Direction::South);
        assert_eq!(yaw_to_cardinal(45.0), Direction::West);
        assert_eq!(yaw_to_cardinal(90.0), Direction::West);
        assert_eq!(yaw_to_cardinal(180.0), Direction::North);
        assert_eq!(yaw_to_cardinal(270.0), Direction::East);
        assert_eq!(yaw_to_cardinal(315.0), Direction::South);
        assert_eq!(yaw_to_cardinal(-90.0), Direction::East);
        assert_eq!(yaw_to_cardinal(360.0), Direction::South);
    }

    #[test]
    fn log_axis_from_each_face() {
        let axis = |face| compute_placement(&ctx(OAK_LOG, face, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(axis(Direction::Up).state_id, 137); // y
        assert_eq!(axis(Direction::Down).state_id, 137); // y
        assert_eq!(axis(Direction::North).state_id, 138); // z
        assert_eq!(axis(Direction::South).state_id, 138); // z
        assert_eq!(axis(Direction::East).state_id, 136); // x
        assert_eq!(axis(Direction::West).state_id, 136); // x
        assert_eq!(axis(Direction::East).rule, PlacementRule::AxisFromFace);
    }

    #[test]
    fn slab_top_bottom_from_cursor_and_face() {
        // Top-face click -> bottom slab regardless of cursor.
        let top_face =
            compute_placement(&ctx(OAK_SLAB, Direction::Up, 0.9, 0.0), &NoNeighbors).unwrap();
        assert_eq!(top_face.state_id, 12054); // bottom (default)
        assert_eq!(top_face.rule, PlacementRule::Slab);
        // Bottom-face click -> top slab.
        let bottom_face =
            compute_placement(&ctx(OAK_SLAB, Direction::Down, 0.1, 0.0), &NoNeighbors).unwrap();
        assert_eq!(bottom_face.state_id, 12052); // top
                                                 // Side click, lower half -> bottom; upper half -> top.
        let side_low =
            compute_placement(&ctx(OAK_SLAB, Direction::North, 0.2, 0.0), &NoNeighbors).unwrap();
        assert_eq!(side_low.state_id, 12054);
        let side_high =
            compute_placement(&ctx(OAK_SLAB, Direction::North, 0.8, 0.0), &NoNeighbors).unwrap();
        assert_eq!(side_high.state_id, 12052);
    }

    #[test]
    fn stairs_facing_from_yaw_and_half_from_cursor() {
        // yaw 180 -> north, top-face click -> bottom half = default 2949.
        let north_bottom =
            compute_placement(&ctx(OAK_STAIRS, Direction::Up, 0.0, 180.0), &NoNeighbors).unwrap();
        assert_eq!(north_bottom.state_id, 2949);
        assert_eq!(north_bottom.rule, PlacementRule::StairBasic);
        // yaw 90 -> west, bottom-face click -> top half = 2979.
        let west_top =
            compute_placement(&ctx(OAK_STAIRS, Direction::Down, 0.0, 90.0), &NoNeighbors).unwrap();
        assert_eq!(west_top.state_id, 2979);
        // yaw 0 -> south, side click lower half -> bottom = 2969.
        let south_bottom =
            compute_placement(&ctx(OAK_STAIRS, Direction::East, 0.2, 0.0), &NoNeighbors).unwrap();
        assert_eq!(south_bottom.state_id, 2969);
    }

    #[test]
    fn torch_floor_vs_wall() {
        // Top click keeps the floor torch.
        let floor = compute_placement(&ctx(TORCH, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(floor.state_id, 2401);
        assert_eq!(floor.rule, PlacementRule::TorchFloor);
        // Side clicks become a wall torch facing the clicked face.
        let north =
            compute_placement(&ctx(TORCH, Direction::North, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(north.state_id, 2402);
        assert_eq!(north.rule, PlacementRule::TorchWall);
        let south =
            compute_placement(&ctx(TORCH, Direction::South, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(south.state_id, 2403);
        let west = compute_placement(&ctx(TORCH, Direction::West, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(west.state_id, 2404);
        let east = compute_placement(&ctx(TORCH, Direction::East, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(east.state_id, 2405);
    }

    #[test]
    fn fence_connects_to_neighbors() {
        // Isolated fence: all-false default.
        let isolated =
            compute_placement(&ctx(OAK_FENCE, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(isolated.state_id, 6027);
        assert_eq!(isolated.rule, PlacementRule::FenceLike);

        // A fence at (0,64,0) with a connectable neighbour to the north.
        let north_only = MockNeighbors {
            connectable: vec![BlockPos::new(0, 64, -1)],
        };
        let connected =
            compute_placement(&ctx(OAK_FENCE, Direction::Up, 0.5, 0.0), &north_only).unwrap();
        assert_eq!(connected.state_id, 6019);

        // North + east connectable.
        let north_east = MockNeighbors {
            connectable: vec![BlockPos::new(0, 64, -1), BlockPos::new(1, 64, 0)],
        };
        let two = compute_placement(&ctx(OAK_FENCE, Direction::Up, 0.5, 0.0), &north_east).unwrap();
        assert_eq!(two.state_id, 6003);
    }

    #[test]
    fn horizontal_facing_faces_player() {
        // yaw 0 -> player faces south -> furnace faces north (default 4359).
        let furnace =
            compute_placement(&ctx(FURNACE, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(furnace.state_id, 4359);
        assert_eq!(furnace.rule, PlacementRule::HorizontalFacing);
    }

    #[test]
    fn simple_cube_is_unchanged() {
        let stone =
            compute_placement(&ctx(STONE, Direction::North, 0.9, 123.0), &NoNeighbors).unwrap();
        assert_eq!(stone.state_id, 1);
        assert_eq!(stone.rule, PlacementRule::SimpleCube);
        assert_eq!(stone.requested_state, 1);
    }

    #[test]
    fn unknown_item_state_is_none() {
        assert!(compute_placement(&ctx(u32::MAX, Direction::Up, 0.5, 0.0), &NoNeighbors).is_none());
    }
}
