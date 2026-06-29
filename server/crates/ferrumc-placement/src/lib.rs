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
//! Most rules are *single-state, single-position*: they resolve the held item to
//! one block-state at the one caller-chosen position. The two **merge** rules
//! (double-slab and candle stacking) instead *relocate* the write onto the clicked
//! cell via [`PlacementResult::place_at`]. Covered families:
//!
//! - Logs/wood & chains — `axis` from the clicked face.
//! - Slabs — `type` (top/bottom) from the clicked face + cursor height; placing a
//!   matching half-slab onto its empty half merges to the `double` state.
//! - Stairs — `facing` from yaw, `half` from the face + cursor, and the vanilla
//!   auto-corner `shape` (`inner_left`/`inner_right`/`outer_left`/`outer_right`)
//!   derived from neighbouring stairs.
//! - Torches — floor vs wall (wall variant `facing` = the clicked face).
//! - Fences — cardinal connectivity via the [`NeighborQuery`].
//! - Walls — `north`/`south`/`east`/`west` connection height (`none`/`low`/`tall`)
//!   plus the `up` post, derived from neighbours like fences.
//! - Glass panes & iron bars — cardinal `north`/`south`/`east`/`west` connectivity.
//! - Trapdoors — `facing` (clicked side, else player yaw) + `half` from the
//!   face/cursor; `open`/`powered` inherit `false`.
//! - Fence gates — `facing` from yaw, `in_wall` when flanked by walls.
//! - Buttons & levers — `face` (floor/wall/ceiling) + `facing`.
//! - Anvils — `facing` perpendicular (clockwise) to the player.
//! - End rods & amethyst clusters/buds — 6-way `facing` from the clicked face.
//! - Ladders — `facing` from the clicked wall face.
//! - Lanterns — `hanging` from an under-face (ceiling) click.
//! - Pointed dripstone — `vertical_direction` from the clicked face (always a
//!   fresh `tip`).
//! - Candles — a fresh placement is one candle; placing onto a matching candle
//!   increments `candles` (`1..=4`), keeping `lit`.
//! - Horizontal facers — the furnace family, carved pumpkins, and observers
//!   (`facing` toward the player).
//! - Simple full cubes — placed unchanged.
//!
//! **Waterlogging** is a cross-cutting post-step: any placed block that carries a
//! `waterlogged` property and lands in a water *source* becomes
//! `waterlogged=true`. It is bound to the property only — there is no fluid-flow
//! simulation. See [`compute_placement`] for the input it reads and the
//! [crate-level note](#waterlogging-input) on what full fidelity still needs.
//!
//! Two families occupy **more than one cell** and so express their second cell
//! through [`PlacementResult::extra_blocks`] rather than a single relocated write:
//!
//! - Doors — `facing` from yaw, `hinge` (`left`/`right`) from neighbouring doors
//!   and walls (default `right`); placed as the `lower` half with the `upper` half
//!   added one cell above.
//! - Beds — `facing` from yaw; placed as the `foot` with the `head` added one cell
//!   along the facing direction.
//!
//! Out of scope (placed as the default/simple state): rails, banners, and
//! redstone wiring.
//!
//! ## Waterlogging input
//!
//! The waterlogging post-step reads [`NeighborQuery::block_state_at`] at the
//! placement position and waterlogs when that cell holds a water *source*
//! (`water` with `level=0`). For this to fire in practice the host must (a) treat
//! a water-source cell as replaceable so a waterloggable item can be placed *into*
//! it, and (b) report that source through `block_state_at` at the placement cell.
//! Not yet detected (would need richer fluid state than a block-state id):
//! water-bearing plants (kelp, seagrass, bubble columns) and flowing water — the
//! latter correctly never waterlogs.

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
/// A result names a primary block-state and (via
/// [`place_at`](PlacementResult::place_at)) the one cell it belongs in. Most rules
/// write at the caller-chosen `ctx.position`; the **merge** rules (double-slab,
/// candle stacking) instead set `place_at` to *relocate* the write onto the clicked
/// cell, replacing the block already there. Families that occupy *more than one*
/// cell (doors' upper half, beds' head) name those extra cells in
/// [`extra_blocks`](PlacementResult::extra_blocks); the primary
/// [`state_id`](PlacementResult::state_id) is the lower/foot cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementResult {
    /// The computed block-state id to write to the world.
    pub state_id: u32,
    /// The block-state id that was requested (the held item's default state),
    /// retained for diagnostics and metric classification.
    pub requested_state: u32,
    /// The rule that produced [`state_id`](PlacementResult::state_id).
    pub rule: PlacementRule,
    /// Where to write [`state_id`](PlacementResult::state_id).
    ///
    /// `None` for the common case — write at the `ctx.position` the caller chose.
    /// `Some(pos)` only for the merge rules, which *replace* the block at `pos`
    /// (the clicked cell) instead of occupying `ctx.position`: a half-slab becomes
    /// a `double`, a candle gains a wick. A host that does not honour `place_at`
    /// will still write a valid block — just at `ctx.position` — so the merge
    /// silently degrades to a normal placement rather than corrupting the world.
    pub place_at: Option<BlockPos>,
    /// Additional cells this placement occupies beyond the primary
    /// [`state_id`](PlacementResult::state_id), each an absolute [`BlockPos`] and
    /// the block-state id to write there.
    ///
    /// Empty for every single-cell rule (the common case). Populated only by the
    /// multi-cell families: a door adds its `upper` half one cell above, a bed adds
    /// its `head` one cell along the facing direction. The host must place the
    /// primary cell *and* every extra atomically — first checking each extra cell is
    /// free, and rejecting the whole placement if any is obstructed (vanilla's
    /// no-half-door rule). A host that ignores this field places only the
    /// lower/foot cell, degrading a door/bed to a single (visually broken) block
    /// rather than corrupting the world.
    pub extra_blocks: Vec<(BlockPos, u32)>,
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
    /// An end rod or amethyst cluster/bud: 6-way `facing` taken from the clicked
    /// face, so it grows out of the surface it was placed against.
    EndRod,
    /// A wall: per-cardinal connection height (`none`/`low`/`tall`) and the `up`
    /// post, resolved from neighbours via the [`NeighborQuery`].
    Wall,
    /// A glass pane or iron bars: cardinal `north`/`south`/`east`/`west`
    /// connectivity resolved via the [`NeighborQuery`].
    Pane,
    /// A ladder: `facing` taken from the clicked (horizontal) wall face.
    Ladder,
    /// A lantern: `hanging` when placed against a block's under-face (ceiling).
    Lantern,
    /// Pointed dripstone: `vertical_direction` from the clicked face, placed as a
    /// fresh `tip`.
    Dripstone,
    /// A slab merged onto its matching half to form the `double` state; the write
    /// is relocated onto the clicked cell (see [`PlacementResult::place_at`]).
    DoubleSlab,
    /// A candle stacked onto a matching candle, incrementing `candles`; the write
    /// is relocated onto the clicked cell (see [`PlacementResult::place_at`]).
    CandleStack,
    /// A door: `facing` from yaw, `hinge` from neighbouring doors/walls; the primary
    /// cell is the `lower` half and the `upper` half rides in
    /// [`PlacementResult::extra_blocks`].
    Door,
    /// A bed: `facing` from yaw; the primary cell is the `foot` and the `head` rides
    /// in [`PlacementResult::extra_blocks`].
    Bed,
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
    /// End rods and amethyst clusters/buds — 6-way `facing` from the clicked face.
    EndRod,
    /// Walls — cardinal connection height plus the `up` post.
    Wall,
    /// Glass panes and iron bars — cardinal connectivity.
    Pane,
    /// Ladders — `facing` from the clicked wall face.
    Ladder,
    /// Lanterns — `hanging` from an under-face click.
    Lantern,
    /// Pointed dripstone — `vertical_direction` from the clicked face.
    Dripstone,
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

/// The `waterlogged` boolean property shared by every waterloggable block.
const WATERLOGGED: &str = "waterlogged";
/// The string a boolean property takes when set; bools encode as
/// `["true", "false"]` (index `0` = `true`).
const BOOL_TRUE: &str = "true";
/// The resource name of the fluid block whose source state drives waterlogging.
const WATER_BLOCK: &str = "water";
/// The `level` value of a water *source* (a still, full block); flowing water has
/// a non-zero level and never waterlogs.
const WATER_SOURCE_LEVEL: &str = "0";

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
    } else if name.ends_with("_wall") {
        // Checked before `_fence`; no wall name ends with `_fence`, and the
        // `wall_torch`/`*_wall_sign` blocks are claimed by earlier arms / a
        // `_sign` suffix, so `_wall` here only matches true `*_wall` blocks.
        Class::Wall
    } else if name.ends_with("_fence") {
        Class::Fence
    } else if is_pane_or_bars(name) {
        Class::Pane
    } else if name == "lever" {
        Class::FaceAttached
    } else if is_anvil(name) {
        Class::Anvil
    } else if name == "end_rod" || is_amethyst(name) {
        Class::EndRod
    } else if name == "ladder" {
        Class::Ladder
    } else if name == "lantern" || name == "soul_lantern" {
        Class::Lantern
    } else if name == "pointed_dripstone" {
        Class::Dripstone
    } else if is_axis_block(name) {
        Class::Axis
    } else if HORIZONTAL_FACING.contains(&name) {
        Class::HorizontalFacing
    } else {
        Class::SimpleCube
    }
}

/// Returns `true` for a glass pane (clear or stained) or iron bars — the thin
/// connecting blocks that share a cardinal `north`/`south`/`east`/`west` shape.
fn is_pane_or_bars(name: &str) -> bool {
    name == "glass_pane" || name.ends_with("_glass_pane") || name == "iron_bars"
}

/// Returns `true` for an amethyst cluster or one of its three bud stages, which
/// grow out of the clicked face exactly like an end rod (6-way `facing`).
fn is_amethyst(name: &str) -> bool {
    name == "amethyst_cluster" || name.ends_with("_amethyst_bud")
}

/// Returns `true` for a candle (plain or dyed), the family that stacks 1..=4
/// candles into one cell. Excludes `*_candle_cake`, which ends with `_cake`.
fn is_candle(name: &str) -> bool {
    name == "candle" || name.ends_with("_candle")
}

/// Returns `true` for log/wood-style blocks that rotate via an `axis` property.
///
/// Chains share the same `axis` (`x`/`y`/`z`) mechanic, so they ride this rule too.
fn is_axis_block(name: &str) -> bool {
    // Stripped variants also end with these suffixes (e.g. `stripped_oak_log`).
    name.ends_with("_log")
        || name.ends_with("_wood")
        || name.ends_with("_stem")
        || name.ends_with("_hyphae")
        || name == "chain"
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

/// Overrides a single property `prop` of `state_id` to `value`, returning the new
/// state id — the in-place counterpart of [`state_property`].
///
/// Unlike [`compute_state_id`] (which starts from a block's *default* state), this
/// edits an arbitrary existing state, preserving every other property's digit. The
/// linear encoding is separable, so replacing one property's digit never disturbs
/// another's. Returns `None` if `state_id` is not a registry block, the block has
/// no such property, or `value` is not one of that property's values. Used to set
/// `waterlogged=true` on an already-computed state and to bump a candle's count.
fn set_state_property(state_id: u32, prop: &str, value: &str) -> Option<u32> {
    let meta = block_metadata(state_id_to_block_name(state_id)?)?;
    let pi = meta.properties.iter().position(|p| p.name == prop)?;
    let later: u32 = meta.properties[pi + 1..]
        .iter()
        .map(|p| p.cardinality as u32)
        .product();
    let card = meta.properties[pi].cardinality as u32;
    let new_index = meta.properties[pi]
        .values
        .iter()
        .position(|v| *v == value)? as u32;
    let old_index = ((state_id - meta.min_state) / later) % card;
    Some(state_id - old_index * later + new_index * later)
}

/// Returns `true` if the block named `name` carries a property called `prop`.
fn block_has_property(name: &str, prop: &str) -> bool {
    block_metadata(name).is_some_and(|m| m.properties.iter().any(|p| p.name == prop))
}

/// Returns `true` if `state_id` is a water *source* block (still `water` at
/// `level=0`). Flowing water (a non-zero `level`) and every other block return
/// `false`.
///
/// Exposed so a host can treat a water-source cell as *replaceable* — admitting a
/// waterloggable block into it (which the waterlogging post-step then flips to
/// `waterlogged=true`) without breaking the usual "cannot place into a solid"
/// rule.
#[must_use]
pub fn is_water_source(state_id: u32) -> bool {
    state_id_to_block_name(state_id) == Some(WATER_BLOCK)
        && state_property(state_id, "level") == Some(WATER_SOURCE_LEVEL)
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

/// The four cardinal directions, in the order the connection rules scan them.
const CARDINALS: [Direction; 4] = [
    Direction::North,
    Direction::South,
    Direction::East,
    Direction::West,
];

/// Returns `true` if the block at `position` connects to a wall: another wall, a
/// fence gate, a pane/bars, or any solid full cube. Air, unreadable, and other
/// non-connectable neighbours return `false`.
fn wall_connects(neighbors: &dyn NeighborQuery, position: BlockPos) -> bool {
    let Some(name) = neighbors
        .block_state_at(position)
        .and_then(state_id_to_block_name)
    else {
        return false;
    };
    name.ends_with("_wall")
        || name.ends_with("_fence_gate")
        || is_pane_or_bars(name)
        || block_metadata(name).is_some_and(|m| m.is_solid_cube)
}

/// Returns `true` if the block at `position` connects to a pane or bars: another
/// pane/bars, a wall, or any solid full cube.
fn pane_connects(neighbors: &dyn NeighborQuery, position: BlockPos) -> bool {
    let Some(name) = neighbors
        .block_state_at(position)
        .and_then(state_id_to_block_name)
    else {
        return false;
    };
    is_pane_or_bars(name)
        || name.ends_with("_wall")
        || block_metadata(name).is_some_and(|m| m.is_solid_cube)
}

/// Returns `true` if the block at `position` would raise a wall's connections to
/// full (`tall`) height — a solid full cube or another wall sitting there.
fn raises_wall(neighbors: &dyn NeighborQuery, position: BlockPos) -> bool {
    neighbors
        .block_state_at(position)
        .and_then(state_id_to_block_name)
        .is_some_and(|name| {
            name.ends_with("_wall") || block_metadata(name).is_some_and(|m| m.is_solid_cube)
        })
}

/// Resolves a wall's connection shape into a concrete state id.
///
/// Each cardinal neighbour the [`NeighborQuery`] reports as connectable sets that
/// side to `low`, raised to `tall` when a solid block (or wall) sits directly
/// above. The `up` centre post is shown unless the wall is a straight run on a
/// single axis (exactly two opposite connections and nothing else) with no block
/// above — mirroring vanilla's `WallBlock`. `waterlogged` inherits its `false`
/// default (the waterlogging post-step sets it). Returns `None` only if
/// `wall_name` is not a registry block.
///
/// The per-connection `tall`/`low` here is a deliberate simplification: vanilla
/// also raises a side when the *neighbour's* collision shape is tall, which a bare
/// block-state id cannot express. The common cases (straight runs, corners, posts,
/// covered walls) match.
#[must_use]
pub fn compute_wall_connection_state(
    wall_name: &str,
    position: BlockPos,
    neighbors: &dyn NeighborQuery,
) -> Option<u32> {
    let above_raises = raises_wall(neighbors, position.offset(Direction::Up));
    let height = if above_raises { "tall" } else { "low" };

    let mut props: BTreeMap<&str, &str> = BTreeMap::new();
    let connected = |dir| wall_connects(neighbors, position.offset(dir));
    let (n, s, e, w) = (
        connected(Direction::North),
        connected(Direction::South),
        connected(Direction::East),
        connected(Direction::West),
    );
    for (dir, on) in [
        (Direction::North, n),
        (Direction::South, s),
        (Direction::East, e),
        (Direction::West, w),
    ] {
        if on {
            props.insert(facing_string(dir), height);
        }
    }

    // The post is dropped only for a straight, single-axis run with nothing above.
    let straight = (n && s && !e && !w) || (e && w && !n && !s);
    props.insert(
        "up",
        if !straight || above_raises {
            "true"
        } else {
            "false"
        },
    );

    compute_state_id(wall_name, &props)
}

/// Resolves a glass pane's or iron bars' cardinal connectivity into a concrete
/// state id.
///
/// Each cardinal neighbour [`pane_connects`] accepts sets that direction's boolean
/// `true`; unset directions and `waterlogged` inherit the (all-false) default.
/// Returns `None` only if `pane_name` is not a registry block.
#[must_use]
pub fn compute_pane_connection_state(
    pane_name: &str,
    position: BlockPos,
    neighbors: &dyn NeighborQuery,
) -> Option<u32> {
    let mut props: BTreeMap<&str, &str> = BTreeMap::new();
    for dir in CARDINALS {
        if pane_connects(neighbors, position.offset(dir)) {
            props.insert(facing_string(dir), "true");
        }
    }
    compute_state_id(pane_name, &props)
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

    // Merge rules relocate the write onto the clicked cell, so they are resolved
    // before the position-keyed family rules (and skip the waterlogging step: a
    // double slab expels its water, a stacked candle keeps the existing state's).
    if let Some(merged) = try_merge(name, ctx, neighbors) {
        return Some(merged);
    }

    // Doors and beds occupy two cells; resolved before the single-cell family rules
    // and exempt from the waterlogging post-step (neither carries a `waterlogged`
    // property).
    if let Some(multi) = try_multiblock(name, ctx, neighbors) {
        return Some(multi);
    }

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
        Class::Wall => (
            compute_wall_connection_state(name, ctx.position, neighbors).unwrap_or(fallback),
            PlacementRule::Wall,
        ),
        Class::Pane => (
            compute_pane_connection_state(name, ctx.position, neighbors).unwrap_or(fallback),
            PlacementRule::Pane,
        ),
        Class::Trapdoor => place_trapdoor(name, fallback, ctx),
        Class::FenceGate => place_fence_gate(name, fallback, ctx, neighbors),
        Class::FaceAttached => place_face_attached(name, fallback, ctx),
        Class::Anvil => place_anvil(name, fallback, ctx),
        Class::EndRod => place_end_rod(name, fallback, ctx),
        Class::Ladder => place_ladder(name, fallback, ctx),
        Class::Lantern => place_lantern(name, fallback, ctx),
        Class::Dripstone => place_dripstone(name, fallback, ctx),
    };

    // Waterlogging post-step: any placed block that carries a `waterlogged`
    // property and lands in a water source becomes waterlogged. Keyed off the
    // *placed* block (a side-clicked torch becomes `wall_torch`, which has no such
    // property), so it composes with every family rule above.
    let placed_name = state_id_to_block_name(state_id).unwrap_or(name);
    let state_id = if block_has_property(placed_name, WATERLOGGED)
        && neighbors
            .block_state_at(ctx.position)
            .is_some_and(is_water_source)
    {
        set_state_property(state_id, WATERLOGGED, BOOL_TRUE).unwrap_or(state_id)
    } else {
        state_id
    };

    Some(PlacementResult {
        state_id,
        requested_state: ctx.item_block_state,
        rule,
        place_at: None,
        extra_blocks: Vec::new(),
    })
}

/// Resolves a slab→`double` or candle-stack merge, or `None` when the click is not
/// a merge.
///
/// A merge fires only when the block already in the **clicked** cell
/// (`ctx.position` stepped back along the clicked face) is the *same* block as the
/// held item and the click completes it. Its [`PlacementResult::place_at`] is the
/// clicked cell, so the host replaces that block instead of writing at
/// `ctx.position`.
fn try_merge(
    name: &str,
    ctx: &PlacementContext,
    neighbors: &dyn NeighborQuery,
) -> Option<PlacementResult> {
    let is_slab = name.ends_with("_slab");
    if !is_slab && !is_candle(name) {
        return None;
    }
    // The clicked cell is one step back along the clicked face from the placement
    // cell (the host steps the placement off the clicked face).
    let clicked = ctx.position.offset(ctx.clicked_face.opposite());
    let existing = neighbors.block_state_at(clicked)?;
    if state_id_to_block_name(existing) != Some(name) {
        return None;
    }

    let (state_id, rule) = if is_slab {
        let completes = match state_property(existing, "type")? {
            // A bottom slab is completed from above (top face) or by aiming at the
            // upper half of a side; a top slab, the mirror. A `double` is full.
            "bottom" => {
                ctx.clicked_face == Direction::Up
                    || (is_horizontal(ctx.clicked_face) && ctx.cursor_position.y > 0.5)
            }
            "top" => {
                ctx.clicked_face == Direction::Down
                    || (is_horizontal(ctx.clicked_face) && ctx.cursor_position.y < 0.5)
            }
            _ => false,
        };
        if !completes {
            return None;
        }
        // Build `double` from the block default, so the merged slab is dry
        // (vanilla expels the water a half-slab may have held).
        let mut props = BTreeMap::new();
        props.insert("type", "double");
        (encode(name, &props, existing), PlacementRule::DoubleSlab)
    } else {
        // Candles stack up to four; bump the existing state in place so its `lit`
        // and `waterlogged` carry over.
        let next = match state_property(existing, "candles")? {
            "1" => "2",
            "2" => "3",
            "3" => "4",
            _ => return None,
        };
        (
            set_state_property(existing, "candles", next)?,
            PlacementRule::CandleStack,
        )
    };

    Some(PlacementResult {
        state_id,
        requested_state: ctx.item_block_state,
        rule,
        place_at: Some(clicked),
        extra_blocks: Vec::new(),
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

/// An end rod or amethyst cluster/bud: 6-way `facing` taken directly from the
/// clicked face, so it points out of the surface it was placed against.
fn place_end_rod(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(ctx.clicked_face));
    (encode(name, &props, fallback), PlacementRule::EndRod)
}

/// A ladder: `facing` is the clicked (horizontal) wall face, so the ladder hangs
/// on that wall facing outward. A top/bottom click cannot attach to a wall here
/// (vanilla searches for a nearby wall, which needs more than the clicked face), so
/// it degrades to the held default.
fn place_ladder(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    if is_horizontal(ctx.clicked_face) {
        let mut props = BTreeMap::new();
        props.insert("facing", facing_string(ctx.clicked_face));
        (encode(name, &props, fallback), PlacementRule::Ladder)
    } else {
        (fallback, PlacementRule::Ladder)
    }
}

/// A lantern: `hanging` when placed against a block's under-face (the clicked
/// `Down` face), otherwise standing. `waterlogged` inherits its default (set by
/// the waterlogging post-step).
fn place_lantern(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let mut props = BTreeMap::new();
    props.insert(
        "hanging",
        if ctx.clicked_face == Direction::Down {
            "true"
        } else {
            "false"
        },
    );
    (encode(name, &props, fallback), PlacementRule::Lantern)
}

/// Pointed dripstone: `vertical_direction` points `down` when hung from a ceiling
/// (the clicked `Down` face) and `up` otherwise; always a fresh `tip` (vanilla
/// merges/thickens via neighbour scans this engine does not run).
fn place_dripstone(name: &str, fallback: u32, ctx: &PlacementContext) -> (u32, PlacementRule) {
    let mut props = BTreeMap::new();
    props.insert(
        "vertical_direction",
        if ctx.clicked_face == Direction::Down {
            "down"
        } else {
            "up"
        },
    );
    props.insert("thickness", "tip");
    (encode(name, &props, fallback), PlacementRule::Dripstone)
}

/// Returns `true` for a door block, the family placed as a two-cell `lower`+`upper`
/// block. A `_trapdoor` is excluded — it does not end with `_door`.
fn is_door(name: &str) -> bool {
    name.ends_with("_door")
}

/// Returns `true` for a bed block, the family placed as a two-cell `foot`+`head`
/// block.
fn is_bed(name: &str) -> bool {
    name.ends_with("_bed")
}

/// Resolves a door or bed into its full multi-cell [`PlacementResult`], or `None`
/// for any other block.
///
/// Both occupy two cells: the primary [`PlacementResult::state_id`] is the cell the
/// caller chose (`ctx.position` — the door's `lower` half, the bed's `foot`) and
/// the second cell (the `upper` half / `head`) rides in
/// [`PlacementResult::extra_blocks`].
fn try_multiblock(
    name: &str,
    ctx: &PlacementContext,
    neighbors: &dyn NeighborQuery,
) -> Option<PlacementResult> {
    if is_door(name) {
        Some(place_door(name, ctx, neighbors))
    } else if is_bed(name) {
        Some(place_bed(name, ctx))
    } else {
        None
    }
}

/// A door: `facing` from the player's yaw, `hinge` from neighbouring doors/walls
/// (default `right`); placed as the `lower` half at `ctx.position` with the `upper`
/// half one cell above (in [`PlacementResult::extra_blocks`]). Both halves share
/// the same `facing`/`hinge` and inherit `open=false`/`powered=false`.
fn place_door(
    name: &str,
    ctx: &PlacementContext,
    neighbors: &dyn NeighborQuery,
) -> PlacementResult {
    let fallback = ctx.item_block_state;
    let facing = yaw_to_cardinal(ctx.player_yaw);
    let hinge = door_hinge(name, facing, ctx.position, neighbors);
    let lower = door_state(name, facing, "lower", hinge, fallback);
    let upper = door_state(name, facing, "upper", hinge, fallback);
    PlacementResult {
        state_id: lower,
        requested_state: ctx.item_block_state,
        rule: PlacementRule::Door,
        place_at: None,
        extra_blocks: vec![(ctx.position.offset(Direction::Up), upper)],
    }
}

/// Encodes one door half (`lower`/`upper`) with the shared `facing`/`hinge` and the
/// inherited `open=false`/`powered=false`, degrading to `fallback` on an encoding
/// failure.
fn door_state(name: &str, facing: Direction, half: &str, hinge: &str, fallback: u32) -> u32 {
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(facing));
    props.insert("half", half);
    props.insert("hinge", hinge);
    props.insert("open", "false");
    props.insert("powered", "false");
    encode(name, &props, fallback)
}

/// Derives a door's `hinge` (`left`/`right`) from its neighbours, a simplified port
/// of vanilla `DoorBlock.getHinge`.
///
/// The hinge defaults to `right`. It flips to `left` when an identical door (lower
/// half) sits on the right side — so the pair meets as a matching double door — or
/// when full-cube blocks weigh the placement toward the left. Vanilla's final
/// cursor-position tie-break is intentionally dropped in favour of that documented
/// `right` default. "Full cube" uses the registry [`block_metadata`]
/// `is_solid_cube` flag as a proxy for vanilla's collision-shape test; other doors
/// never count as full (they are handled by the same-door checks).
fn door_hinge(
    name: &str,
    facing: Direction,
    position: BlockPos,
    neighbors: &dyn NeighborQuery,
) -> &'static str {
    let left = rotate_counterclockwise(facing);
    let right = rotate_clockwise(facing);
    let above = position.offset(Direction::Up);

    let is_full = |p: BlockPos| {
        neighbors
            .block_state_at(p)
            .and_then(state_id_to_block_name)
            .is_some_and(|n| !is_door(n) && block_metadata(n).is_some_and(|m| m.is_solid_cube))
    };
    let is_same_lower_door = |p: BlockPos| {
        neighbors.block_state_at(p).is_some_and(|s| {
            state_id_to_block_name(s) == Some(name) && state_property(s, "half") == Some("lower")
        })
    };

    let left_door = is_same_lower_door(position.offset(left));
    let right_door = is_same_lower_door(position.offset(right));
    // Positive favours the right side, negative the left (matching vanilla's `i`).
    let score = i32::from(is_full(position.offset(right)))
        + i32::from(is_full(above.offset(right)))
        - i32::from(is_full(position.offset(left)))
        - i32::from(is_full(above.offset(left)));

    let favors_left = (!left_door || right_door) && score <= 0;
    let tie_or_right = (!right_door || left_door) && score >= 0;
    if favors_left && !tie_or_right {
        "left"
    } else {
        "right"
    }
}

/// A bed: `facing` from the player's yaw; placed as the `foot` at `ctx.position`
/// with the `head` one cell along the facing direction (in
/// [`PlacementResult::extra_blocks`]). Both parts share the same `facing` and
/// inherit `occupied=false`.
fn place_bed(name: &str, ctx: &PlacementContext) -> PlacementResult {
    let fallback = ctx.item_block_state;
    let facing = yaw_to_cardinal(ctx.player_yaw);
    let foot = bed_state(name, facing, "foot", fallback);
    let head = bed_state(name, facing, "head", fallback);
    PlacementResult {
        state_id: foot,
        requested_state: ctx.item_block_state,
        rule: PlacementRule::Bed,
        place_at: None,
        extra_blocks: vec![(ctx.position.offset(facing), head)],
    }
}

/// Encodes one bed part (`foot`/`head`) with the shared `facing` and the inherited
/// `occupied=false`, degrading to `fallback` on an encoding failure.
fn bed_state(name: &str, facing: Direction, part: &str, fallback: u32) -> u32 {
    let mut props = BTreeMap::new();
    props.insert("facing", facing_string(facing));
    props.insert("part", part);
    encode(name, &props, fallback)
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

    // Round-2 families (default states unless suffixed).
    const WATER_SOURCE: u32 = 86; // water level=0
    const FLOWING_WATER: u32 = 87; // water level=1
    const OAK_SLAB_TOP: u32 = 12052;
    const STONE_SLAB: u32 = 12120;
    const GLASS_PANE: u32 = 7053;
    const IRON_BARS: u32 = 7015;
    const LADDER: u32 = 4751; // facing=north
    const CHAIN: u32 = 7019; // axis=y
    const LANTERN: u32 = 19561; // hanging=false
    const SOUL_LANTERN: u32 = 19565;
    const AMETHYST_CLUSTER: u32 = 22102; // facing=up
    const POINTED_DRIPSTONE: u32 = 25813; // tip, up
    const CANDLE: u32 = 21788; // candles=1, lit=false
    const CANDLE_LIT: u32 = 21786; // candles=1, lit=true
    const CANDLE_FULL: u32 = 21800; // candles=4

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

    /// A neighbour query with a single block-state at the placement cell — the
    /// input the waterlogging post-step reads.
    fn target_state(state: u32) -> MockNeighbors {
        MockNeighbors {
            states: vec![(BlockPos::new(0, 64, 0), state)],
            ..Default::default()
        }
    }

    #[test]
    fn waterlogging_sets_the_property_in_a_water_source() {
        let water = target_state(WATER_SOURCE);
        // Every waterloggable family flips `waterlogged` (subtracting its place
        // value from the dry state) when placed into a still water source.
        let slab = compute_placement(&ctx(OAK_SLAB, Direction::Up, 0.5, 0.0), &water).unwrap();
        assert_eq!(slab.state_id, 12053); // bottom + waterlogged
        assert_eq!(slab.rule, PlacementRule::Slab);
        let stairs =
            compute_placement(&ctx(OAK_STAIRS, Direction::Up, 0.0, 180.0), &water).unwrap();
        assert_eq!(stairs.state_id, 2948); // north/bottom/straight + waterlogged
        let fence = compute_placement(&ctx(OAK_FENCE, Direction::Up, 0.5, 0.0), &water).unwrap();
        assert_eq!(fence.state_id, 6025); // isolated + waterlogged
        let pane = compute_placement(&ctx(GLASS_PANE, Direction::Up, 0.5, 0.0), &water).unwrap();
        assert_eq!(pane.state_id, 7051); // isolated + waterlogged
        let bars = compute_placement(&ctx(IRON_BARS, Direction::Up, 0.5, 0.0), &water).unwrap();
        assert_eq!(bars.state_id, 7013);
        let trap = compute_placement(&ctx(OAK_TRAPDOOR, Direction::Up, 0.5, 0.0), &water).unwrap();
        assert_eq!(trap.state_id, 6154); // north/bottom + waterlogged
        let wall =
            compute_placement(&ctx(COBBLESTONE_WALL, Direction::Up, 0.5, 0.0), &water).unwrap();
        assert_eq!(wall.state_id, 8703); // default post + waterlogged
        let ladder = compute_placement(&ctx(LADDER, Direction::North, 0.5, 0.0), &water).unwrap();
        assert_eq!(ladder.state_id, 4750); // facing north + waterlogged
    }

    #[test]
    fn flowing_water_dry_air_and_non_waterloggable_do_not_waterlog() {
        // Flowing water (level != 0) is not a source -> dry.
        let flowing = target_state(FLOWING_WATER);
        let slab = compute_placement(&ctx(OAK_SLAB, Direction::Up, 0.5, 0.0), &flowing).unwrap();
        assert_eq!(slab.state_id, 12054); // bottom, NOT waterlogged
                                          // No fluid reported at all -> dry.
        let dry = compute_placement(&ctx(OAK_SLAB, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(dry.state_id, 12054);
        // A block with no `waterlogged` property is untouched even in a source.
        let stone = compute_placement(
            &ctx(STONE, Direction::Up, 0.5, 0.0),
            &target_state(WATER_SOURCE),
        )
        .unwrap();
        assert_eq!(stone.state_id, 1);
    }

    #[test]
    fn slab_merges_into_a_double() {
        // Top-face click on a bottom slab -> double at the clicked cell below.
        let below = MockNeighbors {
            states: vec![(BlockPos::new(0, 63, 0), OAK_SLAB)],
            ..Default::default()
        };
        let merged = compute_placement(&ctx(OAK_SLAB, Direction::Up, 0.5, 0.0), &below).unwrap();
        assert_eq!(merged.state_id, 12056); // double
        assert_eq!(merged.rule, PlacementRule::DoubleSlab);
        assert_eq!(merged.place_at, Some(BlockPos::new(0, 63, 0)));

        // Bottom-face click on a top slab -> double at the clicked cell above.
        let above = MockNeighbors {
            states: vec![(BlockPos::new(0, 65, 0), OAK_SLAB_TOP)],
            ..Default::default()
        };
        let merged_top =
            compute_placement(&ctx(OAK_SLAB, Direction::Down, 0.5, 0.0), &above).unwrap();
        assert_eq!(merged_top.state_id, 12056);
        assert_eq!(merged_top.place_at, Some(BlockPos::new(0, 65, 0)));

        // A side click at the upper half of a bottom slab completes it too.
        let side = MockNeighbors {
            states: vec![(BlockPos::new(0, 64, 1), OAK_SLAB)],
            ..Default::default()
        };
        let merged_side =
            compute_placement(&ctx(OAK_SLAB, Direction::North, 0.8, 0.0), &side).unwrap();
        assert_eq!(merged_side.state_id, 12056);
        assert_eq!(merged_side.place_at, Some(BlockPos::new(0, 64, 1)));
    }

    #[test]
    fn slab_does_not_merge_on_a_wrong_click_or_mismatched_material() {
        // Bottom-face click on a bottom slab does not complete it: a plain top slab
        // is placed at the (stepped) position instead, with no relocation.
        let bottom_above = MockNeighbors {
            states: vec![(BlockPos::new(0, 65, 0), OAK_SLAB)],
            ..Default::default()
        };
        let nope =
            compute_placement(&ctx(OAK_SLAB, Direction::Down, 0.5, 0.0), &bottom_above).unwrap();
        assert_eq!(nope.rule, PlacementRule::Slab);
        assert_eq!(nope.state_id, 12052); // top
        assert_eq!(nope.place_at, None);

        // A different slab material never merges.
        let stone_below = MockNeighbors {
            states: vec![(BlockPos::new(0, 63, 0), STONE_SLAB)],
            ..Default::default()
        };
        let diff =
            compute_placement(&ctx(OAK_SLAB, Direction::Up, 0.5, 0.0), &stone_below).unwrap();
        assert_eq!(diff.rule, PlacementRule::Slab);
        assert_eq!(diff.state_id, 12054); // a fresh bottom oak slab
        assert_eq!(diff.place_at, None);
    }

    #[test]
    fn candle_stacks_up_to_four_keeping_lit() {
        // A fresh candle on the ground is one (unlit) candle, no relocation.
        let fresh = compute_placement(&ctx(CANDLE, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(fresh.state_id, CANDLE); // candles=1
        assert_eq!(fresh.rule, PlacementRule::SimpleCube);
        assert_eq!(fresh.place_at, None);

        // Clicking a 1-candle cell adds a second, relocated onto that cell.
        let one = MockNeighbors {
            states: vec![(BlockPos::new(0, 63, 0), CANDLE)],
            ..Default::default()
        };
        let two = compute_placement(&ctx(CANDLE, Direction::Up, 0.5, 0.0), &one).unwrap();
        assert_eq!(two.state_id, 21792); // candles=2, lit=false
        assert_eq!(two.rule, PlacementRule::CandleStack);
        assert_eq!(two.place_at, Some(BlockPos::new(0, 63, 0)));

        // Stacking onto a lit candle keeps it lit.
        let lit = MockNeighbors {
            states: vec![(BlockPos::new(0, 63, 0), CANDLE_LIT)],
            ..Default::default()
        };
        let lit_two = compute_placement(&ctx(CANDLE, Direction::Up, 0.5, 0.0), &lit).unwrap();
        assert_eq!(lit_two.state_id, 21790); // candles=2, lit=true

        // A full (4-candle) cell does not stack; a fresh candle is placed instead.
        let full = MockNeighbors {
            states: vec![(BlockPos::new(0, 63, 0), CANDLE_FULL)],
            ..Default::default()
        };
        let blocked = compute_placement(&ctx(CANDLE, Direction::Up, 0.5, 0.0), &full).unwrap();
        assert_eq!(blocked.rule, PlacementRule::SimpleCube);
        assert_eq!(blocked.state_id, CANDLE);
    }

    #[test]
    fn wall_connection_shape_from_neighbors() {
        // Isolated wall: just the post (up=true), default state.
        let isolated = compute_placement(
            &ctx(COBBLESTONE_WALL, Direction::Up, 0.5, 0.0),
            &NoNeighbors,
        )
        .unwrap();
        assert_eq!(isolated.state_id, 8706);
        assert_eq!(isolated.rule, PlacementRule::Wall);

        // A single solid neighbour to the north -> north=low, post kept.
        let north = MockNeighbors {
            states: vec![(BlockPos::new(0, 64, -1), STONE)],
            ..Default::default()
        };
        let one =
            compute_placement(&ctx(COBBLESTONE_WALL, Direction::Up, 0.5, 0.0), &north).unwrap();
        assert_eq!(one.state_id, 8742); // north=low, up=true

        // A straight north-south run drops the post.
        let straight = MockNeighbors {
            states: vec![
                (BlockPos::new(0, 64, -1), STONE),
                (BlockPos::new(0, 64, 1), STONE),
            ],
            ..Default::default()
        };
        let through =
            compute_placement(&ctx(COBBLESTONE_WALL, Direction::Up, 0.5, 0.0), &straight).unwrap();
        assert_eq!(through.state_id, 8760); // north=low, south=low, up=false

        // A corner (north + east) keeps the post.
        let corner = MockNeighbors {
            states: vec![
                (BlockPos::new(0, 64, -1), STONE),
                (BlockPos::new(1, 64, 0), STONE),
            ],
            ..Default::default()
        };
        let bent =
            compute_placement(&ctx(COBBLESTONE_WALL, Direction::Up, 0.5, 0.0), &corner).unwrap();
        assert_eq!(bent.state_id, 8850); // north=low, east=low, up=true

        // A solid block above raises the connection to tall and forces the post.
        let covered = MockNeighbors {
            states: vec![
                (BlockPos::new(0, 64, -1), STONE),
                (BlockPos::new(0, 64, 1), STONE),
                (BlockPos::new(0, 65, 0), STONE),
            ],
            ..Default::default()
        };
        let tall =
            compute_placement(&ctx(COBBLESTONE_WALL, Direction::Up, 0.5, 0.0), &covered).unwrap();
        assert_eq!(tall.state_id, 8802); // north=tall, south=tall, up=true (block above)
    }

    #[test]
    fn pane_and_bars_connect_to_neighbors() {
        // Isolated glass pane / iron bars: all-false default.
        let pane =
            compute_placement(&ctx(GLASS_PANE, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(pane.state_id, 7053);
        assert_eq!(pane.rule, PlacementRule::Pane);

        // North solid neighbour -> north connection.
        let north = MockNeighbors {
            states: vec![(BlockPos::new(0, 64, -1), STONE)],
            ..Default::default()
        };
        let one = compute_placement(&ctx(GLASS_PANE, Direction::Up, 0.5, 0.0), &north).unwrap();
        assert_eq!(one.state_id, 7045);

        // North + east.
        let north_east = MockNeighbors {
            states: vec![
                (BlockPos::new(0, 64, -1), STONE),
                (BlockPos::new(1, 64, 0), STONE),
            ],
            ..Default::default()
        };
        let two =
            compute_placement(&ctx(GLASS_PANE, Direction::Up, 0.5, 0.0), &north_east).unwrap();
        assert_eq!(two.state_id, 7029);

        // A pane connects to another pane neighbour, not just solid cubes.
        let pane_neighbor = MockNeighbors {
            states: vec![(BlockPos::new(0, 64, -1), GLASS_PANE)],
            ..Default::default()
        };
        let linked =
            compute_placement(&ctx(GLASS_PANE, Direction::Up, 0.5, 0.0), &pane_neighbor).unwrap();
        assert_eq!(linked.state_id, 7045);

        // Iron bars: north + south.
        let ns = MockNeighbors {
            states: vec![
                (BlockPos::new(0, 64, -1), STONE),
                (BlockPos::new(0, 64, 1), STONE),
            ],
            ..Default::default()
        };
        let bars = compute_placement(&ctx(IRON_BARS, Direction::Up, 0.5, 0.0), &ns).unwrap();
        assert_eq!(bars.state_id, 7003);
    }

    #[test]
    fn ladder_faces_the_clicked_wall() {
        let place = |face| compute_placement(&ctx(LADDER, face, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(place(Direction::North).state_id, 4751); // default
        assert_eq!(place(Direction::North).rule, PlacementRule::Ladder);
        assert_eq!(place(Direction::South).state_id, 4753);
        assert_eq!(place(Direction::West).state_id, 4755);
        assert_eq!(place(Direction::East).state_id, 4757);
        // A vertical click cannot attach to a wall -> the held default.
        assert_eq!(place(Direction::Up).state_id, 4751);
        assert_eq!(place(Direction::Down).state_id, 4751);
    }

    #[test]
    fn chain_takes_axis_from_face() {
        let place = |face| compute_placement(&ctx(CHAIN, face, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(place(Direction::Up).state_id, 7019); // y (default)
        assert_eq!(place(Direction::Up).rule, PlacementRule::AxisFromFace);
        assert_eq!(place(Direction::North).state_id, 7021); // z
        assert_eq!(place(Direction::East).state_id, 7017); // x
    }

    #[test]
    fn lantern_hangs_from_a_ceiling_click() {
        let place =
            |item, face| compute_placement(&ctx(item, face, 0.5, 0.0), &NoNeighbors).unwrap();
        // Floor (up) and side clicks stand; an under-face (down) click hangs.
        assert_eq!(place(LANTERN, Direction::Up).state_id, 19561); // standing (default)
        assert_eq!(place(LANTERN, Direction::Up).rule, PlacementRule::Lantern);
        assert_eq!(place(LANTERN, Direction::North).state_id, 19561); // standing
        assert_eq!(place(LANTERN, Direction::Down).state_id, 19559); // hanging
        assert_eq!(place(SOUL_LANTERN, Direction::Down).state_id, 19563); // hanging
    }

    #[test]
    fn amethyst_cluster_grows_out_of_the_clicked_face() {
        let place =
            |face| compute_placement(&ctx(AMETHYST_CLUSTER, face, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(place(Direction::Up).state_id, 22102); // up (default)
        assert_eq!(place(Direction::Up).rule, PlacementRule::EndRod);
        assert_eq!(place(Direction::Down).state_id, 22104);
        assert_eq!(place(Direction::North).state_id, 22094);
        assert_eq!(place(Direction::East).state_id, 22096);
        assert_eq!(place(Direction::South).state_id, 22098);
        assert_eq!(place(Direction::West).state_id, 22100);
    }

    #[test]
    fn pointed_dripstone_orients_from_the_clicked_face() {
        let place = |face| {
            compute_placement(&ctx(POINTED_DRIPSTONE, face, 0.5, 0.0), &NoNeighbors).unwrap()
        };
        // Floor click points up (default tip); ceiling (down) click points down.
        assert_eq!(place(Direction::Up).state_id, 25813); // up/tip (default)
        assert_eq!(place(Direction::Up).rule, PlacementRule::Dripstone);
        assert_eq!(place(Direction::Down).state_id, 25815); // down/tip
        assert_eq!(place(Direction::North).state_id, 25813); // horizontal -> up/tip
    }

    // Multi-cell families (doors + beds): default states unless noted.
    const OAK_DOOR: u32 = 4697; // facing=north, lower, hinge=left, open/powered=false
    const WHITE_BED: u32 = 1734; // facing=north, occupied=false, part=foot
    /// An oak door (south-facing, lower half, hinge=right) used as a right-side
    /// neighbour to drive the hinge-mirror rule.
    const OAK_DOOR_SOUTH_LOWER: u32 = 4717;

    #[test]
    fn door_facing_from_yaw_with_lower_and_upper_halves() {
        // No neighbours -> hinge defaults to right. The lower half lands at the
        // chosen cell; the upper half rides one cell above, both yaw-faced.
        let south =
            compute_placement(&ctx(OAK_DOOR, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(south.rule, PlacementRule::Door);
        assert_eq!(south.state_id, 4717); // facing=south, lower, hinge=right
        assert_eq!(south.place_at, None);
        assert_eq!(south.extra_blocks, vec![(BlockPos::new(0, 65, 0), 4709)]); // upper

        let north =
            compute_placement(&ctx(OAK_DOOR, Direction::Up, 0.5, 180.0), &NoNeighbors).unwrap();
        assert_eq!(north.state_id, 4701); // facing=north, lower, hinge=right
        assert_eq!(north.extra_blocks, vec![(BlockPos::new(0, 65, 0), 4693)]); // upper
    }

    #[test]
    fn door_hinge_flips_left_to_mirror_a_door_on_the_right() {
        // Facing south, the right side is west. An identical lower door there makes
        // the new door hinge left so the pair meets as a matching double door.
        let right_door = MockNeighbors {
            states: vec![(BlockPos::new(-1, 64, 0), OAK_DOOR_SOUTH_LOWER)],
            ..Default::default()
        };
        let placed =
            compute_placement(&ctx(OAK_DOOR, Direction::Up, 0.5, 0.0), &right_door).unwrap();
        assert_eq!(placed.state_id, 4713); // facing=south, lower, hinge=left
        assert_eq!(placed.extra_blocks, vec![(BlockPos::new(0, 65, 0), 4705)]); // upper, hinge=left
    }

    #[test]
    fn bed_foot_and_head_from_yaw() {
        // The foot lands at the chosen cell; the head rides one cell along facing.
        let south =
            compute_placement(&ctx(WHITE_BED, Direction::Up, 0.5, 0.0), &NoNeighbors).unwrap();
        assert_eq!(south.rule, PlacementRule::Bed);
        assert_eq!(south.state_id, 1738); // facing=south, foot
        assert_eq!(south.place_at, None);
        assert_eq!(south.extra_blocks, vec![(BlockPos::new(0, 64, 1), 1737)]); // head one cell south

        let north =
            compute_placement(&ctx(WHITE_BED, Direction::Up, 0.5, 180.0), &NoNeighbors).unwrap();
        assert_eq!(north.state_id, 1734); // facing=north, foot (default)
        assert_eq!(north.extra_blocks, vec![(BlockPos::new(0, 64, -1), 1733)]); // head one cell north
    }
}
