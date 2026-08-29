//! Authoring RAGE sync trees — the inverse of [`super::sync_parse`].
//!
//! A server that only relays what clients send never needs this. A server that
//! *creates* entities does: `CreateVehicle` from a script has to produce a
//! clone payload a stock FiveM client will accept, which means emitting the
//! same presence bits, the same 13-bit leaf framing and the same node layouts
//! the client expects to read.
//!
//! ## Only the nodes we author
//!
//! A create does not have to carry every node in the tree — the client fills
//! the rest with defaults. So authoring is "supply payloads for a few nodes,
//! let the writer emit absent-bits for everything else". A parent's presence
//! bit is set exactly when something in its subtree was supplied, which is
//! what keeps the emitted stream parseable.
//!
//! ## Record types are not interchangeable
//!
//! A create is walked as `sync_type = 1, obj_type = 0` and a sync as
//! `sync_type = 2, obj_type = 1` (the engine's `Unparse` writes that leading
//! object-type bit). The two therefore contain different node sets and
//! different presence bits, and a payload authored for one is meaningless in
//! the other.
//!
//! Node contents follow the engine's own server-side entity factories
//! (`ServerSetters.cpp`), which is the reference for what a client accepts.

use super::clone::NetObjEntityType;
use super::quaternion;
use super::sync_parse::GameBuild;
use super::sync_trees::{tree_for, FlatNode, NodeKind};
use super::MessageBuffer;

/// Bit width of the per-leaf length prefix, mirroring the reader.
const NODE_LENGTH_BITS: usize = 13;
/// Scratch size for one node payload. The largest node BASTON authors is far
/// smaller; the biggest node in any tree is 560 bytes.
const NODE_SCRATCH_BYTES: usize = 1024;
/// Scratch size for a whole authored tree.
const TREE_SCRATCH_BYTES: usize = 4096;

/// Population type for anything a script creates (`POPTYPE_MISSION`).
///
/// The value matters beyond bookkeeping: `CVehicleCreationDataNode` gates a
/// field on `popType - 6 <= 1`, a predicate whose signedness is ambiguous in
/// the engine source. `7` satisfies it under either reading, so the encoding
/// is unambiguous — and it is also the semantically correct type for a
/// script-owned entity.
pub const POPTYPE_MISSION: u32 = 7;

/// `ENTITY_OWNEDBY_SCRIPT`, written by the engine's object factory.
const ENTITY_OWNEDBY_SCRIPT: u32 = 4;

/// Node payloads to emit, keyed by the node's C++ type name.
#[derive(Default)]
pub struct AuthoredNodes {
    /// `(node name, payload bytes, payload bit length)`.
    nodes: Vec<(&'static str, Vec<u8>, usize)>,
}

impl AuthoredNodes {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Author one node.
    ///
    /// A tree may repeat a node type (a ped carries eight task nodes); every
    /// occurrence would receive this same payload. BASTON only authors nodes
    /// that appear once, so that case never arises in practice.
    pub fn set(&mut self, name: &'static str, write: impl FnOnce(&mut MessageBuffer)) {
        let mut scratch = MessageBuffer::new(NODE_SCRATCH_BYTES);
        write(&mut scratch);
        let bits = scratch.current_bit();
        let bytes = scratch.into_inner();
        self.nodes.push((name, bytes, bits));
    }

    fn payload(&self, name: &str) -> Option<(&[u8], usize)> {
        self.nodes
            .iter()
            .find(|(node, _, _)| *node == name)
            .map(|(_, bytes, bits)| (bytes.as_slice(), *bits))
    }

    fn contains(&self, name: &str) -> bool {
        self.nodes.iter().any(|(node, _, _)| *node == name)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Emit a clone payload for `entity_type` containing the authored nodes.
///
/// Returns the bit-packed blob to put in a create or sync record. `None` when
/// the tree could not be written whole, which means the payload would be
/// mis-framed and must not be sent.
#[must_use]
pub fn write_sync_tree(
    entity_type: NetObjEntityType,
    is_create: bool,
    length_hack: bool,
    _build: GameBuild,
    nodes: &AuthoredNodes,
) -> Option<Vec<u8>> {
    let mut buf = MessageBuffer::new(TREE_SCRATCH_BYTES).with_length_hack(length_hack);
    let (sync_type, obj_type) = if is_create {
        (1_u32, 0_u32)
    } else {
        // The engine's `Unparse` writes this bit and then parses as objType 1.
        buf.write_bit(true);
        (2_u32, 1_u32)
    };

    let tree = tree_for(entity_type);
    let mut cursor = 0_usize;
    while cursor < tree.len() {
        if !write_node(tree, &mut cursor, &mut buf, sync_type, obj_type, nodes) {
            return None;
        }
    }

    let used = buf.data_length();
    let mut bytes = buf.into_inner();
    bytes.truncate(used);
    Some(bytes)
}

/// Emit one node and, for parents, its subtree.
fn write_node(
    tree: &[FlatNode],
    cursor: &mut usize,
    buf: &mut MessageBuffer,
    sync_type: u32,
    obj_type: u32,
    nodes: &AuthoredNodes,
) -> bool {
    let Some(node) = tree.get(*cursor).copied() else {
        return true;
    };
    let depth = node.depth;
    let (id1, id2, id3) = (
        u32::from(node.ids.0),
        u32::from(node.ids.1),
        u32::from(node.ids.2),
    );
    *cursor += 1;

    // Gates that consume no bits — the reader skips these without reading, so
    // the writer must emit nothing for the whole subtree.
    if id1 & sync_type == 0 || (id3 != 0 && obj_type & id3 == 0) {
        skip_subtree(tree, cursor, depth);
        return true;
    }

    // The reader consumes a presence bit exactly when `Id2 & sync_type` is set.
    // When it does not, the node is *unconditionally* read — a create's
    // creation nodes are the important case — so the writer must always emit
    // it, even empty, or the reader takes its length prefix from whatever
    // happens to follow and the rest of the tree is garbage.
    if id2 & sync_type != 0 {
        let present = match node.kind {
            NodeKind::Parent => {
                subtree_has_payload(tree, *cursor, depth, sync_type, obj_type, nodes)
            }
            NodeKind::Data { name, .. } => nodes.contains(name),
        };
        if !buf.write_bit(present) {
            return false;
        }
        if !present {
            // An absent node contributes nothing further, and an absent parent
            // takes its whole subtree with it.
            skip_subtree(tree, cursor, depth);
            return true;
        }
    }

    match node.kind {
        NodeKind::Parent => {
            while tree.get(*cursor).is_some_and(|child| child.depth > depth) {
                if !write_node(tree, cursor, buf, sync_type, obj_type, nodes) {
                    return false;
                }
            }
            true
        }
        NodeKind::Data { name, .. } => match nodes.payload(name) {
            Some((bytes, bits)) => {
                buf.write_bits_single(bits as u32, NODE_LENGTH_BITS) && buf.write_bits(bytes, bits)
            }
            // Unconditionally-read node we have nothing to say about: an
            // explicit zero-length body keeps the stream aligned.
            None => buf.write_bits_single(0, NODE_LENGTH_BITS),
        },
    }
}

/// Whether any leaf under a parent — reachable under the current gates — was
/// authored. A parent whose subtree is empty is emitted as absent.
fn subtree_has_payload(
    tree: &[FlatNode],
    start: usize,
    depth: u8,
    sync_type: u32,
    obj_type: u32,
    nodes: &AuthoredNodes,
) -> bool {
    let mut index = start;
    while let Some(node) = tree.get(index) {
        if node.depth <= depth {
            break;
        }
        index += 1;
        let (id1, id3) = (u32::from(node.ids.0), u32::from(node.ids.2));
        if id1 & sync_type == 0 || (id3 != 0 && obj_type & id3 == 0) {
            continue; // unreachable under these gates
        }
        if let NodeKind::Data { name, .. } = node.kind {
            if nodes.contains(name) {
                return true;
            }
        }
    }
    false
}

fn skip_subtree(tree: &[FlatNode], cursor: &mut usize, depth: u8) {
    while tree.get(*cursor).is_some_and(|child| child.depth > depth) {
        *cursor += 1;
    }
}

/// Split a world position into the sector index and in-sector offset the
/// position nodes carry. Inverse of [`super::sync_parse::world_position`], and
/// a port of the engine's `SetupPosition`.
#[must_use]
pub fn sector_of(position: [f32; 3]) -> ([i32; 3], [f32; 3]) {
    let sector = [
        (position[0] / 54.0 + 512.0) as i32,
        (position[1] / 54.0 + 512.0) as i32,
        ((position[2] + 1700.0) / 69.0) as i32,
    ];
    let offset = [
        position[0] - (sector[0] as f32 - 512.0) * 54.0,
        position[1] - (sector[1] as f32 - 512.0) * 54.0,
        position[2] - (sector[2] as f32 * 69.0 - 1700.0),
    ];
    (sector, offset)
}

/// Author the position pair every entity type needs: the sector index and the
/// in-sector offset, using `offset_node` for the entity's offset node type.
pub fn author_position(nodes: &mut AuthoredNodes, position: [f32; 3], offset_node: OffsetNode) {
    let (sector, offset) = sector_of(position);
    nodes.set("CSectorDataNode", move |b| {
        b.write_bits_single(sector[0] as u32, 10);
        b.write_bits_single(sector[1] as u32, 10);
        b.write_bits_single(sector[2] as u32, 6);
    });
    match offset_node {
        OffsetNode::Sector => nodes.set("CSectorPositionDataNode", move |b| {
            write_offset(b, offset, 12);
        }),
        OffsetNode::Object => nodes.set("CObjectSectorPosNode", move |b| {
            // High-resolution encoding, as the engine's object factory uses.
            b.write_bit(true);
            write_offset(b, offset, 20);
        }),
        OffsetNode::PedMap => nodes.set("CPedSectorPosMapNode", move |b| {
            write_offset(b, offset, 12);
            // No extra data: not standing on anything, not ragdolling.
            b.write_bit(false);
        }),
    }
}

/// Which in-sector offset node an entity type uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetNode {
    /// `CSectorPositionDataNode` — vehicles and most entities.
    Sector,
    /// `CObjectSectorPosNode` — objects, with a resolution selector bit.
    Object,
    /// `CPedSectorPosMapNode` — peds on the map.
    PedMap,
}

fn write_offset(buf: &mut MessageBuffer, offset: [f32; 3], bits: usize) {
    buf.write_float(bits, 54.0, offset[0]);
    buf.write_float(bits, 54.0, offset[1]);
    buf.write_float(bits, 69.0, offset[2]);
}

/// Write a compressed quaternion in the layout both orientation nodes share.
fn write_quaternion(buf: &mut MessageBuffer, heading: f32) {
    let compressed =
        quaternion::compress(quaternion::from_heading_degrees(heading), quaternion::BITS);
    let bits = quaternion::BITS as usize;
    buf.write_bits_single(compressed.largest, 2);
    buf.write_bits_single(compressed.a, bits);
    buf.write_bits_single(compressed.b, bits);
    buf.write_bits_single(compressed.c, bits);
}

/// Author a vehicle create, mirroring the engine's `MakeVehicle`.
pub fn author_vehicle(
    model: u32,
    position: [f32; 3],
    heading: f32,
    creation_token: u32,
) -> AuthoredNodes {
    let mut nodes = AuthoredNodes::new();
    nodes.set("CVehicleCreationDataNode", move |b| {
        b.write_bits_single(model, 32);
        b.write_bits_single(POPTYPE_MISSION, 4);
        b.write_bits_single(0, 16); // random seed
        b.write_bit(false); // car budget (read because popType is 7)
        b.write_bits_single(1000, 19); // max health
        b.write_bits_single(2, 3); // vehicle status
        b.write_bits_single(creation_token, 32);
        b.write_bit(false); // needs to be hotwired
        b.write_bit(false); // tyres don't burst
        b.write_bit(false); // uses special flight mode
    });
    // Automobile-family trees carry a second creation node.
    nodes.set("CAutomobileCreationDataNode", |b| {
        b.write_bit(true); // all doors closed
    });
    author_position(&mut nodes, position, OffsetNode::Sector);
    nodes.set("CEntityOrientationDataNode", move |b| {
        write_quaternion(b, heading);
    });
    nodes
}

/// Author a ped create, mirroring the engine's `MakePed`.
pub fn author_ped(model: u32, position: [f32; 3], heading: f32) -> AuthoredNodes {
    let mut nodes = AuthoredNodes::new();
    nodes.set("CPedCreationDataNode", move |b| {
        b.write_bit(false); // is respawn object id
        b.write_bit(false); // respawn flagged for removal
        b.write_bits_single(POPTYPE_MISSION, 4);
        b.write_bits_single(model, 32);
        b.write_bits_single(0, 16); // random seed
        b.write_bit(false); // not in a vehicle
        b.write_bits_single(NO_VOICE, 32);
        b.write_bit(false); // no prop
        b.write_bit(true); // is standing
        b.write_bit(false); // no damage attribution
        b.write_bits_single(200, 13); // max health
        b.write_bit(false);
    });
    author_position(&mut nodes, position, OffsetNode::PedMap);
    // Peds carry their heading as two quantised angles rather than a
    // quaternion: where they face now, and where they are turning to.
    nodes.set("CPedOrientationDataNode", move |b| {
        let radians = heading.to_radians();
        b.write_signed_float(8, quaternion::TAU_RADIANS, radians);
        b.write_signed_float(8, quaternion::TAU_RADIANS, radians);
    });
    nodes
}

/// `HashString("NO_VOICE")`, the value the engine's ped factory writes.
const NO_VOICE: u32 = 0x87BF_F09A;

/// Author an object create, mirroring the engine's `MakeObject`.
pub fn author_object(model: u32, position: [f32; 3], heading: f32, dynamic: bool) -> AuthoredNodes {
    let mut nodes = AuthoredNodes::new();
    nodes.set("CObjectCreationDataNode", move |b| {
        b.write_bits_single(ENTITY_OWNEDBY_SCRIPT, 5);
        b.write_bits_single(model, 32);
        b.write_bit(dynamic); // has init physics
                              // Flags the engine's factory writes as false: script-grabbed-from-world,
                              // no-reassign, and the trailing reserved bits.
        for _ in 0..5 {
            b.write_bit(false);
        }
    });
    author_position(&mut nodes, position, OffsetNode::Object);
    nodes.set("CObjectOrientationDataNode", move |b| {
        // Low-resolution form: a leading bit selects the quaternion layout
        // over the three-angle one, as the engine's object factory does.
        b.write_bit(false);
        write_quaternion(b, heading);
    });
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rage::sync_parse::{parse_sync_tree, world_position};

    /// The whole point: what we author must read back as what we meant. These
    /// round-trips run the real writer against the real parser, so a framing
    /// mistake in either direction fails here rather than on a player's screen.
    fn round_trip(
        entity_type: NetObjEntityType,
        nodes: &AuthoredNodes,
    ) -> super::super::sync_parse::SyncNodeData {
        let blob = write_sync_tree(entity_type, true, false, GameBuild::default(), nodes)
            .expect("tree is writable");
        parse_sync_tree(entity_type, true, &blob, false, GameBuild::default())
    }

    #[test]
    fn sector_split_is_the_inverse_of_world_position() {
        for position in [
            [0.0, 0.0, 0.0],
            [-1234.5, 4567.0, 72.5],
            [3000.0, -3000.0, -50.0],
        ] {
            let (sector, offset) = sector_of(position);
            let restored = world_position(sector, offset);
            for axis in 0..3 {
                assert!(
                    (restored[axis] - position[axis]).abs() < 0.01,
                    "axis {axis}: {restored:?} != {position:?}"
                );
            }
        }
    }

    #[test]
    fn an_authored_vehicle_reads_back_with_its_model_and_position() {
        let position = [1234.5, -567.25, 42.0];
        let nodes = author_vehicle(0xDEAD_BEEF, position, 90.0, 7);

        let decoded = round_trip(NetObjEntityType::Automobile, &nodes);

        assert_eq!(decoded.model, Some(0xDEAD_BEEF));
        let sector = decoded.sector.expect("sector authored");
        let offset = decoded.sector_position.expect("offset authored");
        let world = world_position(sector, offset);
        for axis in 0..3 {
            assert!(
                (world[axis] - position[axis]).abs() < 0.1,
                "axis {axis}: {world:?} != {position:?}"
            );
        }
    }

    #[test]
    fn an_authored_ped_reads_back_with_its_model_and_position() {
        let position = [-800.0, 175.5, 71.0];
        let nodes = author_ped(0x0D17_1234, position, 225.0);

        let decoded = round_trip(NetObjEntityType::Ped, &nodes);

        assert_eq!(decoded.model, Some(0x0D17_1234));
        assert_eq!(
            decoded.vehicle_seat, None,
            "a ped created on foot is not in a vehicle"
        );
        let world = world_position(decoded.sector.unwrap(), decoded.sector_position.unwrap());
        for axis in 0..3 {
            assert!((world[axis] - position[axis]).abs() < 0.1, "axis {axis}");
        }
    }

    #[test]
    fn an_authored_object_reads_back_at_high_resolution() {
        let position = [10.0, 20.0, 30.0];
        let nodes = author_object(0x1234_5678, position, 45.0, true);

        let decoded = round_trip(NetObjEntityType::Object, &nodes);

        let world = world_position(decoded.sector.unwrap(), decoded.sector_position.unwrap());
        for axis in 0..3 {
            // High-resolution offsets are 20-bit, so far tighter than 12-bit.
            assert!((world[axis] - position[axis]).abs() < 0.01, "axis {axis}");
        }
    }

    /// Every tree must be writable, for both record kinds, with no payload at
    /// all — that is the "all nodes absent" case the framing has to survive.
    #[test]
    fn an_empty_tree_is_writable_and_parses_as_empty() {
        let nodes = AuthoredNodes::new();
        for entity_type in NetObjEntityType::ALL {
            for is_create in [true, false] {
                let blob =
                    write_sync_tree(entity_type, is_create, false, GameBuild::default(), &nodes)
                        .unwrap_or_else(|| panic!("{entity_type:?} create={is_create} writable"));
                let decoded =
                    parse_sync_tree(entity_type, is_create, &blob, false, GameBuild::default());
                assert!(
                    decoded.is_empty(),
                    "{entity_type:?} create={is_create} decoded {decoded:?} from nothing"
                );
            }
        }
    }

    /// Each entity family carries its heading in a different node — a
    /// compressed quaternion for vehicles, a selector-prefixed one for
    /// objects, two quantised angles for peds. All three must survive.
    #[test]
    fn headings_survive_for_every_entity_family() {
        let heading_error = |a: f32, b: f32| (((a - b) + 540.0).rem_euclid(360.0) - 180.0).abs();

        for heading in [0.0_f32, 37.5, 90.0, 180.0, 270.0, 359.0] {
            let vehicle = round_trip(
                NetObjEntityType::Automobile,
                &author_vehicle(1, [0.0; 3], heading, 1),
            )
            .heading
            .expect("vehicle heading decoded");
            assert!(
                heading_error(vehicle, heading) < 0.5,
                "vehicle {heading} → {vehicle}"
            );

            let object = round_trip(
                NetObjEntityType::Object,
                &author_object(1, [0.0; 3], heading, false),
            )
            .heading
            .expect("object heading decoded");
            assert!(
                heading_error(object, heading) < 0.5,
                "object {heading} → {object}"
            );

            let ped = round_trip(NetObjEntityType::Ped, &author_ped(1, [0.0; 3], heading))
                .heading
                .expect("ped heading decoded");
            // Peds quantise the angle to 8 bits over a full turn, so the step
            // is coarse — about 1.4° — and the tolerance has to admit it.
            assert!(heading_error(ped, heading) < 3.0, "ped {heading} → {ped}");
        }
    }

    /// A payload authored for a create must not be emitted as a sync: the node
    /// sets differ, so the two blobs are genuinely different bytes.
    #[test]
    fn create_and_sync_payloads_differ() {
        let nodes = author_vehicle(0xAABB_CCDD, [0.0, 0.0, 0.0], 0.0, 1);
        let create = write_sync_tree(
            NetObjEntityType::Automobile,
            true,
            false,
            GameBuild::default(),
            &nodes,
        )
        .unwrap();
        let sync = write_sync_tree(
            NetObjEntityType::Automobile,
            false,
            false,
            GameBuild::default(),
            &nodes,
        )
        .unwrap();
        assert_ne!(create, sync);
    }

    /// The position nodes live under a parent that must be marked present; if
    /// the writer got that wrong the parser would find nothing.
    #[test]
    fn parent_presence_follows_the_authored_subtree() {
        let mut nodes = AuthoredNodes::new();
        author_position(&mut nodes, [500.0, 500.0, 20.0], OffsetNode::Sector);

        let decoded = round_trip(NetObjEntityType::Automobile, &nodes);

        assert!(decoded.sector.is_some(), "sector parent was marked present");
        assert!(decoded.sector_position.is_some());
        assert_eq!(decoded.model, None, "no creation node was authored");
    }
}
