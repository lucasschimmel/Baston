//! Semantic decoding of RAGE sync trees — the piece that turns an opaque clone
//! blob into world state the server can reason about.
//!
//! ## Why this exists
//!
//! [`super::sync_trees`] ports the *structure* of every GTA V sync tree (node
//! order, `NodeIds`, max sizes). This module ports the *traversal* and the
//! handful of leaf decoders the server actually needs, so OneSync-NG can derive
//! an authoritative position and velocity for **every** networked entity —
//! not just the players a client-side shim happens to report.
//!
//! ## The traversal contract (`SyncTrees_Header.h`)
//!
//! Parsing is a preorder walk driven by three integers per node,
//! `NodeIds<Id1, Id2, Id3>`, and two parse parameters:
//!
//! - `sync_type` — `1` on a create record, `2` on a sync record;
//! - `obj_type`  — `0` for a plain entity, `1` for a script entity. Creates are
//!   always parsed as `0`; sync records read it as a **single leading bit**
//!   before the root node.
//!
//! `shouldRead` then decides, in this exact order:
//!
//! 1. `Id1 & sync_type == 0` → skip, consuming **no** bits;
//! 2. `Id3 != 0 && obj_type & Id3 == 0` → skip, consuming **no** bits;
//! 3. `Id2 & sync_type != 0` → consume **one presence bit**; a zero skips.
//!
//! A skipped parent skips its whole subtree without consuming anything. A leaf
//! that is read is framed by a **13-bit bit-length prefix**, so the walk can
//! always resynchronise at `start + length` regardless of whether the leaf
//! decoder understood the payload. That framing is what makes this module safe
//! against hostile input: a malformed or unknown node costs us that node, never
//! the rest of the packet.
//!
//! Note that the 13-bit prefix goes through [`MessageBuffer`]'s length hack
//! exactly like every other 13-bit field, so big-id mode widens it to 16 bits
//! automatically — the caller only has to pass the right flag.
//!
//! ## World position
//!
//! Positions arrive split across two nodes: a coarse sector index and a
//! quantised offset inside that sector. A sync record may carry either, so the
//! server keeps the last-known halves per entity and recombines them with
//! [`world_position`] — the port of the engine's `CalculatePosition`.

use super::clone::NetObjEntityType;
use super::quaternion;
use super::sync_trees::{tree_for, FlatNode, NodeKind};
use super::MessageBuffer;

/// Bit width of the per-leaf length prefix (`Read<uint32_t>(13)`).
const NODE_LENGTH_BITS: usize = 13;

/// Sector indices assumed when an entity has never sent a `CSectorDataNode`.
pub const DEFAULT_SECTOR: [i32; 3] = [512, 512, 0];

/// Metres covered by one sector along X and Y.
const SECTOR_SIZE_XY: f32 = 54.0;
/// Metres covered by one sector along Z.
const SECTOR_SIZE_Z: f32 = 69.0;
/// Sector-space origin offset on X and Y (sector 512 is world zero).
const SECTOR_ORIGIN_XY: f32 = 512.0;
/// World-space Z offset applied after the sector product.
const SECTOR_ORIGIN_Z: f32 = 1700.0;

/// Game build whose node layouts to decode against.
///
/// Some nodes changed shape between GTA V builds — the engine gates those
/// branches on the enforced build, and so must we. The value is the numeric
/// `sv_enforceGameBuild`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GameBuild(pub u32);

impl GameBuild {
    /// From 3717 ("Winter 2025"), ped health fields are 14 bits, not 13.
    ///
    /// The engine has more build gates than the ones declared here; only those
    /// that sit *before* a field we decode matter, because every decoder stops
    /// at the last field it uses and a gate past that point can never change
    /// what we read.
    #[must_use]
    fn is_winter_update_25(self) -> bool {
        self.0 >= 3717
    }

    #[must_use]
    fn is_2060(self) -> bool {
        self.0 >= 2060
    }

    #[must_use]
    fn is_2189(self) -> bool {
        self.0 >= 2189
    }

    #[must_use]
    fn is_2372(self) -> bool {
        self.0 >= 2372
    }

    #[must_use]
    fn is_2545(self) -> bool {
        self.0 >= 2545
    }

    #[must_use]
    fn is_2699(self) -> bool {
        self.0 >= 2699
    }

    #[must_use]
    fn is_3258(self) -> bool {
        self.0 >= 3258
    }

    #[must_use]
    fn is_3407(self) -> bool {
        self.0 >= 3407
    }
}

impl Default for GameBuild {
    /// Baston's own default `sv_enforceGameBuild`.
    fn default() -> Self {
        Self(3258)
    }
}

/// State recovered from one clone record.
///
/// Every field is optional: a sync record only carries the nodes that changed,
/// so an update may refresh the sector without the offset, or the health alone.
/// Callers merge these into their own last-known state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SyncNodeData {
    /// Sector indices from `CSectorDataNode`.
    pub sector: Option<[i32; 3]>,
    /// Quantised offset within the sector, from whichever sector-position node
    /// this entity type uses.
    pub sector_position: Option<[f32; 3]>,
    /// Linear velocity in m/s, from `CPhysicalVelocityDataNode`.
    pub velocity: Option<[f32; 3]>,
    /// Current health, from `CPedHealthDataNode`. Decoding this is what lets
    /// the server hold health authority instead of believing a client report.
    pub health: Option<f32>,
    /// Maximum health that goes with [`Self::health`].
    pub max_health: Option<f32>,
    /// Body armour, from `CPedHealthDataNode`.
    pub armour: Option<f32>,
    /// Model hash, from whichever creation node this entity type uses.
    pub model: Option<u32>,
    /// Vehicle a ped was created inside: `(vehicle object id, seat index)`.
    pub vehicle_seat: Option<(u16, u8)>,
    /// Heading in degrees (0..360), from whichever orientation node this
    /// entity type uses.
    pub heading: Option<f32>,
    /// Engine, doors, lights and locks, from `CVehicleGameStateDataNode`.
    pub vehicle_game_state: Option<VehicleGameState>,
    /// Body/engine/tank health and tyres, from `CVehicleHealthDataNode`.
    pub vehicle_health: Option<VehicleHealth>,
    /// Colours, mods, plate and neons, from `CVehicleAppearanceDataNode`.
    pub vehicle_appearance: Option<VehicleAppearance>,
    /// Bullet and window damage, from `CVehicleDamageStatusDataNode`.
    pub vehicle_damage: Option<VehicleDamage>,
    /// Weapon and vehicle occupancy, from `CPedGameStateDataNode`.
    pub ped_game_state: Option<PedGameState>,
}

/// `CVehicleGameStateDataNode`: everything a script reads about a vehicle's
/// running state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VehicleGameState {
    pub radio_station: u8,
    pub engine_on: bool,
    pub engine_starting: bool,
    pub handbrake: bool,
    /// False when the vehicle overrides its headlight colour.
    pub default_headlights: bool,
    /// Xenon colour index; `0` when the headlights are the model's default.
    pub headlights_colour: u8,
    pub siren_on: bool,
    pub lock_status: u8,
    /// Bitmask of open doors, one bit per door (0..6).
    pub doors_open: u8,
    /// Per-door opening, 0 (shut) to 7 (fully open). Only doors flagged in
    /// [`Self::doors_open`] carry a value.
    pub door_positions: [u8; 7],
    pub is_stationary: bool,
    pub lights_on: bool,
    pub highbeams_on: bool,
    /// `SetVehicleLights` state (3 bits).
    pub light_state: u8,
    pub has_been_owned_by_player: bool,
    pub has_lock: bool,
    /// Bitmask of players the vehicle is locked for; `-1` means everyone.
    pub locked_players: i32,
}

/// `CVehicleHealthDataNode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VehicleHealth {
    pub engine_health: i32,
    pub petrol_tank_health: i32,
    pub health: i32,
    pub body_health: i32,
    /// False when at least one tyre is damaged; only then is
    /// [`Self::tyre_status`] populated.
    pub tyres_fine: bool,
    /// Per-wheel: 0 = intact, 1 = burst, 2 = on the rim.
    pub tyre_status: [u8; 16],
    pub total_repairs: u8,
}

impl Default for VehicleHealth {
    /// The engine's own "undamaged" values — the same ones its parser
    /// substitutes when a health field is absent from the record.
    fn default() -> Self {
        Self {
            engine_health: 1000,
            petrol_tank_health: 1000,
            health: 1000,
            body_health: 1000,
            tyres_fine: true,
            tyre_status: [0; 16],
            total_repairs: 0,
        }
    }
}

/// `CVehicleAppearanceDataNode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VehicleAppearance {
    pub primary_colour: u8,
    pub secondary_colour: u8,
    pub pearl_colour: u8,
    pub wheel_colour: u8,
    pub interior_colour: u8,
    pub dashboard_colour: u8,
    pub is_primary_colour_rgb: bool,
    pub primary_rgb: [u8; 3],
    pub is_secondary_colour_rgb: bool,
    pub secondary_rgb: [u8; 3],
    /// 0..15; the native reports it as a float.
    pub dirt_level: u8,
    /// Bitmask of enabled extras.
    pub extras: u16,
    /// `-1` when the vehicle has no custom livery.
    pub livery_index: i16,
    pub roof_livery_index: i16,
    pub kit_index: u8,
    pub wheel_choice: u8,
    /// `255` when no mod kit is applied.
    pub wheel_type: u8,
    pub window_tint_index: u8,
    pub tyre_smoke_colour: [u8; 3],
    /// Number plate, space-padded exactly as the engine stores it.
    pub plate: [u8; 8],
    pub number_plate_text_index: u32,
    pub horn_type_hash: u32,
    pub has_neon_lights: bool,
    pub neon_colour: [u8; 3],
    /// Left, right, front, back.
    pub neon_sides: [bool; 4],
}

impl Default for VehicleAppearance {
    fn default() -> Self {
        Self {
            primary_colour: 0,
            secondary_colour: 0,
            pearl_colour: 0,
            wheel_colour: 0,
            interior_colour: 0,
            dashboard_colour: 0,
            is_primary_colour_rgb: false,
            primary_rgb: [0; 3],
            is_secondary_colour_rgb: false,
            secondary_rgb: [0; 3],
            dirt_level: 1,
            extras: 0,
            livery_index: -1,
            roof_livery_index: -1,
            kit_index: 0,
            wheel_choice: 0,
            wheel_type: 255,
            window_tint_index: 0,
            tyre_smoke_colour: [255; 3],
            plate: [b' '; 8],
            number_plate_text_index: 0,
            horn_type_hash: 0,
            has_neon_lights: false,
            neon_colour: [0; 3],
            neon_sides: [false; 4],
        }
    }
}

impl VehicleAppearance {
    /// The plate as text, trailing padding removed.
    #[must_use]
    pub fn plate_text(&self) -> String {
        String::from_utf8_lossy(&self.plate).trim_end().to_owned()
    }
}

/// `CVehicleDamageStatusDataNode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VehicleDamage {
    pub damaged_by_bullets: bool,
    pub any_window_broken: bool,
    /// True = that window is broken.
    pub windows_broken: [bool; 8],
}

/// `CPedGameStateDataNode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PedGameState {
    pub cur_weapon: u32,
    /// Object id of the vehicle the ped is in, or `-1`.
    pub cur_vehicle: i32,
    /// Seat index, `-1` when not in a vehicle and `-2` while entering one.
    pub cur_vehicle_seat: i32,
    pub is_handcuffed: bool,
    pub is_flashlight_on: bool,
    pub action_mode_enabled: bool,
    pub stealth_mode_enabled: bool,
    pub arrest_state: u8,
    pub death_state: u8,
}

impl Default for PedGameState {
    fn default() -> Self {
        Self {
            cur_weapon: 0,
            cur_vehicle: -1,
            cur_vehicle_seat: -1,
            is_handcuffed: false,
            is_flashlight_on: false,
            action_mode_enabled: false,
            stealth_mode_enabled: false,
            arrest_state: 0,
            death_state: 0,
        }
    }
}

impl SyncNodeData {
    /// True when the record carried nothing this module understands.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Recombine a sector index and an in-sector offset into world coordinates.
///
/// Port of `SyncTree::GetPosition`:
/// ```text
/// x = (sectorX - 512) * 54 + posX
/// y = (sectorY - 512) * 54 + posY
/// z = (sectorZ * 69 + posZ) - 1700
/// ```
#[must_use]
pub fn world_position(sector: [i32; 3], sector_position: [f32; 3]) -> [f32; 3] {
    [
        (sector[0] as f32 - SECTOR_ORIGIN_XY) * SECTOR_SIZE_XY + sector_position[0],
        (sector[1] as f32 - SECTOR_ORIGIN_XY) * SECTOR_SIZE_XY + sector_position[1],
        (sector[2] as f32 * SECTOR_SIZE_Z + sector_position[2]) - SECTOR_ORIGIN_Z,
    ]
}

/// Decode the kinematic nodes of one clone record.
///
/// `data` is the raw bit-packed sync-tree blob carried by a create or sync
/// record; `length_hack` must match the buffer mode the rest of the clone
/// stream uses, because the leaf length prefix is a 13-bit field.
///
/// Returns whatever could be decoded. A truncated or malformed blob yields the
/// nodes read before the damage rather than an error: the caller's previous
/// state stays valid, and a hostile client can only withhold information, never
/// desynchronise the walk past its own record.
#[must_use]
pub fn parse_sync_tree(
    entity_type: NetObjEntityType,
    is_create: bool,
    data: &[u8],
    length_hack: bool,
    build: GameBuild,
) -> SyncNodeData {
    let mut buf = MessageBuffer::from_bytes(data.to_vec()).with_length_hack(length_hack);
    let mut out = SyncNodeData::default();

    let (sync_type, obj_type) = if is_create {
        // ParseCreate: root.Parse<1, 0>, no leading object-type bit.
        (1_u32, 0_u32)
    } else {
        // ParseSync: the object-type bit precedes the root node.
        (2_u32, u32::from(buf.read_bit()))
    };

    let ctx = WalkContext {
        tree: tree_for(entity_type),
        sync_type,
        obj_type,
        build,
    };
    let mut cursor = 0_usize;
    walk(&ctx, &mut cursor, &mut buf, &mut out);
    out
}

/// Invariants of one traversal, so the recursive walk carries one parameter
/// instead of four.
struct WalkContext {
    tree: &'static [FlatNode],
    sync_type: u32,
    obj_type: u32,
    build: GameBuild,
}

/// Preorder walk of one node and, for parents, its subtree.
///
/// Returns `false` once the buffer is exhausted so the caller stops early
/// instead of spinning over a truncated blob.
fn walk(
    ctx: &WalkContext,
    cursor: &mut usize,
    buf: &mut MessageBuffer,
    out: &mut SyncNodeData,
) -> bool {
    let Some(node) = ctx.tree.get(*cursor).copied() else {
        return false;
    };
    let depth = node.depth;
    *cursor += 1;

    let read = should_read(node.ids, ctx.sync_type, ctx.obj_type, buf);

    match node.kind {
        NodeKind::Parent => {
            if read {
                while ctx
                    .tree
                    .get(*cursor)
                    .is_some_and(|child| child.depth > depth)
                {
                    if !walk(ctx, cursor, buf, out) {
                        return false;
                    }
                }
            } else {
                skip_subtree(ctx.tree, cursor, depth);
            }
            true
        }
        NodeKind::Data { name, .. } => {
            if !read {
                return true;
            }
            let Some(length) = buf.read_bits_single(NODE_LENGTH_BITS) else {
                return false;
            };
            let start = buf.current_bit();
            let length = length as usize;
            let end = start + length;
            if end > buf.buffer().len() * 8 {
                return false;
            }
            if decodes(name) {
                // Decode from a view bounded to this node's declared length, so
                // a truncated or empty node yields nothing rather than reading
                // fields out of whatever follows it.
                let mut body = vec![0_u8; length.div_ceil(8)];
                if buf.read_bits(&mut body, length) {
                    let mut view = MessageBuffer::from_bits(body, length);
                    decode_leaf(name, &mut view, ctx.build, out);
                }
            }
            // Resynchronise unconditionally: an unknown or partially understood
            // leaf must not shift the rest of the walk.
            buf.set_current_bit(end);
            true
        }
    }
}

/// Advance `cursor` past every descendant of a node at `depth`, without
/// touching the bit stream — a subtree whose parent was skipped contributes no
/// bits at all.
fn skip_subtree(tree: &[FlatNode], cursor: &mut usize, depth: u8) {
    while tree.get(*cursor).is_some_and(|child| child.depth > depth) {
        *cursor += 1;
    }
}

/// Port of `shouldRead<syncType, objType, TIds>`. The presence bit is consumed
/// only when the first two gates pass, and only when `Id2 & sync_type` is set —
/// order matters, an early presence read would desync the stream.
fn should_read(ids: (u8, u8, u8), sync_type: u32, obj_type: u32, buf: &mut MessageBuffer) -> bool {
    let (id1, id2, id3) = (u32::from(ids.0), u32::from(ids.1), u32::from(ids.2));
    if id1 & sync_type == 0 {
        return false;
    }
    if id3 != 0 && obj_type & id3 == 0 {
        return false;
    }
    if id2 & sync_type != 0 && buf.read_bit() == 0 {
        return false;
    }
    true
}

/// Decode the leaves this server understands. Everything else is intentionally
/// ignored — the caller resynchronises from the length prefix either way, so
/// adding a node here is purely additive and can never break the walk.
///
/// ## Decoding policy: read the useful prefix, then stop
///
/// Because the walk resynchronises on the length prefix, a decoder is free to
/// read only the leading fields it needs and return. Several nodes end in
/// branches whose exact predicate is ambiguous in the engine source (signed
/// enum arithmetic, undocumented build gates); reading past a field we care
/// about buys nothing and risks misreading. So each decoder below stops at the
/// last field it actually uses.
/// Whether [`decode_leaf`] understands this node. Checked first so the walk
/// does not copy the body of the seventy-odd nodes it ignores.
fn decodes(name: &str) -> bool {
    matches!(
        name,
        "CSectorDataNode"
            | "CSectorPositionDataNode"
            | "CPlayerSectorPosNode"
            | "CObjectSectorPosNode"
            | "CPedSectorPosMapNode"
            | "CPedHealthDataNode"
            | "CVehicleCreationDataNode"
            | "CPedCreationDataNode"
            | "CEntityOrientationDataNode"
            | "CObjectOrientationDataNode"
            | "CPedOrientationDataNode"
            | "CPhysicalVelocityDataNode"
            | "CVehicleGameStateDataNode"
            | "CVehicleHealthDataNode"
            | "CVehicleAppearanceDataNode"
            | "CVehicleDamageStatusDataNode"
            | "CPedGameStateDataNode"
    )
}

fn decode_leaf(name: &str, buf: &mut MessageBuffer, build: GameBuild, out: &mut SyncNodeData) {
    match name {
        "CSectorDataNode" => {
            if let (Some(x), Some(y), Some(z)) = (
                buf.read_bits_single(10),
                buf.read_bits_single(10),
                buf.read_bits_single(6),
            ) {
                out.sector = Some([x as i32, y as i32, z as i32]);
            }
        }
        // Vehicles, and every entity using the plain sector-position node.
        "CSectorPositionDataNode" => {
            out.sector_position = read_sector_offset(buf, 12);
        }
        // Players: optional "standing on" preamble, then the same offset.
        "CPlayerSectorPosNode" => {
            if buf.read_bit() != 0 {
                // Two engine-internal flag bits, then the standing-on block.
                buf.read_bit();
                buf.read_bit();
                if buf.read_bit() != 0 {
                    // Standing-on handle and offset. Read to stay aligned; the
                    // offset is deliberately not applied — resolving it needs
                    // the carrier entity, which the caller owns, and the raw
                    // sector position is already correct for interest purposes.
                    buf.read_bits_single(13);
                    buf.read_signed_float(14, 40.0);
                    buf.read_signed_float(14, 40.0);
                    buf.read_signed_float(10, 20.0);
                }
            }
            out.sector_position = read_sector_offset(buf, 12);
        }
        // Objects: a leading bit selects 20-bit high-resolution encoding.
        "CObjectSectorPosNode" => {
            let bits = if buf.read_bit() != 0 { 20 } else { 12 };
            out.sector_position = read_sector_offset(buf, bits);
        }
        // Peds on the map: offset first, extra data after.
        "CPedSectorPosMapNode" => {
            out.sector_position = read_sector_offset(buf, 12);
        }
        // Health authority. Decoding this is what lets the server know a
        // ped's real health instead of trusting whatever the client reports
        // out-of-band — the difference between an anti-cheat and a rumour.
        "CPedHealthDataNode" => {
            let health_bits = if build.is_winter_update_25() { 14 } else { 13 };
            let is_fine = buf.read_bit() != 0;
            let max_health_changed = buf.read_bit() != 0;
            let max_health = if max_health_changed {
                buf.read_bits_single(health_bits)
            } else {
                // Unchanged: the engine keeps its previous value, defaulting
                // to 200. The caller merges, so reporting nothing is right.
                None
            };
            let health = if is_fine {
                // "Fine" means at full health, whatever the maximum is.
                max_health
            } else {
                let value = buf.read_bits_single(health_bits);
                buf.read_bit(); // killed with headshot
                buf.read_bit(); // killed with melee
                value
            };
            let no_armour = buf.read_bit() != 0;
            let armour = if no_armour {
                Some(0)
            } else {
                buf.read_bits_single(13)
            };
            out.health = health.map(|v| v as f32);
            out.max_health = max_health.map(|v| v as f32);
            out.armour = armour.map(|v| v as f32);
        }
        // Model only. The remaining fields sit behind a population-type
        // predicate that is ambiguous in the engine source, and we need none
        // of them.
        "CVehicleCreationDataNode" => {
            out.model = buf.read_bits_single(32);
        }
        // Model, plus the vehicle the ped spawned inside — the only place the
        // server learns ped-to-vehicle occupancy at creation time.
        "CPedCreationDataNode" => {
            buf.read_bit(); // is respawn object id
            buf.read_bit(); // respawn flagged for removal
            buf.read_bits_single(4); // population type
            out.model = buf.read_bits_single(32);
            buf.read_bits_single(16); // random seed
            let in_vehicle = buf.read_bit() != 0;
            buf.read_bits_single(32); // voice hash
            if in_vehicle {
                if let (Some(vehicle), Some(seat)) =
                    (buf.read_bits_single(13), buf.read_bits_single(5))
                {
                    out.vehicle_seat = Some((vehicle as u16, seat as u8));
                }
            }
        }
        // Vehicles and most entities: a compressed quaternion.
        "CEntityOrientationDataNode" => {
            out.heading = read_quaternion_heading(buf);
        }
        // Objects: a leading bit selects between three raw angles and the
        // same compressed quaternion.
        "CObjectOrientationDataNode" => {
            if buf.read_bit() != 0 {
                // High-resolution: three signed angles over ±4π.
                let divisor = std::f32::consts::PI * 4.0;
                buf.read_signed_float(20, divisor);
                buf.read_signed_float(20, divisor);
                out.heading = buf
                    .read_signed_float(20, divisor)
                    .map(|z| normalise_degrees(z.to_degrees()));
            } else {
                out.heading = read_quaternion_heading(buf);
            }
        }
        // Peds: the current heading as a quantised angle, then the desired one.
        "CPedOrientationDataNode" => {
            out.heading = buf
                .read_signed_float(8, quaternion::TAU_RADIANS)
                .map(|radians| normalise_degrees(radians.to_degrees()));
        }
        "CPhysicalVelocityDataNode" => {
            if let (Some(x), Some(y), Some(z)) = (
                buf.read_signed(12),
                buf.read_signed(12),
                buf.read_signed(12),
            ) {
                const QUANTUM: f32 = 0.0625;
                out.velocity = Some([x as f32 * QUANTUM, y as f32 * QUANTUM, z as f32 * QUANTUM]);
            }
        }
        "CVehicleGameStateDataNode" => {
            out.vehicle_game_state = decode_vehicle_game_state(buf, build);
        }
        "CVehicleHealthDataNode" => {
            out.vehicle_health = decode_vehicle_health(buf, build);
        }
        "CVehicleAppearanceDataNode" => {
            out.vehicle_appearance = decode_vehicle_appearance(buf, build);
        }
        "CVehicleDamageStatusDataNode" => {
            out.vehicle_damage = decode_vehicle_damage(buf);
        }
        "CPedGameStateDataNode" => {
            out.ped_game_state = decode_ped_game_state(buf, build);
        }
        _ => {}
    }
}

/// `CVehicleGameStateDataNode::Parse`.
///
/// The node is long and mostly opaque, but the fields scripts read are spread
/// across it, so the port has to walk the variable-length middle faithfully
/// rather than stop early. Every read is checked: the first truncated field
/// abandons the node and the walk resynchronises on the length prefix.
///
/// Fields are assigned in sequence rather than through a struct literal: the
/// order here **is** the wire order, and a literal would let a future edit
/// reorder it silently.
#[allow(clippy::field_reassign_with_default)]
fn decode_vehicle_game_state(
    buf: &mut MessageBuffer,
    build: GameBuild,
) -> Option<VehicleGameState> {
    let mut out = VehicleGameState::default();

    out.radio_station = buf.read_bits_single(if build.is_2545() { 7 } else { 6 })? as u8;
    buf.read_bit();
    out.engine_on = buf.read_bit() != 0;
    out.engine_starting = buf.read_bit() != 0;
    buf.read_bit();
    out.handbrake = buf.read_bit() != 0;
    buf.read_bit();
    buf.read_bit();
    // Set when the vehicle publishes no detailed state at all; the engine then
    // substitutes the defaults mirrored below.
    let state_absent = buf.read_bit() != 0;

    if state_absent {
        out.default_headlights = true;
    } else {
        out.default_headlights = buf.read_bit() != 0;
        if !out.default_headlights {
            out.headlights_colour = buf.read_bits_single(8)? as u8;
        }
        out.siren_on = buf.read_bit() != 0;
        if build.is_3407() {
            buf.read_bit();
        }
        if buf.read_bit() != 0 {
            buf.read_bit();
        }
        buf.read_bit();
        if buf.read_bit() != 0 {
            out.lock_status = buf.read_bits_single(5)? as u8;
            buf.read_bits_single(7)?;
            buf.read_bits_single(7)?; // unbreakable doors
            let doors_open = buf.read_bits_single(7)? as u8;
            out.doors_open = doors_open;
            for (door, position) in out.door_positions.iter_mut().enumerate() {
                if doors_open & (1 << door) != 0 {
                    *position = buf.read_bits_single(4)? as u8;
                }
            }
            // Eight bits are read, but the engine only walks the low seven.
            let extra_mask = buf.read_bits_single(8)?;
            for slot in 0..7 {
                if extra_mask & (1 << slot) != 0 {
                    buf.read_bits_single(5)?;
                }
            }
        }
        if buf.read_bit() != 0 {
            buf.read_bits_single(6)?; // open windows
        }
        buf.read_bit();
        buf.read_bit();
        out.is_stationary = buf.read_bit() != 0;
        buf.read_bit(); // is parked
        buf.read_bit();
        buf.read_bit();
        if buf.read_bit() != 0 {
            buf.read_float(10, 3000.0)?;
        }
    }

    if buf.read_bit() != 0 {
        buf.read_bits_single(32)?;
        buf.read_bits_single(13)?;
    }

    // Two 13-bit ids when present. The engine reads them with a counter it
    // decrements to zero, then compares the lock flags below against that
    // counter — which is therefore always zero by then. Reproduced as a plain
    // `!= 0` rather than the counter, because that is what it evaluates to.
    if buf.read_bit() != 0 {
        buf.read_bits_single(13)?;
        buf.read_bits_single(13)?;
    }

    if buf.read_bit() != 0 {
        buf.read_bits_single(13)?;
    }

    out.lights_on = buf.read_bit() != 0;
    out.highbeams_on = buf.read_bit() != 0;
    out.light_state = buf.read_bits_single(3)? as u8;

    for _ in 0..6 {
        buf.read_bit();
    }
    buf.read_bits_single(32)?;
    buf.read_bits_single(3)?;
    buf.read_bit();
    out.has_been_owned_by_player = buf.read_bit() != 0;

    for _ in 0..6 {
        buf.read_bit();
    }
    if build.is_2699() {
        buf.read_bit();
        buf.read_bit();
    }

    out.has_lock = buf.read_bit() != 0;
    if out.has_lock {
        out.locked_players = buf.read_bits_single(32)? as i32;
    }

    Some(out)
}

/// `CVehicleHealthDataNode::Parse`.
fn decode_vehicle_health(buf: &mut MessageBuffer, build: GameBuild) -> Option<VehicleHealth> {
    let mut out = VehicleHealth::default();

    buf.read_bit();
    buf.read_bit();
    let engine_damaged = buf.read_bit() != 0;
    let petrol_tank_damaged = buf.read_bit() != 0;

    if engine_damaged {
        out.engine_health = buf.read_signed(19)?;
    }
    if petrol_tank_damaged {
        out.petrol_tank_health = buf.read_signed(19)?;
    }

    out.tyres_fine = buf.read_bit() != 0;
    let wheel_extra_fine = buf.read_bit() != 0;

    if !out.tyres_fine || !wheel_extra_fine {
        let total_wheels = buf.read_bits_single(4)? as usize;
        if !out.tyres_fine {
            for wheel in 0..total_wheels {
                if build.is_2060() && buf.read_bit() != 0 {
                    buf.read_bits_single(8)?;
                }
                let burst = buf.read_bit() != 0;
                let on_rim = buf.read_bit() != 0;
                buf.read_bit();
                buf.read_bit();
                if let Some(slot) = out.tyre_status.get_mut(wheel) {
                    *slot = if on_rim {
                        2
                    } else if burst {
                        1
                    } else {
                        0
                    };
                }
            }
        }
        if !wheel_extra_fine {
            for _ in 0..total_wheels {
                if buf.read_bit() != 0 {
                    buf.read_bits_single(10)?;
                }
            }
        }
    }

    if buf.read_bit() == 0 {
        out.health = buf.read_signed(19)?;
    }
    if buf.read_bit() == 0 {
        out.body_health = buf.read_signed(19)?;
    }

    if buf.read_bit() != 0 {
        buf.read_bits_single(13)?; // damaging entity
        buf.read_bits_single(32)?; // last damage source
    }

    buf.read_bits_single(4)?;
    out.total_repairs = buf.read_bits_single(4)? as u8;

    Some(out)
}

/// `CVehicleAppearanceDataNode::Serialize`.
///
/// Assigned in sequence for the same reason as
/// [`decode_vehicle_game_state`]: the order is the wire format.
#[allow(clippy::field_reassign_with_default)]
fn decode_vehicle_appearance(
    buf: &mut MessageBuffer,
    build: GameBuild,
) -> Option<VehicleAppearance> {
    let mut out = VehicleAppearance::default();

    out.primary_colour = buf.read_bits_single(8)? as u8;
    out.secondary_colour = buf.read_bits_single(8)? as u8;
    out.pearl_colour = buf.read_bits_single(8)? as u8;
    out.wheel_colour = buf.read_bits_single(8)? as u8;
    out.interior_colour = buf.read_bits_single(8)? as u8;
    out.dashboard_colour = buf.read_bits_single(8)? as u8;

    out.is_primary_colour_rgb = buf.read_bit() != 0;
    if out.is_primary_colour_rgb {
        for channel in &mut out.primary_rgb {
            *channel = buf.read_bits_single(8)? as u8;
        }
    }
    out.is_secondary_colour_rgb = buf.read_bit() != 0;
    if out.is_secondary_colour_rgb {
        for channel in &mut out.secondary_rgb {
            *channel = buf.read_bits_single(8)? as u8;
        }
    }

    buf.read_bits_single(8)?; // environment effect scale

    if buf.read_bit() != 0 {
        out.dirt_level = buf.read_bits_single(5)? as u8;
        out.extras = buf.read_bits_single(16)? as u16;
        out.livery_index = if buf.read_bit() != 0 {
            buf.read_bits_single(5)? as i16
        } else {
            0
        };
        out.roof_livery_index = if buf.read_bit() != 0 {
            buf.read_bits_single(5)? as i16
        } else {
            0
        };
    }

    out.kit_index = buf.read_bits_single(2)? as u8;
    if out.kit_index != 0 {
        for _ in 0..13 {
            if buf.read_bit() != 0 {
                buf.read_bits_single(32)?;
            }
        }
        if buf.read_bit() != 0 {
            buf.read_bits_single(6)?; // toggle mods
        }
        out.wheel_choice = buf.read_bits_single(8)? as u8;
        out.wheel_type = buf.read_bits_single(4)? as u8;
        if buf.read_bit() != 0 {
            buf.read_bits_single(8)?; // rear wheel choice
        }
        buf.read_bit(); // custom tyres
        buf.read_bit(); // wheel variation
    }

    if buf.read_bit() != 0 {
        out.window_tint_index = buf.read_bits_single(8)? as u8;
    }

    if buf.read_bit() != 0 {
        for channel in &mut out.tyre_smoke_colour {
            *channel = buf.read_bits_single(8)? as u8;
        }
    }

    // The eight plate characters are always framed; the flag only decides
    // whether they carry text or padding.
    let has_plate = buf.read_bit() != 0;
    for character in &mut out.plate {
        if has_plate {
            *character = buf.read_bits_single(7)? as u8;
        }
    }

    out.number_plate_text_index = buf.read_bits_single(32)?;
    out.horn_type_hash = buf.read_bits_single(32)?;

    if buf.read_bit() != 0 {
        // Emblems: a crew emblem or a texture pair, then up to four badges.
        if buf.read_bit() != 0 {
            buf.read_bits_single(32)?; // txd name
            buf.read_bits_single(32)?; // texture name
        } else {
            buf.read_bits_single(1)?;
            buf.read_bits_single(32)?;
            buf.read_bit();
            buf.read_bits_single(3)?;
        }
        for _ in 0..4 {
            if buf.read_bit() != 0 {
                buf.read_bits_single(10)?; // bone index
                buf.read_bits_single(8)?; // alpha
                for _ in 0..3 {
                    // offset, direction, side: two 14-bit axes then a 10-bit one
                    buf.read_bits_single(14)?;
                    buf.read_bits_single(14)?;
                    buf.read_bits_single(10)?;
                }
                buf.read_bits_single(16)?; // size
            }
        }
    }

    out.has_neon_lights = buf.read_bit() != 0;
    if out.has_neon_lights {
        for channel in &mut out.neon_colour {
            *channel = buf.read_bits_single(8)? as u8;
        }
        for side in &mut out.neon_sides {
            *side = buf.read_bit() != 0;
        }
        if build.is_2372() {
            buf.read_bit(); // neon suppressed
        }
    }

    Some(out)
}

/// `CVehicleDamageStatusDataNode::Parse`.
fn decode_vehicle_damage(buf: &mut MessageBuffer) -> Option<VehicleDamage> {
    let mut out = VehicleDamage::default();

    if buf.read_bit() != 0 {
        for _ in 0..6 {
            buf.read_bits_single(2)?; // per-panel deformation level
        }
    }

    out.damaged_by_bullets = buf.read_bit() != 0;
    if out.damaged_by_bullets {
        for _ in 0..6 {
            buf.read_bits_single(8)?;
        }
    }

    if buf.read_bit() != 0 {
        buf.read_bits_single(2)?; // front bumper
        buf.read_bits_single(2)?; // rear bumper
    }

    if buf.read_bit() != 0 {
        for _ in 0..22 {
            buf.read_bit();
        }
    }

    out.any_window_broken = buf.read_bit() != 0;
    for window in &mut out.windows_broken {
        *window = out.any_window_broken && buf.read_bit() != 0;
    }

    Some(out)
}

/// `CPedGameStateDataNode::Parse`.
///
/// The engine's node carries the vehicle a ped occupies — the only
/// authoritative source for the whole `*_PED_IN_VEHICLE_*` native family.
fn decode_ped_game_state(buf: &mut MessageBuffer, build: GameBuild) -> Option<PedGameState> {
    let mut out = PedGameState::default();

    for _ in 0..6 {
        buf.read_bit();
    }
    if build.is_2060() {
        buf.read_bit();
        buf.read_bit();
        if build.is_2189() {
            buf.read_bit();
        }
        if build.is_2372() {
            buf.read_bit();
        }
        if build.is_3407() {
            buf.read_bit();
            buf.read_bit();
        }
    }

    out.arrest_state = buf.read_bits_single(1)? as u8;
    out.death_state = buf.read_bits_single(2)? as u8;

    if buf.read_bit() != 0 {
        out.cur_weapon = buf.read_bits_single(32)?;
        if build.is_3258() {
            buf.read_bits_single(3)?; // weapon state
        }
    }

    if build.is_2060() {
        buf.read_bit();
    }

    for _ in 0..5 {
        buf.read_bit(); // weapon exists/visible/has ammo/attach left/unknown
    }

    if buf.read_bit() != 0 {
        buf.read_bits_single(5)?; // weapon tint
    }

    let weapon_components = buf.read_bits_single(4)?;
    for _ in 0..weapon_components {
        buf.read_bits_single(32)?;
        if build.is_2372() && buf.read_bit() != 0 {
            buf.read_bits_single(5)?;
        }
    }

    let gadgets = buf.read_bits_single(2)?;
    for _ in 0..gadgets {
        buf.read_bits_single(32)?;
    }

    if buf.read_bit() != 0 {
        let vehicle = buf.read_bits_single(13)? as i32;
        if buf.read_bit() != 0 {
            out.cur_vehicle = vehicle;
            out.cur_vehicle_seat = buf.read_bits_single(5)? as i32;
        } else {
            // Entering or leaving: the engine reports the vehicle as gone and
            // remembers it as the last one instead. The caller owns that
            // merge, because it needs the previous record to do it.
            out.cur_vehicle = -1;
            out.cur_vehicle_seat = -1;
        }
    }

    if buf.read_bit() != 0 {
        buf.read_bit();
    }

    if buf.read_bit() != 0 {
        buf.read_bits_single(13)?; // custodian
        out.is_handcuffed = buf.read_bit() != 0;
        for _ in 0..4 {
            buf.read_bit(); // arrest capability flags
        }
    }

    out.is_flashlight_on = buf.read_bit() != 0;
    out.action_mode_enabled = buf.read_bit() != 0;
    out.stealth_mode_enabled = buf.read_bit() != 0;

    Some(out)
}

/// Read a compressed quaternion and return the heading it encodes, in degrees.
fn read_quaternion_heading(buf: &mut MessageBuffer) -> Option<f32> {
    let compressed = quaternion::CompressedQuaternion {
        largest: buf.read_bits_single(2)?,
        a: buf.read_bits_single(quaternion::BITS as usize)?,
        b: buf.read_bits_single(quaternion::BITS as usize)?,
        c: buf.read_bits_single(quaternion::BITS as usize)?,
    };
    Some(quaternion::to_heading_degrees(quaternion::decompress(
        compressed,
        quaternion::BITS,
    )))
}

/// Fold an angle into the 0..360 range GTA headings use.
fn normalise_degrees(degrees: f32) -> f32 {
    degrees.rem_euclid(360.0)
}

/// Read the three quantised in-sector coordinates. X and Y span one 54 m
/// sector, Z spans 69 m.
fn read_sector_offset(buf: &mut MessageBuffer, bits: usize) -> Option<[f32; 3]> {
    let x = buf.read_float(bits, SECTOR_SIZE_XY)?;
    let y = buf.read_float(bits, SECTOR_SIZE_XY)?;
    let z = buf.read_float(bits, SECTOR_SIZE_Z)?;
    Some([x, y, z])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writer mirroring the reader's framing, so tests exercise the real
    /// traversal instead of a hand-built bit string.
    struct TreeWriter {
        buf: MessageBuffer,
    }

    impl TreeWriter {
        fn new() -> Self {
            Self {
                buf: MessageBuffer::new(512),
            }
        }

        /// Emit the presence bit for a node whose `Id2 & sync_type` is set.
        fn present(&mut self, present: bool) {
            self.buf.write_bit(present);
        }

        /// Emit a leaf: 13-bit bit-length prefix followed by `body`.
        fn leaf(&mut self, body: &dyn Fn(&mut MessageBuffer)) {
            let mut scratch = MessageBuffer::new(256);
            body(&mut scratch);
            let bits = scratch.current_bit();
            self.buf.write_bits_single(bits as u32, NODE_LENGTH_BITS);
            let start = self.buf.current_bit();
            self.buf.write_bits(scratch.buffer(), bits);
            assert_eq!(self.buf.current_bit(), start + bits);
        }

        fn finish(self) -> Vec<u8> {
            let len = self.buf.data_length();
            let mut bytes = self.buf.into_inner();
            bytes.truncate(len.max(1));
            bytes
        }
    }

    fn sector_body(x: u32, y: u32, z: u32) -> impl Fn(&mut MessageBuffer) {
        move |b: &mut MessageBuffer| {
            b.write_bits_single(x, 10);
            b.write_bits_single(y, 10);
            b.write_bits_single(z, 6);
        }
    }

    fn offset_body(x: f32, y: f32, z: f32) -> impl Fn(&mut MessageBuffer) {
        move |b: &mut MessageBuffer| {
            b.write_float(12, SECTOR_SIZE_XY, x);
            b.write_float(12, SECTOR_SIZE_XY, y);
            b.write_float(12, SECTOR_SIZE_Z, z);
        }
    }

    #[test]
    fn world_position_matches_engine_formula() {
        // Sector origin with a zero offset sits at world (0, 0, -1700).
        assert_eq!(world_position([512, 512, 0], [0.0; 3]), [0.0, 0.0, -1700.0]);
        // One sector east and north, plus a half-sector offset.
        let p = world_position([513, 511, 25], [27.0, 27.0, 34.5]);
        assert!((p[0] - 81.0).abs() < 0.001, "x = {}", p[0]);
        assert!((p[1] + 27.0).abs() < 0.001, "y = {}", p[1]);
        // 25 * 69 + 34.5 - 1700
        assert!((p[2] - 59.5).abs() < 0.001, "z = {}", p[2]);
    }

    /// Walk the real `PLAYER_TREE` on a sync record carrying only the sector
    /// and sector-position nodes. Every gate before them must be honoured for
    /// the decoder to land on the right bits.
    #[test]
    fn player_sync_yields_position() {
        let mut w = TreeWriter::new();
        // Leading object-type bit: plain entity.
        w.present(false);
        // p(0, (127,0,0))  — Id2 = 0, no presence bit.
        // p(1, (1,0,0))    — Id1 & 2 == 0, skipped with no bits.
        // p(1, (127,86,0)) — 86 & 2 != 0, presence bit: absent.
        w.present(false);
        // p(1, (127,86,0)) — the movement/position parent: present.
        w.present(true);
        //   d(2, (87,87,0)) CPedOrientationDataNode — absent.
        w.present(false);
        //   d(2, (87,87,0)) CPedMovementDataNode — absent.
        w.present(false);
        //   p(2, (127,87,0)) task parent — absent.
        w.present(false);
        //   d(2, (87,87,0)) CSectorDataNode — present.
        w.present(true);
        w.leaf(&sector_body(600, 400, 30));
        //   d(2, (87,87,0)) CPlayerSectorPosNode — present.
        w.present(true);
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bit(false); // no extra data
            offset_body(10.0, 20.0, 30.0)(b);
        });
        //   d(2, (86,86,0)) CPlayerCameraDataNode — 86 & 2 != 0, absent.
        w.present(false);
        //   d(2, (86,86,0)) CPlayerWantedAndLOSDataNode — absent.
        w.present(false);
        // p(1, (4,0,0)) migration parent — 4 & 2 == 0, skipped with no bits.

        let k = parse_sync_tree(
            NetObjEntityType::Player,
            false,
            &w.finish(),
            false,
            GameBuild::default(),
        );

        assert_eq!(k.sector, Some([600, 400, 30]));
        let offset = k.sector_position.expect("sector position decoded");
        assert!((offset[0] - 10.0).abs() < 0.05, "x = {}", offset[0]);
        assert!((offset[1] - 20.0).abs() < 0.05, "y = {}", offset[1]);
        assert!((offset[2] - 30.0).abs() < 0.05, "z = {}", offset[2]);

        let world = world_position(k.sector.unwrap(), offset);
        assert!((world[0] - 4762.0).abs() < 0.05, "world x = {}", world[0]);
        assert!((world[1] + 6028.0).abs() < 0.05, "world y = {}", world[1]);
        assert!((world[2] - 400.0).abs() < 0.05, "world z = {}", world[2]);
    }

    /// A player standing on another entity emits a longer node; the offset must
    /// still be found, which only works if the preamble is consumed exactly.
    #[test]
    fn player_standing_on_preamble_is_consumed() {
        let mut w = TreeWriter::new();
        w.present(false); // object type
        w.present(false); // p(1,(127,86,0)) game-state parent absent
        w.present(true); // position parent present
        w.present(false); // orientation
        w.present(false); // movement
        w.present(false); // task parent
        w.present(false); // CSectorDataNode absent — offset only
        w.present(true); // CPlayerSectorPosNode present
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bit(true); // has extra data
            b.write_bit(false);
            b.write_bit(false);
            b.write_bit(true); // is standing on
            b.write_bits_single(1234, 13);
            b.write_signed_float(14, 40.0, 1.5);
            b.write_signed_float(14, 40.0, -2.5);
            b.write_signed_float(10, 20.0, 0.5);
            offset_body(42.0, 8.0, 12.0)(b);
        });
        w.present(false);
        w.present(false);

        let k = parse_sync_tree(
            NetObjEntityType::Player,
            false,
            &w.finish(),
            false,
            GameBuild::default(),
        );

        assert_eq!(k.sector, None, "no sector node was sent");
        let offset = k.sector_position.expect("offset decoded past the preamble");
        assert!((offset[0] - 42.0).abs() < 0.05, "x = {}", offset[0]);
        assert!((offset[1] - 8.0).abs() < 0.05, "y = {}", offset[1]);
        assert!((offset[2] - 12.0).abs() < 0.05, "z = {}", offset[2]);
    }

    /// Vehicles carry position *and* velocity in the same parent.
    #[test]
    fn automobile_sync_yields_position_and_velocity() {
        let mut w = TreeWriter::new();
        w.present(false); // object type
                          // p(1,(1,0,0)) creation parent — 1 & 2 == 0, skipped with no bits.
                          // p(1,(127,127,0)) game-state parent — absent. Its subtree covers the
                          // attach / appearance / damage / reservation / health / task leaves, so
                          // one bit suppresses all of them.
        w.present(false);
        // p(1,(127,86,0)) position parent — 86 & 2 != 0, present.
        w.present(true);
        w.present(true); // CSectorDataNode
        w.leaf(&sector_body(500, 520, 10));
        w.present(true); // CSectorPositionDataNode
        w.leaf(&offset_body(1.0, 2.0, 3.0));
        w.present(false); // CEntityOrientationDataNode
        w.present(true); // CPhysicalVelocityDataNode
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_signed(160, 12); // 160 * 0.0625 = 10 m/s
            b.write_signed(-80, 12); // -5 m/s
            b.write_signed(0, 12);
        });
        w.present(false); // CVehicleAngVelocityDataNode
        w.present(false); // p(2,(127,86,0)) control parent

        let k = parse_sync_tree(
            NetObjEntityType::Automobile,
            false,
            &w.finish(),
            false,
            GameBuild::default(),
        );

        assert_eq!(k.sector, Some([500, 520, 10]));
        assert!(k.sector_position.is_some());
        let v = k.velocity.expect("velocity decoded");
        assert!((v[0] - 10.0).abs() < 0.2, "vx = {}", v[0]);
        // The ported signed codec is deliberately asymmetric for negatives
        // (see `buffer::tests::signed_matches_cpp_asymmetry`), so allow one
        // quantum of drift rather than an exact match.
        assert!((v[1] + 5.0).abs() < 0.2, "vy = {}", v[1]);
        assert!(v[2].abs() < 0.2, "vz = {}", v[2]);
    }

    /// A create record has no leading object-type bit and uses `sync_type = 1`,
    /// which gates a different set of nodes.
    #[test]
    fn create_record_skips_the_object_type_bit() {
        let mut w = TreeWriter::new();
        // p(1,(1,0,0)) creation parent — Id1 & 1 != 0, Id2 = 0: always read.
        //   d(2,(1,0,0)) CObjectCreationDataNode — no presence bit either.
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bits_single(0xABCD, 32);
        });
        // p(1,(127,127,0)) game-state parent — 127 & 1 != 0, presence bit. Its
        // subtree holds the attach and health leaves, so one bit skips them.
        w.present(false);
        // p(1,(87,87,0)) position parent — 87 & 1 != 0, presence bit.
        w.present(true);
        w.present(true); // CSectorDataNode
        w.leaf(&sector_body(512, 512, 0));
        w.present(true); // CObjectSectorPosNode
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bit(false); // low resolution
            offset_body(5.0, 6.0, 7.0)(b);
        });

        let k = parse_sync_tree(
            NetObjEntityType::Object,
            true,
            &w.finish(),
            false,
            GameBuild::default(),
        );

        assert_eq!(k.sector, Some([512, 512, 0]));
        let offset = k.sector_position.expect("object offset decoded");
        assert!((offset[0] - 5.0).abs() < 0.05, "x = {}", offset[0]);
    }

    /// High-resolution objects switch every coordinate to 20 bits.
    #[test]
    fn object_high_resolution_position() {
        let mut w = TreeWriter::new();
        w.present(false); // object type
        w.present(false); // game-state parent (covers attach + health)
        w.present(true); // position parent
        w.present(false); // CSectorDataNode absent
        w.present(true); // CObjectSectorPosNode
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bit(true); // high resolution
            b.write_float(20, SECTOR_SIZE_XY, 13.25);
            b.write_float(20, SECTOR_SIZE_XY, 41.75);
            b.write_float(20, SECTOR_SIZE_Z, 60.5);
        });

        let k = parse_sync_tree(
            NetObjEntityType::Object,
            false,
            &w.finish(),
            false,
            GameBuild::default(),
        );
        let offset = k.sector_position.expect("high-res offset decoded");
        assert!((offset[0] - 13.25).abs() < 0.01, "x = {}", offset[0]);
        assert!((offset[1] - 41.75).abs() < 0.01, "y = {}", offset[1]);
        assert!((offset[2] - 60.5).abs() < 0.01, "z = {}", offset[2]);
    }

    /// An unknown leaf must cost only itself: the walk resynchronises on the
    /// length prefix and still finds the nodes after it.
    #[test]
    fn unknown_leaf_does_not_desync_the_walk() {
        let mut w = TreeWriter::new();
        w.present(false); // object type
        w.present(false); // game-state parent (covers the six depth-2 leaves)
        w.present(true); // position parent
        w.present(true); // CSectorDataNode
        w.leaf(&sector_body(300, 300, 5));
        w.present(true); // CSectorPositionDataNode
        w.leaf(&offset_body(4.0, 4.0, 4.0));
        w.present(true); // CEntityOrientationDataNode — not decoded here
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bits_single(0xDEAD, 32);
            b.write_bits_single(0xBEEF, 3);
        });
        w.present(true); // CPhysicalVelocityDataNode, after the unknown node
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_signed(16, 12); // 1 m/s
            b.write_signed(0, 12);
            b.write_signed(0, 12);
        });
        w.present(false);
        w.present(false);

        let k = parse_sync_tree(
            NetObjEntityType::Automobile,
            false,
            &w.finish(),
            false,
            GameBuild::default(),
        );

        assert_eq!(k.sector, Some([300, 300, 5]));
        let v = k.velocity.expect("velocity found after the unknown node");
        assert!((v[0] - 1.0).abs() < 0.1, "vx = {}", v[0]);
    }

    /// Health is the field a cheating client has the most reason to lie about,
    /// so the server reading it out of the entity's own sync tree — rather
    /// than believing a separate client report — is the whole point.
    #[test]
    fn ped_health_and_armour_are_decoded() {
        let mut w = TreeWriter::new();
        w.present(false); // object type
        w.present(true); // p(1,(127,87,0)) — the parent holding health
        w.present(false); // p(2,(127,127,0)) game-state subtree absent
                          // CPedAttachDataNode has Id3 = 1: skipped on objType 0, no bit.
        w.present(true); // CPedHealthDataNode
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bit(false); // not "fine": an explicit health value follows
            b.write_bit(true); // max health changed
            b.write_bits_single(200, 13); // max health
            b.write_bits_single(137, 13); // current health
            b.write_bit(false); // killed with headshot
            b.write_bit(false); // killed with melee
            b.write_bit(false); // has armour
            b.write_bits_single(50, 13); // armour
        });

        let k = parse_sync_tree(
            NetObjEntityType::Ped,
            false,
            &w.finish(),
            false,
            GameBuild(3258),
        );

        assert_eq!(k.health, Some(137.0));
        assert_eq!(k.max_health, Some(200.0));
        assert_eq!(k.armour, Some(50.0));
    }

    /// A ped reported as "fine" carries no explicit health value: it is at its
    /// maximum, whatever that maximum is.
    #[test]
    fn a_fine_ped_reports_full_health() {
        let mut w = TreeWriter::new();
        w.present(false);
        w.present(true);
        w.present(false);
        w.present(true); // CPedHealthDataNode
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bit(true); // fine
            b.write_bit(true); // max health changed
            b.write_bits_single(328, 13);
            b.write_bit(true); // no armour
        });

        let k = parse_sync_tree(
            NetObjEntityType::Ped,
            false,
            &w.finish(),
            false,
            GameBuild(3258),
        );

        assert_eq!(k.health, Some(328.0), "fine means at maximum");
        assert_eq!(k.armour, Some(0.0));
    }

    /// From build 3717 the health fields widen to 14 bits. Decoding a 3717
    /// payload as if it were 3258 would read health from the wrong offset.
    #[test]
    fn winter_update_widens_health_fields() {
        let body = |b: &mut MessageBuffer| {
            b.write_bit(false);
            b.write_bit(true);
            b.write_bits_single(200, 14);
            b.write_bits_single(9000, 14);
            b.write_bit(false);
            b.write_bit(false);
            b.write_bit(true); // no armour
        };
        let mut w = TreeWriter::new();
        w.present(false);
        w.present(true);
        w.present(false);
        w.present(true);
        w.leaf(&body);
        let blob = w.finish();

        let modern = parse_sync_tree(NetObjEntityType::Ped, false, &blob, false, GameBuild(3717));
        assert_eq!(modern.health, Some(9000.0));

        let legacy = parse_sync_tree(NetObjEntityType::Ped, false, &blob, false, GameBuild(3258));
        assert_ne!(
            legacy.health,
            Some(9000.0),
            "reading 14-bit fields as 13-bit must not accidentally agree"
        );
    }

    /// The creation node is where the server learns a ped's model and, when it
    /// spawns straight into a car, which vehicle and seat it occupies.
    #[test]
    fn ped_creation_yields_model_and_vehicle_seat() {
        let mut w = TreeWriter::new();
        // p(1,(1,0,0)) and its CPedCreationDataNode child both have Id2 = 0 on
        // a create, so neither consumes a presence bit.
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bit(false); // is respawn object id
            b.write_bit(false); // respawn flagged for removal
            b.write_bits_single(7, 4); // population type
            b.write_bits_single(0x0D17_1234, 32); // model
            b.write_bits_single(6841, 16); // random seed
            b.write_bit(true); // in vehicle
            b.write_bits_single(0x87BF_F09A, 32); // voice hash
            b.write_bits_single(4321, 13); // vehicle object id
            b.write_bits_single(3, 5); // seat
        });
        w.present(false); // p(1,(127,87,0)) absent

        let k = parse_sync_tree(
            NetObjEntityType::Ped,
            true,
            &w.finish(),
            false,
            GameBuild(3258),
        );

        assert_eq!(k.model, Some(0x0D17_1234));
        assert_eq!(k.vehicle_seat, Some((4321, 3)));
    }

    /// A vehicle create must yield its model — the first 32 bits of the node.
    #[test]
    fn vehicle_creation_yields_model() {
        let mut w = TreeWriter::new();
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bits_single(0xDEAD_BEEF, 32); // model
            b.write_bits_single(6, 4); // population type
        });
        // The automobile tree has a second creation leaf, then the rest.
        w.leaf(&|b: &mut MessageBuffer| {
            b.write_bits_single(0, 8);
        });
        w.present(false); // game-state parent
        w.present(false); // position parent

        let k = parse_sync_tree(
            NetObjEntityType::Automobile,
            true,
            &w.finish(),
            false,
            GameBuild(3258),
        );

        assert_eq!(k.model, Some(0xDEAD_BEEF));
    }

    #[test]
    fn truncated_blob_returns_what_was_decoded() {
        let mut w = TreeWriter::new();
        w.present(false); // object type
        w.present(false); // game-state parent
        w.present(true); // position parent
        w.present(true); // CSectorDataNode
        w.leaf(&sector_body(100, 200, 3));
        let mut bytes = w.finish();
        // Cut the blob short, mid-stream.
        bytes.truncate(bytes.len().saturating_sub(1));

        let k = parse_sync_tree(
            NetObjEntityType::Automobile,
            false,
            &bytes,
            false,
            GameBuild::default(),
        );
        // Whatever survives, the decoder must not panic and must not invent a
        // position from nothing.
        assert!(k.velocity.is_none());
    }

    #[test]
    fn empty_blob_is_inert() {
        for ty in NetObjEntityType::ALL {
            let k = parse_sync_tree(ty, false, &[], false, GameBuild::default());
            assert!(k.is_empty(), "{ty:?} decoded something from nothing");
            let k = parse_sync_tree(ty, true, &[], false, GameBuild::default());
            assert!(k.is_empty(), "{ty:?} decoded something from nothing");
        }
    }

    /// Fuzz-lite: random bytes through every tree, both record kinds, both
    /// buffer modes. The contract is "never panic, never hang".
    #[test]
    fn random_input_never_panics() {
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2_000 {
            let len = (next() % 96) as usize;
            let blob: Vec<u8> = (0..len).map(|_| (next() & 0xFF) as u8).collect();
            let ty = NetObjEntityType::ALL[(next() % 14) as usize];
            let is_create = next() & 1 == 0;
            let length_hack = next() & 2 == 0;
            // Alternate the build so the widened-field branches are fuzzed too.
            let build = if next() & 4 == 0 {
                GameBuild(3258)
            } else {
                GameBuild(3717)
            };
            let _ = parse_sync_tree(ty, is_create, &blob, length_hack, build);
        }
    }

    // --- vehicle and ped state nodes ---

    /// Author a node body the way the engine writes it, then decode it back.
    /// Written against the field order in `SyncTrees_Five.h`, so a drift on
    /// either side fails here rather than in production.
    struct NodeWriter {
        buf: MessageBuffer,
    }

    impl NodeWriter {
        fn new() -> Self {
            Self {
                buf: MessageBuffer::new(256),
            }
        }

        fn bit(&mut self, value: bool) -> &mut Self {
            self.buf.write_bit(value);
            self
        }

        fn bits(&mut self, value: u32, length: usize) -> &mut Self {
            self.buf.write_bits_single(value, length);
            self
        }

        fn signed(&mut self, value: i32, length: usize) -> &mut Self {
            self.buf.write_signed(value, length);
            self
        }

        /// Seal the body at exactly the bits written, so a decoder reading one
        /// bit too many fails instead of drifting into the zero padding.
        fn finish(&mut self) -> MessageBuffer {
            let bits = self.buf.current_bit();
            MessageBuffer::from_bits(self.buf.buffer().to_vec(), bits)
        }
    }

    #[test]
    fn vehicle_game_state_decodes_engine_doors_and_lights() {
        let mut w = NodeWriter::new();
        w.bits(21, 7) // radio station (7 bits from build 2545)
            .bit(false)
            .bit(true) // engine on
            .bit(false) // engine starting
            .bit(false)
            .bit(true) // handbrake
            .bit(false)
            .bit(false)
            .bit(false); // detailed state present
        w.bit(false) // non-default headlights
            .bits(5, 8) // headlight colour
            .bit(true) // siren on
            .bit(false) // unk12
            .bit(false) // unk14
            .bit(true); // lock/door block present
        w.bits(7, 5) // lock status
            .bits(0, 7)
            .bits(0, 7) // unbreakable doors
            .bits(0b000_0101, 7); // doors 0 and 2 open
        w.bits(3, 4) // door 0 position
            .bits(6, 4) // door 2 position
            .bits(0, 8); // no extra door slots
        w.bit(false) // no open windows
            .bit(false)
            .bit(false)
            .bit(true) // stationary
            .bit(false)
            .bit(false)
            .bit(false)
            .bit(false); // no trailing float
        w.bit(false) // unk33
            .bit(false) // no id pair
            .bit(false); // unk38
        w.bit(true) // lights on
            .bit(false) // highbeams
            .bits(2, 3); // light state
        for _ in 0..6 {
            w.bit(false);
        }
        w.bits(0, 32).bits(0, 3).bit(false).bit(true); // owned by a player
        for _ in 0..6 {
            w.bit(false);
        }
        w.bit(false).bit(false); // 2699 gate
        w.bit(true) // has lock
            .bits(0xFFFF_FFFF, 32); // locked for everyone

        let state = decode_vehicle_game_state(&mut w.finish(), GameBuild(3258)).expect("decoded");
        assert_eq!(state.radio_station, 21);
        assert!(state.engine_on && state.handbrake && state.siren_on);
        assert!(!state.default_headlights);
        assert_eq!(state.headlights_colour, 5);
        assert_eq!(state.lock_status, 7);
        assert_eq!(state.doors_open, 0b000_0101);
        assert_eq!(state.door_positions[0], 3);
        assert_eq!(state.door_positions[2], 6);
        assert_eq!(
            state.door_positions[1], 0,
            "a shut door carries no position"
        );
        assert!(state.is_stationary && state.lights_on && !state.highbeams_on);
        assert_eq!(state.light_state, 2);
        assert!(state.has_been_owned_by_player && state.has_lock);
        assert_eq!(state.locked_players, -1, "-1 is the engine's ALL sentinel");
    }

    /// Before 2545 the radio index was 6 bits wide. Decoding an old record
    /// with the newer width would shift every field after it.
    #[test]
    fn vehicle_game_state_radio_width_follows_the_build() {
        let mut w = NodeWriter::new();
        w.bits(37, 6); // fits the pre-2545 width
        for _ in 0..7 {
            w.bit(false);
        }
        w.bit(false); // detailed state present
        w.bit(true) // default headlights
            .bit(false) // siren
            .bit(false) // unk12
            .bit(false) // unk14
            .bit(false) // no lock block
            .bit(false); // no open windows
        for _ in 0..7 {
            w.bit(false);
        }
        w.bit(false).bit(false).bit(false);
        w.bit(false).bit(false).bits(0, 3);
        for _ in 0..6 {
            w.bit(false);
        }
        w.bits(0, 32).bits(0, 3).bit(false).bit(false);
        for _ in 0..6 {
            w.bit(false);
        }
        w.bit(false);

        let state = decode_vehicle_game_state(&mut w.finish(), GameBuild(2372)).expect("decoded");
        assert_eq!(state.radio_station, 37);
        assert!(state.default_headlights);
    }

    #[test]
    fn vehicle_health_decodes_damage_and_tyres() {
        let mut w = NodeWriter::new();
        w.bit(false)
            .bit(false)
            .bit(true) // engine damaged
            .bit(true); // petrol tank damaged
        w.signed(650, 19).signed(400, 19);
        w.bit(false) // tyres not fine
            .bit(true); // no extra wheel data
        w.bits(4, 4); // four wheels
                      // From 2060 each wheel is preceded by an optional heat byte; the flag
                      // is written false here. Then: burst, on-rim, and two unknowns.
        w.bit(false).bit(true).bit(false).bit(false).bit(false);
        w.bit(false).bit(false).bit(true).bit(false).bit(false);
        w.bit(false).bit(false).bit(false).bit(false).bit(false);
        w.bit(false).bit(false).bit(false).bit(false).bit(false);
        w.bit(false).signed(820, 19); // overall health
        w.bit(false).signed(910, 19); // body health
        w.bit(false); // no damage source
        w.bits(0, 4).bits(3, 4); // total repairs

        let health = decode_vehicle_health(&mut w.finish(), GameBuild(3258)).expect("decoded");
        assert_eq!(health.engine_health, 650);
        assert_eq!(health.petrol_tank_health, 400);
        assert_eq!(health.health, 820);
        assert_eq!(health.body_health, 910);
        assert!(!health.tyres_fine);
        assert_eq!(health.tyre_status[0], 1, "burst");
        assert_eq!(health.tyre_status[1], 2, "on the rim");
        assert_eq!(health.tyre_status[2], 0);
        assert_eq!(health.total_repairs, 3);
    }

    /// An undamaged vehicle sends almost nothing; the engine substitutes 1000
    /// everywhere, and so must we, or every intact car reads as a wreck.
    #[test]
    fn vehicle_health_defaults_to_intact() {
        let mut w = NodeWriter::new();
        w.bit(false).bit(false).bit(false).bit(false);
        w.bit(true).bit(true); // tyres and wheels fine
        w.bit(true).bit(true); // health and body health fine
        w.bit(false);
        w.bits(0, 4).bits(0, 4);

        let health = decode_vehicle_health(&mut w.finish(), GameBuild(3258)).expect("decoded");
        assert_eq!(health.engine_health, 1000);
        assert_eq!(health.petrol_tank_health, 1000);
        assert_eq!(health.health, 1000);
        assert_eq!(health.body_health, 1000);
        assert!(health.tyres_fine);
    }

    #[test]
    fn vehicle_appearance_decodes_colours_plate_and_neons() {
        let mut w = NodeWriter::new();
        w.bits(12, 8) // primary
            .bits(34, 8) // secondary
            .bits(1, 8) // pearl
            .bits(2, 8) // wheel
            .bits(3, 8) // interior
            .bits(4, 8); // dashboard
        w.bit(false).bit(false); // no RGB overrides
        w.bits(0, 8); // env effect scale
        w.bit(true) // has extras
            .bits(9, 5) // dirt level
            .bits(0b110, 16) // extras 1 and 2
            .bit(true)
            .bits(5, 5) // livery
            .bit(false); // no roof livery
        w.bits(0, 2); // no mod kit
        w.bit(true).bits(3, 8); // window tint
        w.bit(true).bits(10, 8).bits(20, 8).bits(30, 8); // tyre smoke
        w.bit(true);
        for byte in b"ABC123  " {
            w.bits(u32::from(*byte), 7);
        }
        w.bits(7, 32) // plate text index
            .bits(0xDEAD_BEEF, 32); // horn hash
        w.bit(false); // no emblems
        w.bit(true) // neons
            .bits(255, 8)
            .bits(128, 8)
            .bits(0, 8)
            .bit(true)
            .bit(true)
            .bit(false)
            .bit(true);

        let look = decode_vehicle_appearance(&mut w.finish(), GameBuild(3258)).expect("decoded");
        assert_eq!(look.primary_colour, 12);
        assert_eq!(look.secondary_colour, 34);
        assert_eq!(look.interior_colour, 3);
        assert_eq!(look.dashboard_colour, 4);
        assert_eq!(look.dirt_level, 9);
        assert_eq!(look.extras, 0b110);
        assert_eq!(look.livery_index, 5);
        assert_eq!(look.roof_livery_index, 0);
        assert_eq!(look.wheel_type, 255, "no kit means no wheel type");
        assert_eq!(look.window_tint_index, 3);
        assert_eq!(look.tyre_smoke_colour, [10, 20, 30]);
        assert_eq!(look.plate_text(), "ABC123");
        assert_eq!(look.number_plate_text_index, 7);
        assert_eq!(look.horn_type_hash, 0xDEAD_BEEF);
        assert!(look.has_neon_lights);
        assert_eq!(look.neon_colour, [255, 128, 0]);
        assert_eq!(look.neon_sides, [true, true, false, true]);
    }

    #[test]
    fn vehicle_damage_decodes_bullets_and_windows() {
        let mut w = NodeWriter::new();
        w.bit(false); // no body deformation
        w.bit(true); // damaged by bullets
        for _ in 0..6 {
            w.bits(2, 8);
        }
        w.bit(false); // no broken bumper
        w.bit(false); // no broken light
        w.bit(true); // some window broken
        w.bit(true).bit(false).bit(false).bit(true);
        w.bit(false).bit(false).bit(false).bit(false);

        let damage = decode_vehicle_damage(&mut w.finish()).expect("decoded");
        assert!(damage.damaged_by_bullets);
        assert!(damage.any_window_broken);
        assert_eq!(
            damage.windows_broken,
            [true, false, false, true, false, false, false, false]
        );
    }

    #[test]
    fn ped_game_state_decodes_the_vehicle_a_ped_sits_in() {
        let mut w = NodeWriter::new();
        for _ in 0..6 {
            w.bit(false);
        }
        // Build 3258 opens the 2060, 2189 and 2372 gates (four bits), not 3407.
        for _ in 0..4 {
            w.bit(false);
        }
        w.bits(0, 1).bits(0, 2); // arrest and death state
        w.bit(true).bits(0x1B06_D7B1, 32).bits(0, 3); // weapon (build >= 3258)
        w.bit(false); // 2060 gate
        for _ in 0..5 {
            w.bit(false);
        }
        w.bit(false); // no tint
        w.bits(0, 4); // no weapon components
        w.bits(0, 2); // no gadgets
        w.bit(true) // in a vehicle
            .bits(4243, 13)
            .bit(true) // seated
            .bits(2, 5);
        w.bit(false); // unk6
        w.bit(true) // custodian block
            .bits(0, 13)
            .bit(true) // handcuffed
            .bit(false)
            .bit(false)
            .bit(false)
            .bit(false);
        w.bit(true) // flashlight
            .bit(false) // action mode
            .bit(true); // stealth

        let ped = decode_ped_game_state(&mut w.finish(), GameBuild(3258)).expect("decoded");
        assert_eq!(ped.cur_weapon, 0x1B06_D7B1);
        assert_eq!(ped.cur_vehicle, 4243);
        assert_eq!(ped.cur_vehicle_seat, 2);
        assert!(ped.is_handcuffed && ped.is_flashlight_on && ped.stealth_mode_enabled);
        assert!(!ped.action_mode_enabled);
    }

    /// A ped mid-entry reports a vehicle without a seat. The engine treats
    /// that as "not in a vehicle" and remembers it as the last one instead —
    /// so the decoder must not report occupancy the ped does not have.
    #[test]
    fn ped_entering_a_vehicle_is_not_reported_as_inside_it() {
        let mut w = NodeWriter::new();
        for _ in 0..6 {
            w.bit(false);
        }
        for _ in 0..4 {
            w.bit(false); // 2060, 2189 and 2372 gates
        }
        w.bits(0, 1).bits(0, 2);
        w.bit(false); // no weapon
        w.bit(false); // 2060 gate
        for _ in 0..5 {
            w.bit(false);
        }
        w.bit(false).bits(0, 4).bits(0, 2);
        w.bit(true).bits(4243, 13).bit(false); // vehicle, but no seat
        w.bit(false).bit(false);
        w.bit(false).bit(false).bit(false);

        let ped = decode_ped_game_state(&mut w.finish(), GameBuild(3258)).expect("decoded");
        assert_eq!(ped.cur_vehicle, -1);
        assert_eq!(ped.cur_vehicle_seat, -1);
    }

    /// Truncation must cost the node, not the walk: a body that stops halfway
    /// yields nothing rather than a half-filled struct of plausible garbage.
    #[test]
    fn a_truncated_vehicle_node_decodes_to_nothing() {
        let mut w = NodeWriter::new();
        w.bits(21, 6).bit(false).bit(true);
        assert!(decode_vehicle_game_state(&mut w.finish(), GameBuild(3258)).is_none());
    }
}
