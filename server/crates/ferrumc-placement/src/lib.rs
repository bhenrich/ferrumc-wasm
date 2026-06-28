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
//! ## Scope
//!
//! Single-state placements only — every rule resolves the held item to exactly
//! one block-state at the one caller-chosen position. Covered families:
//!
//! - Logs/wood — `axis` from the clicked face.
//! - Slabs — `type` (top/bottom) from the clicked face + cursor height.
//! - Stairs — `facing` from yaw, `half` from the face + cursor, and the vanilla
//!   auto-corner `shape` (`inner_left`/`inner_right`/`outer_left`/`outer_right`)
//!   derived from neighbouring stairs.
//! - Torches — floor vs wall (wall variant `facing` = the clicked face).
//! - Fences — cardinal connectivity via the [`NeighborQuery`].
//! - Trapdoors — `facing` (clicked side, else player yaw) + `half` from the
//!   face/cursor; `open`/`powered` inherit `false`.
//! - Fence gates — `facing` from yaw, `in_wall` when flanked by walls.
//! - Buttons & levers — `face` (floor/wall/ceiling) + `facing`.
//! - Anvils — `facing` perpendicular (clockwise) to the player.
//! - End rods — 6-way `facing` from the clicked face.
//! - Horizontal facers — the furnace family, carved pumpkins, and observers
//!   (`facing` toward the player).
//! - Simple full cubes — placed unchanged.
//!
//! Out of scope (placed as the default/simple state): waterlogging, rails,
//! signs, banners, redstone wiring, and fluids. Out of *reach* of this engine
//! entirely — they need a multi-position placement path, not just a richer state
//! rule — are doors (upper+lower), beds (head+foot), and double-slab merging (a
//! write relocated onto the clicked slab); see [`PlacementResult`].

use std::collections::BTreeMap;

use ferrumc_math::{BlockPos, Direction, Vec3};
use ferrumc_registry::block_state::{block_metadata, compute_state_id, state_id_to_block_name};

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
///
/// A result is intentionally *single-state, single-position*: it names one
/// block-state for the one position the caller already chose. Families that need
/// to write a second block (doors' upper half, beds' foot) or to *relocate* the
/// write onto the clicked cell (double-slab merging) cannot be expressed here —
/// they require the consumer's placement→mutation path to carry more than one
/// target, which is a deliberately out-of-scope change.
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
    /// Stairs: `facing` from yaw, `half` from the clicked face + cursor height,
    /// and the auto-corner `shape` derived from neighbouring stairs (the vanilla
    /// `straight`/`inner_*`/`outer_*` behaviour) via the [`NeighborQuery`].
    StairBasic,
    /// A floor torch: placed unchanged (single state).
    TorchFloor,
    /// A wall torch: `facing` is the clicked face (the block name switches to the
    /// wall variant).
    TorchWall,
    /// A fence: cardinal connectivity resolved via the [`NeighborQuery`].
    FenceLike,
    /// A trapdoor: `facing` (the clicked side, or the player's yaw for a
    /// top/bottom click) and `half` from the face + cursor height.
    Trapdoor,
    /// A fence gate: `facing` from yaw and `in_wall` when flanked by walls
    /// (resolved via the [`NeighborQuery`]).
    FenceGate,
    /// A button or lever: `face` (floor/wall/ceiling) from the clicked face and
    /// `facing` from the clicked side or the player's yaw.
    FaceAttached,
    /// An anvil: `facing` perpendicular (clockwise) to the player's yaw.
    AnvilFacing,
    /// An end rod: 6-way `facing` taken from the clicked face.
    EndRod,
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

    /// Returns the raw block-state id at `position`, or `None` when the cell is
    /// air, unloaded, or otherwise unreadable.
    ///
    /// Drives the neighbour-dependent rules: stair auto-corner `shape` and
    /// fence-gate `in_wall`. The default returns `None`, so an implementation
    /// that cannot read the world (or has not opted in) degrades those rules to
    /// their neighbour-free result — straight stairs and `in_wall=false` — rather
    /// than failing. An implementation backed by live chunks should override it.
    fn block_state_at(&self, _position: BlockPos) -> Option<u32> {
        None
    }
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
    Trapdoor,
    FenceGate,
    /// Buttons and levers — `face` (floor/wall/ceiling) plus `facing`.
    FaceAttached,
    Anvil,
    EndRod,
}

/// Floor-torch base names that switch to a wall variant on a side click.
const FLOOR_TORCHES: [(&str, &str); 3] = [
    ("torch", "wall_torch"),
    ("soul_torch", "soul_wall_torch"),
    ("redstone_torch", "redstone_wall_torch"),
];

/// The set of single-block blocks rotated to face the player (`facing` =
/// opposite of where the player looks). Deliberately explicit to avoid
/// mis-handling beds/doors, which stay [`PlacementRule::SimpleCube`]. `observer`
/// is 6-way in vanilla but, lacking pitch here, is resolved to a horizontal
/// `facing` toward the player — correct for the common eye-level placement.
const HORIZONTAL_FACING: [&str; 6] = [
    "furnace",
    "blast_furnace",
    "smoker",
    "carved_pumpkin",
    "jack_o_lantern",
    "observer",
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
    } else if name.ends_with("_trapdoor") {
        Class::Trapdoor
    } else if name.ends_with("_fence_gate") {
        // Checked before `_fence`; `_fence_gate` ends with `_gate`, not `_fence`.
        Class::FenceGate
    } else if name.ends_with("_slab") {
        Class::Slab
    } else if name.ends_with("_stairs") {
        Class::Stair
    } else if name.ends_with("_button") {
        Class::FaceAttached
    } else if name.ends_with("_fence") {
        Class::Fence
    } else if name == "lever" {
        Class::FaceAttached
    } else if is_anvil(name) {
        Class::Anvil
    } else if name == "end_rod" {
        Class::EndRod
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

/// Returns `true` for the three anvil damage variants, which share a single
/// `facing` property.
fn is_anvil(name: &str) -> bool {
    matches!(name, "anvil" | "chipped_anvil" | "damaged_anvil")
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

/// Returns the cardinal direction one quarter-turn clockwise (viewed from above)
/// from `dir`; vertical directions are returned unchanged (never used).
fn rotate_clockwise(dir: Direction) -> Direction {
    match dir {
        Direction::North => Direction::East,
        Direction::East => Direction::South,
        Direction::South => Direction::West,
        Direction::West => Direction::North,
        other => other,
    }
}

/// Returns the cardinal direction one quarter-turn counter-clockwise (viewed from
/// above) from `dir`; vertical directions are returned unchanged (never used).
fn rotate_counterclockwise(dir: Direction) -> Direction {
    match dir {
        Direction::North => Direction::West,
        Direction::West => Direction::South,
        Direction::South => Direction::East,
        Direction::East => Direction::North,
        other => other,
    }
}

/// Returns `true` if `a` and `b` lie on the same horizontal axis (both north/south
/// or both east/west). Vertical directions share a third axis.
fn same_axis(a: Direction, b: Direction) -> bool {
    fn axis(d: Direction) -> u8 {
        match d {
            Direction::North | Direction::South => 0,
            Direction::East | Direction::West => 1,
            Direction::Up | Direction::Down => 2,
        }
    }
    axis(a) == axis(b)
}

/// Parses a cardinal `facing` value (`"north"`/`"south"`/`"west"`/`"east"`) into a
/// [`Direction`], or `None` for any other string.
fn cardinal_from_name(name: &str) -> Option<Direction> {
    match name {
        "north" => Some(Direction::North),
        "south" => Some(Direction::South),
        "west" => Some(Direction::West),
        "east" => Some(Direction::East),
        _ => None,
    }
}

/// Decodes the value of property `prop` from `state_id` — the single-property
/// inverse of [`compute_state_id`].
///
/// Returns `None` if `state_id` is not a registry block or the block has no such
/// property. Used to read a neighbouring stair's `facing`/`half`.
fn state_property(state_id: u32, prop: &str) -> Option<&'static str> {
    let meta = block_metadata(state_id_to_block_name(state_id)?)?;
    let pi = meta.properties.iter().position(|p| p.name == prop)?;
    // Place value of `prop`: the product of every later property's cardinality.
    let later: u32 = meta.properties[pi + 1..]
        .iter()
        .map(|p| p.cardinality as u32)
        .product();
    let card = meta.properties[pi].cardinality as u32;
    let digit = ((state_id - meta.min_state) / later) % card;
    meta.properties[pi].values.get(digit as usize).copied()
}

/// Resolves the stair at `position` into its `(facing, half)`, or `None` when the
/// neighbour is absent, not a stair, or missing a cardinal `facing`.
fn neighbor_stair(
    neighbors: &dyn NeighborQuery,
    position: BlockPos,
) -> Option<(Direction, &'static str)> {
    let state = neighbors.block_state_at(position)?;
    let name = state_id_to_block_name(state)?;
    if !name.ends_with("_stairs") {
        return None;
    }
    let facing = cardinal_from_name(state_property(state, "facing")?)?;
    let half = state_property(state, "half")?;
    Some((facing, half))
}

/// Vanilla `canTakeShape`: the perpendicular neighbour at `position` only blocks a
/// corner when it is itself a stair already aligned the same way (same `facing`
/// and `half`). Anything else — air, a non-stair, or a differently-oriented stair
/// — permits the corner.
fn stair_can_take_shape(
    facing: Direction,
    half: &str,
    position: BlockPos,
    neighbors: &dyn NeighborQuery,
) -> bool {
    match neighbor_stair(neighbors, position) {
        Some((nf, nh)) => nf != facing || nh != half,
        None => true,
    }
}

/// Derives a stair's auto-corner `shape` from its neighbours, mirroring vanilla
/// `StairBlock`.
///
/// A perpendicular stair of the same `half` directly in front (the way these
/// stairs face) forms an outer corner; one directly behind forms an inner corner.
/// `left`/`right` is decided by whether the neighbour faces counter-clockwise from
/// this stair. A neighbour already aligned the same way on the turning side
/// suppresses the corner (`stair_can_take_shape`). Otherwise the shape is
/// `straight`.
fn stair_shape(
    facing: Direction,
    half: &str,
    position: BlockPos,
    neighbors: &dyn NeighborQuery,
) -> &'static str {
    // Outer corner: a perpendicular same-half stair in the faced direction.
    if let Some((nf, nh)) = neighbor_stair(neighbors, position.offset(facing)) {
        if nh == half
            && !same_axis(nf, facing)
            && stair_can_take_shape(facing, half, position.offset(nf.opposite()), neighbors)
        {
            return if nf == rotate_counterclockwise(facing) {
                "outer_left"
            } else {
                "outer_right"
            };
        }
    }
    // Inner corner: a perpendicular same-half stair directly behind.
    if let Some((bf, bh)) = neighbor_stair(neighbors, position.offset(facing.opposite())) {
        if bh == half
            && !same_axis(bf, facing)
            && stair_can_take_shape(facing, half, position.offset(bf), neighbors)
        {
            return if bf == rotate_counterclockwise(facing) {
                "inner_left"
            } else {
                "inner_right"
            };
        }
    }
    "straight"
}

/// Returns `true` if the block at `position` is a wall (`*_wall`), the family a
/// fence gate lowers itself into via `in_wall`.
fn has_wall(neighbors: &dyn NeighborQuery, position: BlockPos) -> bool {
    neighbors
        .block_state_at(position)
        .and_then(state_id_to_block_name)
        .is_some_and(|name| name.ends_with("_wall"))
}

/// Computes a fence gate's `in_wall`: `true` when a wall flanks either side
/// perpendicular to the gate's `facing` (matching vanilla, which lowers the gate
/// to line up with adjacent walls).
fn fence_gate_in_wall(
    facing: Direction,
    position: BlockPos,
    neighbors: &dyn NeighborQuery,
) -> bool {
    let (left, right) = match facing {
        Direction::North | Direction::South => (Direction::West, Direction::East),
        _ => (Direction::North, Direction::South),
    };
    has_wall(neighbors, position.offset(left)) || has_wall(neighbors, position.offset(right))
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
        Class::Axis => place_axis(name, fallback, ctx),
        Class::HorizontalFacing => place_horizontal_facing(name, fallback, ctx),
        Class::Slab => place_slab(name, fallback, ctx),
        Class::Stair => place_stair(name, fallback, ctx, neighbors),
        Class::Torch => place_torch(name, fallback, ctx),
        Class::Fence => (
            compute_fence_connection_state(name, ctx.position, neighbors).unwrap_or(fallback),
            PlacementRule::FenceLike,
        ),
        Class::Trapdoor => place_trapdoor(name, fallback, ctx),
        Class::FenceGate => place_fence_gate(name, fallback, ctx, neighbors),
        Class::FaceAttached => place_face_attached(name, fallback, ctx),
        Class::Anvil => place_anvil(name, fallback, ctx),
        Class::EndRod => place_end_rod(name, fallback, ctx),
    };

    Some(PlacementResult {
        state_id,
        requested_state: ctx.item_block_state,
        rule,
    })
}

/// Resolves the state id for one computed property set, degrading to `fallback`
/// (the held default) if the encoding fails — always a valid placement.
fn encode(name: &str, props: &BTreeMap<&str, &str>, fallback: u32) -> u32 {
    compute_state_id(name, props).unwrap_or(fallback)
}

/// A log/pillar: `axis` from the clicked face.
fn place_axis(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let mut props = BTreeMap::new();
    props.insert("axis", axis_for_face(ctx.clicked_face));
    (encode(name, &props, fallback), PlacementRule::AxisFromFace)
}

/// A horizontal facer (furnace family, observer, …): `facing` toward the player,
/// the opposite of where the player looks.
fn place_horizontal_facing(
    name: &str,
    fallback: u32,
    ctx: &PlacementContext,
) -> (u32, PlacementRule) {
    let facing = yaw_to_cardinal(ctx.player_yaw).opposite();
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(facing));
    (
        encode(name, &props, fallback),
        PlacementRule::HorizontalFacing,
    )
}

/// A slab: `type` (top/bottom) from the clicked face + cursor height.
fn place_slab(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let mut props = BTreeMap::new();
    props.insert(
        "type",
        vertical_half(ctx.clicked_face, ctx.cursor_position.y),
    );
    (encode(name, &props, fallback), PlacementRule::Slab)
}

/// Stairs: `facing` from the way the player looks (not the opposite), `half` from
/// the face + cursor height, and the auto-corner `shape` from neighbouring stairs.
fn place_stair(
    name: &str,
    fallback: u32,
    ctx: &PlacementContext,
    neighbors: &dyn NeighborQuery,
) -> (u32, PlacementRule) {
    let facing = yaw_to_cardinal(ctx.player_yaw);
    let half = vertical_half(ctx.clicked_face, ctx.cursor_position.y);
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(facing));
    props.insert("half", half);
    props.insert("shape", stair_shape(facing, half, ctx.position, neighbors));
    (encode(name, &props, fallback), PlacementRule::StairBasic)
}

/// A torch: a side click becomes a wall torch facing the clicked face; a
/// top/bottom click keeps the single-state floor torch.
fn place_torch(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    if is_horizontal(ctx.clicked_face) {
        let wall = wall_torch_variant(name).unwrap_or(name);
        let mut props = BTreeMap::new();
        props.insert("facing", facing_string(ctx.clicked_face));
        (encode(wall, &props, fallback), PlacementRule::TorchWall)
    } else {
        (fallback, PlacementRule::TorchFloor)
    }
}

/// A trapdoor: `facing` is the clicked side for a horizontal click, otherwise the
/// player's yaw (so it faces the player); `half` from the face + cursor height.
/// `open`/`powered`/`waterlogged` inherit their `false` defaults.
fn place_trapdoor(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let facing = if is_horizontal(ctx.clicked_face) {
        ctx.clicked_face
    } else {
        yaw_to_cardinal(ctx.player_yaw).opposite()
    };
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(facing));
    props.insert(
        "half",
        vertical_half(ctx.clicked_face, ctx.cursor_position.y),
    );
    (encode(name, &props, fallback), PlacementRule::Trapdoor)
}

/// A fence gate: `facing` the way the player looks, `in_wall` when flanked by
/// walls. `open`/`powered` inherit their `false` defaults (no redstone here).
fn place_fence_gate(
    name: &str,
    fallback: u32,
    ctx: &PlacementContext,
    neighbors: &dyn NeighborQuery,
) -> (u32, PlacementRule) {
    let facing = yaw_to_cardinal(ctx.player_yaw);
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(facing));
    if fence_gate_in_wall(facing, ctx.position, neighbors) {
        props.insert("in_wall", "true");
    }
    (encode(name, &props, fallback), PlacementRule::FenceGate)
}

/// A button or lever: `face` is `floor`/`ceiling` for a top/bottom click (with
/// `facing` from the player's yaw) or `wall` for a side click (with `facing` the
/// clicked side). `powered` inherits its `false` default.
fn place_face_attached(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let (face, facing) = match ctx.clicked_face {
        Direction::Up => ("floor", yaw_to_cardinal(ctx.player_yaw)),
        Direction::Down => ("ceiling", yaw_to_cardinal(ctx.player_yaw)),
        side => ("wall", side),
    };
    let mut props = BTreeMap::new();
    props.insert("face", face);
    props.insert("facing", facing_string(facing));
    (encode(name, &props, fallback), PlacementRule::FaceAttached)
}

/// An anvil: `facing` perpendicular (clockwise) to the player, so its long axis
/// runs left-to-right across the player's view (matching vanilla).
fn place_anvil(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let facing = rotate_clockwise(yaw_to_cardinal(ctx.player_yaw));
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(facing));
    (encode(name, &props, fallback), PlacementRule::AnvilFacing)
}

/// An end rod: 6-way `facing` taken directly from the clicked face, so it points
/// out of the surface it was placed against.
fn place_end_rod(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(ctx.clicked_face));
    (encode(name, &props, fallback), PlacementRule::EndRod)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock neighbour query: positions in `connectable` connect for fences, and
    /// `states` supplies explicit neighbour block-state ids (stairs, walls, …).
    #[derive(Default)]
    struct MockNeighbors {
        connectable: Vec<BlockPos>,
        states: Vec<(BlockPos, u32)>,
    }

    impl NeighborQuery for MockNeighbors {
        fn is_fence_connectable(&self, position: BlockPos, _fence_block_name: &str) -> bool {
            self.connectable.contains(&position)
        }

        fn block_state_at(&self, position: BlockPos) -> Option<u32> {
            self.states
                .iter()
                .find(|(pos, _)| *pos == position)
                .map(|(_, state)| *state)
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
    const OAK_TRAPDOOR: u32 = 6155;
    const OAK_FENCE_GATE: u32 = 7375;
    const STONE_BUTTON: u32 = 5935;
    const LEVER: u32 = 5811;
    const ANVIL: u32 = 9916;
    const END_ROD: u32 = 13361;
    const OBSERVER: u32 = 13578;
    const CARVED_PUMPKIN: u32 = 6045;
    const COBBLESTONE_WALL: u32 = 8706;

    /// Bottom-half oak stairs facing a given cardinal (`shape=straight`), used as
    /// neighbours when testing corner-shape derivation.
    const OAK_STAIRS_NORTH_BOTTOM: u32 = 2949;
    const OAK_STAIRS_SOUTH_BOTTOM: u32 = 2969;
    const OAK_STAIRS_WEST_BOTTOM: u32 = 2989;
    const OAK_STAIRS_EAST_BOTTOM: u32 = 3009;
    const OAK_STAIRS_WEST_TOP: u32 = 2979;

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
            ..Default::default()
        };
        let connected =
            compute_placement(&ctx(OAK_FENCE, Direction::Up, 0.5, 0.0), &north_only).unwrap();
        assert_eq!(connected.state_id, 6019);

        // North + east connectable.
        let north_east = MockNeighbors {
            connectable: vec![BlockPos::new(0, 64, -1), BlockPos::new(1, 64, 0)],
            ..Default::default()
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

    #[test]
    fn trapdoor_facing_and_half() {
        let place = |face, cursor_y, yaw| {
            compute_placement(&ctx(OAK_TRAPDOOR, face, cursor_y, yaw), &NoNeighbors).unwrap()
        };
        // Top-face click, yaw 0 (player faces south) -> faces north, bottom half =
        // the default state.
        let top = place(Direction::Up, 0.5, 0.0);
        assert_eq!(top.state_id, 6155);
        assert_eq!(top.rule, PlacementRule::Trapdoor);
        // Bottom-face click -> top half, still facing north.
        assert_eq!(place(Direction::Down, 0.5, 0.0).state_id, 6147);
        // Top-face click, yaw 90 (player faces west) -> faces east, bottom half.
        assert_eq!(place(Direction::Up, 0.5, 90.0).state_id, 6203);
        // Side clicks attach to the clicked face; cursor height picks the half.
        assert_eq!(place(Direction::East, 0.2, 0.0).state_id, 6203); // east/bottom
        assert_eq!(place(Direction::East, 0.8, 0.0).state_id, 6195); // east/top
        assert_eq!(place(Direction::South, 0.9, 0.0).state_id, 6163); // south/top
        assert_eq!(place(Direction::West, 0.1, 0.0).state_id, 6187); // west/bottom
    }

    #[test]
    fn fence_gate_facing_from_yaw() {
        let place = |yaw| {
            compute_placement(&ctx(OAK_FENCE_GATE, Direction::Up, 0.5, yaw), &NoNeighbors).unwrap()
        };
        // The gate faces the way the player looks (not the opposite).
        let south = place(0.0);
        assert_eq!(south.state_id, 7383); // facing south
        assert_eq!(south.rule, PlacementRule::FenceGate);
        assert_eq!(place(180.0).state_id, 7375); // facing north (default)
        assert_eq!(place(90.0).state_id, 7391); // facing west
        assert_eq!(place(270.0).state_id, 7399); // facing east
    }

    #[test]
    fn fence_gate_in_wall_when_flanked() {
        // Facing north (yaw 180): the perpendicular sides are west/east. A wall on
        // either of those lowers the gate (in_wall=true).
        let east_wall = MockNeighbors {
            states: vec![(BlockPos::new(1, 64, 0), COBBLESTONE_WALL)],
            ..Default::default()
        };
        let lowered =
            compute_placement(&ctx(OAK_FENCE_GATE, Direction::Up, 0.5, 180.0), &east_wall).unwrap();
        assert_eq!(lowered.state_id, 7371); // north, in_wall=true

        // Facing east (yaw 270): perpendicular sides are north/south.
        let north_wall = MockNeighbors {
            states: vec![(BlockPos::new(0, 64, -1), COBBLESTONE_WALL)],
            ..Default::default()
        };
        let lowered_east =
            compute_placement(&ctx(OAK_FENCE_GATE, Direction::Up, 0.5, 270.0), &north_wall)
                .unwrap();
        assert_eq!(lowered_east.state_id, 7395); // east, in_wall=true

        // A wall on the parallel side (north, in line with the facing) does NOT
        // lower a north-facing gate.
        let parallel_wall = MockNeighbors {
            states: vec![(BlockPos::new(0, 64, -1), COBBLESTONE_WALL)],
            ..Default::default()
        };
        let unaffected = compute_placement(
            &ctx(OAK_FENCE_GATE, Direction::Up, 0.5, 180.0),
            &parallel_wall,
        )
        .unwrap();
        assert_eq!(unaffected.state_id, 7375); // north, in_wall=false (default)
    }

    /// Places a north-facing bottom stair at (0,64,0) and reads the resulting
    /// `shape` against the supplied neighbour states.
    fn place_corner_stair(states: Vec<(BlockPos, u32)>) -> u32 {
        let neighbors = MockNeighbors {
            states,
            ..Default::default()
        };
        // yaw 180 -> facing north; Up click -> bottom half.
        compute_placement(&ctx(OAK_STAIRS, Direction::Up, 0.0, 180.0), &neighbors)
            .unwrap()
            .state_id
    }

    #[test]
    fn stairs_corner_shapes_from_neighbors() {
        let front = BlockPos::new(0, 64, -1); // north of the placement (faced dir)
        let back = BlockPos::new(0, 64, 1); // south of the placement

        // No neighbours -> straight (default).
        assert_eq!(place_corner_stair(vec![]), 2949);

        // Perpendicular same-half stair in front -> outer corner. A west-facing
        // front (counter-clockwise of north) is outer_left; east-facing outer_right.
        assert_eq!(
            place_corner_stair(vec![(front, OAK_STAIRS_WEST_BOTTOM)]),
            2955 // outer_left
        );
        assert_eq!(
            place_corner_stair(vec![(front, OAK_STAIRS_EAST_BOTTOM)]),
            2957 // outer_right
        );

        // Perpendicular same-half stair behind -> inner corner.
        assert_eq!(
            place_corner_stair(vec![(back, OAK_STAIRS_WEST_BOTTOM)]),
            2951 // inner_left
        );
        assert_eq!(
            place_corner_stair(vec![(back, OAK_STAIRS_EAST_BOTTOM)]),
            2953 // inner_right
        );
    }

    #[test]
    fn stairs_corner_suppressed_and_filtered() {
        let front = BlockPos::new(0, 64, -1);
        let east = BlockPos::new(1, 64, 0);

        // canTakeShape: a stair already aligned the same way on the turning side
        // suppresses the outer corner -> straight. (West front would be outer_left,
        // but a north/bottom stair to the east blocks it.)
        assert_eq!(
            place_corner_stair(vec![
                (front, OAK_STAIRS_WEST_BOTTOM),
                (east, OAK_STAIRS_NORTH_BOTTOM),
            ]),
            2949 // straight
        );

        // A different-half neighbour does not form a corner.
        assert_eq!(place_corner_stair(vec![(front, OAK_STAIRS_WEST_TOP)]), 2949);

        // A parallel (same-axis) neighbour does not form a corner.
        assert_eq!(
            place_corner_stair(vec![(front, OAK_STAIRS_SOUTH_BOTTOM)]),
            2949
        );
    }

    #[test]
    fn button_face_and_facing() {
        let place = |face, yaw| {
            compute_placement(&ctx(STONE_BUTTON, face, 0.5, yaw), &NoNeighbors).unwrap()
        };
        // Top-face click -> floor button facing the player's yaw direction.
        let floor = place(Direction::Up, 0.0); // yaw 0 -> south
        assert_eq!(floor.state_id, 5929); // floor/south
        assert_eq!(floor.rule, PlacementRule::FaceAttached);
        assert_eq!(place(Direction::Up, 180.0).state_id, 5927); // floor/north
                                                                // Bottom-face click -> ceiling button.
        assert_eq!(place(Direction::Down, 270.0).state_id, 5949); // ceiling/east
                                                                  // Side clicks -> wall button facing the clicked side (yaw ignored).
        assert_eq!(place(Direction::North, 12.0).state_id, 5935); // wall/north (default)
        assert_eq!(place(Direction::East, 0.0).state_id, 5941); // wall/east
        assert_eq!(place(Direction::South, 0.0).state_id, 5937); // wall/south
    }

    #[test]
    fn lever_face_and_facing() {
        let place =
            |face, yaw| compute_placement(&ctx(LEVER, face, 0.5, yaw), &NoNeighbors).unwrap();
        assert_eq!(place(Direction::Up, 90.0).state_id, 5807); // floor/west
        assert_eq!(place(Direction::North, 0.0).state_id, 5811); // wall/north (default)
        assert_eq!(place(Direction::Down, 0.0).state_id, 5821); // ceiling/south
        assert_eq!(place(Direction::Up, 90.0).rule, PlacementRule::FaceAttached);
    }

    #[test]
    fn anvil_faces_perpendicular_to_player() {
        let place =
            |yaw| compute_placement(&ctx(ANVIL, Direction::Up, 0.5, yaw), &NoNeighbors).unwrap();
        // facing = clockwise of the player's yaw direction.
        let west = place(0.0); // yaw 0 -> south -> cw -> west
        assert_eq!(west.state_id, 9918);
        assert_eq!(west.rule, PlacementRule::AnvilFacing);
        assert_eq!(place(90.0).state_id, 9916); // west -> cw -> north
        assert_eq!(place(180.0).state_id, 9919); // north -> cw -> east
        assert_eq!(place(270.0).state_id, 9917); // east -> cw -> south
    }

    #[test]
    fn end_rod_faces_clicked_face() {
        let place = |face| compute_placement(&ctx(END_ROD, face, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(place(Direction::Up).state_id, 13361); // up (default)
        assert_eq!(place(Direction::Down).state_id, 13362); // down
        assert_eq!(place(Direction::North).state_id, 13357);
        assert_eq!(place(Direction::East).state_id, 13358);
        assert_eq!(place(Direction::South).state_id, 13359);
        assert_eq!(place(Direction::West).state_id, 13360);
        assert_eq!(place(Direction::Up).rule, PlacementRule::EndRod);
    }

    #[test]
    fn observer_and_carved_pumpkin_face_player() {
        // Observer is rotated by the horizontal-facing rule (faces the player).
        let observer =
            compute_placement(&ctx(OBSERVER, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(observer.state_id, 13574); // yaw 0 -> faces north
        assert_eq!(observer.rule, PlacementRule::HorizontalFacing);
        assert_eq!(
            compute_placement(&ctx(OBSERVER, Direction::Up, 0.5, 180.0), &NoNeighbors)
                .unwrap()
                .state_id,
            13578 // faces south (default)
        );

        // Carved pumpkin (already in the horizontal-facing set) faces the player.
        assert_eq!(
            compute_placement(&ctx(CARVED_PUMPKIN, Direction::Up, 0.5, 0.0), &NoNeighbors)
                .unwrap()
                .state_id,
            6045 // faces north (default)
        );
        assert_eq!(
            compute_placement(&ctx(CARVED_PUMPKIN, Direction::Up, 0.5, 90.0), &NoNeighbors)
                .unwrap()
                .state_id,
            6048 // faces east
        );
    }
}
