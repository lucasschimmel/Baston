//! GTA5 sync tree structure tables, ported 1:1 from FiveM's
//! `citizen-server-impl/include/state/SyncTrees_Five.h` (STATE_FIVE branch).
//!
//! Each `*_TREE` static is the flattened **preorder** traversal of the
//! corresponding C++ `SyncTree<...>` template. ORDER IS THE WIRE FORMAT —
//! do not reorder, insert, or remove entries. NodeIds triples and max byte
//! sizes are copied verbatim from the source.
//!
//! Entity-type-to-tree mapping follows `MakeSyncTree` in
//! `citizen-server-impl/src/state/ServerGameState_SyncTrees.cpp`; notably
//! `Trailer` uses `CAutomobileSyncTree` (the C++ `CTrailerSyncTree` is
//! defined but unused for GTA5).
//! Enum values come from `shared/net/NetObjEntityType.h` (STATE_FIVE branch).

/// The entity-type enum is canonical in [`super::clone`]; the tree tables key
/// off the same type so the codec and the structure tables never diverge.
pub use super::clone::NetObjEntityType;

/// One item of a flattened (preorder) sync tree.
#[derive(Debug, Clone, Copy)]
pub struct FlatNode {
    /// Nesting depth (root ParentNode = 0).
    pub depth: u8,
    /// NodeIds<Id1, Id2, Id3> from the C++ template.
    pub ids: (u8, u8, u8),
    pub kind: NodeKind,
}

#[derive(Debug, Clone, Copy)]
pub enum NodeKind {
    Parent,
    /// Leaf data node: C++ type name + max data size in bytes.
    Data {
        name: &'static str,
        max_bytes: u16,
    },
}

/// Returns the flattened sync tree for a GTA5 entity type.
///
/// Matches `MakeSyncTree` exactly: `Trailer` shares the automobile tree.
pub fn tree_for(ty: NetObjEntityType) -> &'static [FlatNode] {
    match ty {
        NetObjEntityType::Automobile => AUTOMOBILE_TREE,
        NetObjEntityType::Bike => BIKE_TREE,
        NetObjEntityType::Boat => BOAT_TREE,
        NetObjEntityType::Door => DOOR_TREE,
        NetObjEntityType::Heli => HELI_TREE,
        NetObjEntityType::Object => OBJECT_TREE,
        NetObjEntityType::Ped => PED_TREE,
        NetObjEntityType::Pickup => PICKUP_TREE,
        NetObjEntityType::PickupPlacement => PICKUP_PLACEMENT_TREE,
        NetObjEntityType::Plane => PLANE_TREE,
        NetObjEntityType::Submarine => SUBMARINE_TREE,
        NetObjEntityType::Player => PLAYER_TREE,
        NetObjEntityType::Trailer => AUTOMOBILE_TREE,
        NetObjEntityType::Train => TRAIN_TREE,
    }
}

/// Shorthand constructor for a parent node.
const fn p(depth: u8, ids: (u8, u8, u8)) -> FlatNode {
    FlatNode {
        depth,
        ids,
        kind: NodeKind::Parent,
    }
}

/// Shorthand constructor for a leaf data node.
const fn d(depth: u8, ids: (u8, u8, u8), name: &'static str, max_bytes: u16) -> FlatNode {
    FlatNode {
        depth,
        ids,
        kind: NodeKind::Data { name, max_bytes },
    }
}

/// CAutomobileSyncTree — also used for Trailer (see MakeSyncTree).
pub static AUTOMOBILE_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CVehicleCreationDataNode", 14),
    d(2, (1, 0, 0), "CAutomobileCreationDataNode", 2),
    p(1, (127, 127, 0)),
    p(2, (127, 127, 0)),
    p(3, (127, 127, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CVehicleGameStateDataNode", 57),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CVehicleScriptGameStateDataNode", 50),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 0), "CPhysicalAttachDataNode", 28),
    d(2, (127, 127, 0), "CVehicleAppearanceDataNode", 179),
    d(2, (127, 127, 0), "CVehicleDamageStatusDataNode", 34),
    d(2, (127, 127, 0), "CVehicleComponentReservationDataNode", 65),
    d(2, (127, 127, 0), "CVehicleHealthDataNode", 57),
    d(2, (127, 127, 0), "CVehicleTaskDataNode", 34),
    p(1, (127, 86, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CSectorPositionDataNode", 5),
    d(2, (87, 87, 0), "CEntityOrientationDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CVehicleAngVelocityDataNode", 4),
    p(2, (127, 86, 0)),
    d(3, (86, 86, 0), "CVehicleSteeringDataNode", 2),
    d(3, (87, 87, 0), "CVehicleControlDataNode", 28),
    d(3, (127, 127, 0), "CVehicleGadgetDataNode", 31),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
    d(2, (4, 0, 0), "CVehicleProximityMigrationDataNode", 36),
];

/// CBikeSyncTree
pub static BIKE_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CVehicleCreationDataNode", 14),
    p(1, (127, 127, 0)),
    p(2, (127, 127, 0)),
    p(3, (127, 127, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CVehicleGameStateDataNode", 57),
    d(4, (127, 127, 0), "CBikeGameStateDataNode", 1),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CVehicleScriptGameStateDataNode", 50),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 0), "CPhysicalAttachDataNode", 28),
    d(2, (127, 127, 0), "CVehicleAppearanceDataNode", 179),
    d(2, (127, 127, 0), "CVehicleDamageStatusDataNode", 34),
    d(2, (127, 127, 0), "CVehicleComponentReservationDataNode", 65),
    d(2, (127, 127, 0), "CVehicleHealthDataNode", 57),
    d(2, (127, 127, 0), "CVehicleTaskDataNode", 34),
    p(1, (127, 86, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CSectorPositionDataNode", 5),
    d(2, (87, 87, 0), "CEntityOrientationDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CVehicleAngVelocityDataNode", 4),
    p(2, (127, 86, 0)),
    d(3, (86, 86, 0), "CVehicleSteeringDataNode", 2),
    d(3, (87, 87, 0), "CVehicleControlDataNode", 28),
    d(3, (127, 127, 0), "CVehicleGadgetDataNode", 31),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
    d(2, (4, 0, 0), "CVehicleProximityMigrationDataNode", 36),
];

/// CBoatSyncTree
pub static BOAT_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CVehicleCreationDataNode", 14),
    p(1, (127, 87, 0)),
    p(2, (127, 87, 0)),
    p(3, (127, 87, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CVehicleGameStateDataNode", 57),
    d(4, (87, 87, 0), "CBoatGameStateDataNode", 5),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CVehicleScriptGameStateDataNode", 50),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 0), "CPhysicalAttachDataNode", 28),
    d(2, (127, 127, 0), "CVehicleAppearanceDataNode", 179),
    d(2, (127, 127, 0), "CVehicleDamageStatusDataNode", 34),
    d(2, (127, 127, 0), "CVehicleComponentReservationDataNode", 65),
    d(2, (127, 127, 0), "CVehicleHealthDataNode", 57),
    d(2, (127, 127, 0), "CVehicleTaskDataNode", 34),
    p(1, (127, 86, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CSectorPositionDataNode", 5),
    d(2, (87, 87, 0), "CEntityOrientationDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CVehicleAngVelocityDataNode", 4),
    p(2, (127, 86, 0)),
    d(3, (86, 86, 0), "CVehicleSteeringDataNode", 2),
    d(3, (87, 87, 0), "CVehicleControlDataNode", 28),
    d(3, (127, 127, 0), "CVehicleGadgetDataNode", 31),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
    d(2, (4, 0, 0), "CVehicleProximityMigrationDataNode", 36),
];

/// CDoorSyncTree
pub static DOOR_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CDoorCreationDataNode", 17),
    p(1, (127, 127, 0)),
    d(2, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(2, (127, 127, 1), "CDoorScriptInfoDataNode", 28),
    d(2, (127, 127, 1), "CDoorScriptGameStateDataNode", 8),
    d(1, (86, 86, 0), "CDoorMovementDataNode", 2),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
];

/// CHeliSyncTree
pub static HELI_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CVehicleCreationDataNode", 14),
    d(2, (1, 0, 0), "CAutomobileCreationDataNode", 2),
    p(1, (127, 87, 0)),
    p(2, (127, 127, 0)),
    p(3, (127, 127, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CVehicleGameStateDataNode", 57),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CVehicleScriptGameStateDataNode", 50),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 0), "CPhysicalAttachDataNode", 28),
    d(2, (127, 127, 0), "CVehicleAppearanceDataNode", 179),
    d(2, (127, 127, 0), "CVehicleDamageStatusDataNode", 34),
    d(2, (127, 127, 0), "CVehicleComponentReservationDataNode", 65),
    d(2, (127, 127, 0), "CVehicleHealthDataNode", 57),
    d(2, (127, 127, 0), "CVehicleTaskDataNode", 34),
    d(2, (87, 87, 0), "CHeliHealthDataNode", 16),
    p(1, (127, 86, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CSectorPositionDataNode", 5),
    d(2, (87, 87, 0), "CEntityOrientationDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CVehicleAngVelocityDataNode", 4),
    p(2, (127, 86, 0)),
    d(3, (86, 86, 0), "CVehicleSteeringDataNode", 2),
    d(3, (87, 87, 0), "CVehicleControlDataNode", 28),
    d(3, (127, 127, 0), "CVehicleGadgetDataNode", 31),
    d(3, (86, 86, 0), "CHeliControlDataNode", 8),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
    d(2, (4, 0, 0), "CVehicleProximityMigrationDataNode", 36),
];

/// CObjectSyncTree
pub static OBJECT_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CObjectCreationDataNode", 30),
    p(1, (127, 127, 0)),
    p(2, (127, 127, 0)),
    p(3, (127, 127, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CObjectGameStateDataNode", 44),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CObjectScriptGameStateDataNode", 14),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 0), "CPhysicalAttachDataNode", 28),
    d(2, (127, 127, 0), "CPhysicalHealthDataNode", 19),
    p(1, (87, 87, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CObjectSectorPosNode", 8),
    d(2, (87, 87, 0), "CObjectOrientationDataNode", 8),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalAngVelocityDataNode", 4),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
];

/// CPedSyncTree
pub static PED_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CPedCreationDataNode", 20),
    d(2, (1, 0, 1), "CPedScriptCreationDataNode", 1),
    p(1, (127, 87, 0)),
    p(2, (127, 127, 0)),
    p(3, (127, 127, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CPedGameStateDataNode", 104),
    d(4, (127, 127, 0), "CPedComponentReservationDataNode", 65),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CPedScriptGameStateDataNode", 115),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 1), "CPedAttachDataNode", 22),
    d(2, (127, 127, 0), "CPedHealthDataNode", 17),
    d(2, (87, 87, 0), "CPedMovementGroupDataNode", 26),
    d(2, (127, 127, 1), "CPedAIDataNode", 9),
    d(2, (87, 87, 0), "CPedAppearanceDataNode", 141),
    p(1, (127, 87, 0)),
    d(2, (87, 87, 0), "CPedOrientationDataNode", 3),
    d(2, (87, 87, 0), "CPedMovementDataNode", 5),
    p(2, (127, 87, 0)),
    d(3, (127, 127, 0), "CPedTaskTreeDataNode", 28),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CPedSectorPosMapNode", 12),
    d(2, (87, 87, 0), "CPedSectorPosNavMeshNode", 4),
    p(1, (87, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
    // NodeIds changed from <5,0,0> to <87,0,0> upstream (see comment in SyncTrees_Five.h)
    d(2, (87, 0, 0), "CPedInventoryDataNode", 321),
    d(2, (4, 4, 1), "CPedTaskSequenceDataNode", 1),
];

/// CPickupSyncTree
pub static PICKUP_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CPickupCreationDataNode", 66),
    p(1, (127, 127, 0)),
    p(2, (127, 127, 0)),
    d(3, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(3, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    p(2, (127, 127, 1)),
    d(3, (127, 127, 1), "CPickupScriptGameStateNode", 14),
    d(3, (127, 127, 1), "CPhysicalGameStateDataNode", 4),
    d(3, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(3, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(3, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(3, (127, 127, 1), "CPhysicalHealthDataNode", 19),
    d(2, (127, 127, 1), "CPhysicalAttachDataNode", 28),
    p(1, (87, 87, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CPickupSectorPosNode", 8),
    d(2, (87, 87, 0), "CEntityOrientationDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalAngVelocityDataNode", 4),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 1), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
];

/// CPickupPlacementSyncTree
pub static PICKUP_PLACEMENT_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    d(1, (1, 0, 0), "CPickupPlacementCreationDataNode", 54),
    d(1, (4, 0, 0), "CMigrationDataNode", 13),
    d(1, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(1, (127, 127, 0), "CPickupPlacementStateDataNode", 7),
];

/// CPlaneSyncTree
pub static PLANE_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CVehicleCreationDataNode", 14),
    p(1, (127, 127, 0)),
    p(2, (127, 127, 0)),
    p(3, (127, 127, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CVehicleGameStateDataNode", 57),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CVehicleScriptGameStateDataNode", 50),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 0), "CPhysicalAttachDataNode", 28),
    d(2, (127, 127, 0), "CVehicleAppearanceDataNode", 179),
    d(2, (127, 127, 0), "CVehicleDamageStatusDataNode", 34),
    d(2, (127, 127, 0), "CVehicleComponentReservationDataNode", 65),
    d(2, (127, 127, 0), "CVehicleHealthDataNode", 57),
    d(2, (127, 127, 0), "CVehicleTaskDataNode", 34),
    d(2, (127, 127, 0), "CPlaneGameStateDataNode", 52),
    p(1, (127, 86, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CSectorPositionDataNode", 5),
    d(2, (87, 87, 0), "CEntityOrientationDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CVehicleAngVelocityDataNode", 4),
    p(2, (127, 86, 0)),
    d(3, (86, 86, 0), "CVehicleSteeringDataNode", 2),
    d(3, (87, 87, 0), "CVehicleControlDataNode", 28),
    d(3, (127, 127, 0), "CVehicleGadgetDataNode", 31),
    d(3, (86, 86, 0), "CPlaneControlDataNode", 7),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
    d(2, (4, 0, 0), "CVehicleProximityMigrationDataNode", 36),
];

/// CSubmarineSyncTree
pub static SUBMARINE_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CVehicleCreationDataNode", 14),
    p(1, (127, 87, 0)),
    p(2, (127, 87, 0)),
    p(3, (127, 87, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CVehicleGameStateDataNode", 57),
    d(4, (87, 87, 0), "CSubmarineGameStateDataNode", 1),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CVehicleScriptGameStateDataNode", 50),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 0), "CPhysicalAttachDataNode", 28),
    d(2, (127, 127, 0), "CVehicleAppearanceDataNode", 179),
    d(2, (127, 127, 0), "CVehicleDamageStatusDataNode", 34),
    d(2, (127, 127, 0), "CVehicleComponentReservationDataNode", 65),
    d(2, (127, 127, 0), "CVehicleHealthDataNode", 57),
    d(2, (127, 127, 0), "CVehicleTaskDataNode", 34),
    p(1, (127, 86, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CSectorPositionDataNode", 5),
    d(2, (87, 87, 0), "CEntityOrientationDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CVehicleAngVelocityDataNode", 4),
    p(2, (127, 86, 0)),
    d(3, (86, 86, 0), "CVehicleSteeringDataNode", 2),
    d(3, (87, 87, 0), "CVehicleControlDataNode", 28),
    d(3, (127, 127, 0), "CVehicleGadgetDataNode", 31),
    d(3, (86, 86, 0), "CSubmarineControlDataNode", 4),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
    d(2, (4, 0, 0), "CVehicleProximityMigrationDataNode", 36),
];

/// CPlayerSyncTree
pub static PLAYER_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CPlayerCreationDataNode", 128),
    p(1, (127, 86, 0)),
    p(2, (127, 87, 0)),
    p(3, (127, 127, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CPedGameStateDataNode", 104),
    d(4, (127, 127, 0), "CPedComponentReservationDataNode", 65),
    p(3, (127, 87, 0)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (87, 87, 0), "CPlayerGameStateDataNode", 104),
    d(2, (127, 127, 1), "CPedAttachDataNode", 22),
    d(2, (127, 127, 0), "CPedHealthDataNode", 17),
    d(2, (87, 87, 0), "CPedMovementGroupDataNode", 26),
    d(2, (127, 127, 1), "CPedAIDataNode", 9),
    d(2, (87, 87, 0), "CPlayerAppearanceDataNode", 560),
    d(2, (86, 86, 0), "CPlayerPedGroupDataNode", 19),
    d(2, (86, 86, 0), "CPlayerAmbientModelStreamingNode", 5),
    d(2, (86, 86, 0), "CPlayerGamerDataNode", 370),
    d(2, (86, 86, 0), "CPlayerExtendedGameStateNode", 20),
    p(1, (127, 86, 0)),
    d(2, (87, 87, 0), "CPedOrientationDataNode", 3),
    d(2, (87, 87, 0), "CPedMovementDataNode", 5),
    p(2, (127, 87, 0)),
    d(3, (127, 127, 0), "CPedTaskTreeDataNode", 28),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(3, (87, 87, 0), "CPedTaskSpecificDataNode", 77),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CPlayerSectorPosNode", 13),
    d(2, (86, 86, 0), "CPlayerCameraDataNode", 24),
    d(2, (86, 86, 0), "CPlayerWantedAndLOSDataNode", 30),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
];

/// CTrainSyncTree
pub static TRAIN_TREE: &[FlatNode] = &[
    p(0, (127, 0, 0)),
    p(1, (1, 0, 0)),
    d(2, (1, 0, 0), "CVehicleCreationDataNode", 14),
    p(1, (127, 127, 0)),
    p(2, (127, 127, 0)),
    p(3, (127, 127, 0)),
    d(4, (127, 127, 0), "CGlobalFlagsDataNode", 2),
    d(4, (127, 127, 0), "CDynamicEntityGameStateDataNode", 102),
    d(4, (127, 127, 0), "CPhysicalGameStateDataNode", 4),
    d(4, (127, 127, 0), "CVehicleGameStateDataNode", 57),
    d(4, (127, 127, 0), "CTrainGameStateDataNode", 17),
    p(3, (127, 127, 1)),
    d(4, (127, 127, 1), "CEntityScriptGameStateDataNode", 1),
    d(4, (127, 127, 1), "CPhysicalScriptGameStateDataNode", 13),
    d(4, (127, 127, 1), "CVehicleScriptGameStateDataNode", 50),
    d(4, (127, 127, 1), "CEntityScriptInfoDataNode", 24),
    d(2, (127, 127, 0), "CPhysicalAttachDataNode", 28),
    d(2, (127, 127, 0), "CVehicleAppearanceDataNode", 179),
    d(2, (127, 127, 0), "CVehicleDamageStatusDataNode", 34),
    d(2, (127, 127, 0), "CVehicleComponentReservationDataNode", 65),
    d(2, (127, 127, 0), "CVehicleHealthDataNode", 57),
    d(2, (127, 127, 0), "CVehicleTaskDataNode", 34),
    p(1, (127, 86, 0)),
    d(2, (87, 87, 0), "CSectorDataNode", 4),
    d(2, (87, 87, 0), "CSectorPositionDataNode", 5),
    d(2, (87, 87, 0), "CEntityOrientationDataNode", 5),
    d(2, (87, 87, 0), "CPhysicalVelocityDataNode", 5),
    d(2, (87, 87, 0), "CVehicleAngVelocityDataNode", 4),
    p(2, (127, 86, 0)),
    d(3, (86, 86, 0), "CVehicleSteeringDataNode", 2),
    d(3, (87, 87, 0), "CVehicleControlDataNode", 28),
    d(3, (127, 127, 0), "CVehicleGadgetDataNode", 31),
    p(1, (4, 0, 0)),
    d(2, (4, 0, 0), "CMigrationDataNode", 13),
    d(2, (4, 0, 0), "CPhysicalMigrationDataNode", 1),
    d(2, (4, 0, 1), "CPhysicalScriptMigrationDataNode", 1),
    d(2, (4, 0, 0), "CVehicleProximityMigrationDataNode", 36),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn first_name(tree: &'static [FlatNode]) -> &'static str {
        tree.iter()
            .find_map(|n| match n.kind {
                NodeKind::Data { name, .. } => Some(name),
                NodeKind::Parent => None,
            })
            .expect("tree has at least one data node")
    }

    fn last_name(tree: &'static [FlatNode]) -> &'static str {
        tree.iter()
            .rev()
            .find_map(|n| match n.kind {
                NodeKind::Data { name, .. } => Some(name),
                NodeKind::Parent => None,
            })
            .expect("tree has at least one data node")
    }

    #[test]
    fn node_counts() {
        assert_eq!(AUTOMOBILE_TREE.len(), 37);
        assert_eq!(BIKE_TREE.len(), 37);
        assert_eq!(BOAT_TREE.len(), 37);
        assert_eq!(DOOR_TREE.len(), 11);
        assert_eq!(HELI_TREE.len(), 39);
        assert_eq!(OBJECT_TREE.len(), 27);
        assert_eq!(PED_TREE.len(), 44);
        assert_eq!(PICKUP_TREE.len(), 25);
        assert_eq!(PICKUP_PLACEMENT_TREE.len(), 5);
        assert_eq!(PLANE_TREE.len(), 38);
        assert_eq!(SUBMARINE_TREE.len(), 38);
        assert_eq!(PLAYER_TREE.len(), 44);
        assert_eq!(TRAIN_TREE.len(), 37);
    }

    #[test]
    fn first_and_last_data_nodes() {
        assert_eq!(first_name(AUTOMOBILE_TREE), "CVehicleCreationDataNode");
        assert_eq!(
            last_name(AUTOMOBILE_TREE),
            "CVehicleProximityMigrationDataNode"
        );
        assert_eq!(first_name(BIKE_TREE), "CVehicleCreationDataNode");
        assert_eq!(last_name(BIKE_TREE), "CVehicleProximityMigrationDataNode");
        assert_eq!(first_name(BOAT_TREE), "CVehicleCreationDataNode");
        assert_eq!(last_name(BOAT_TREE), "CVehicleProximityMigrationDataNode");
        assert_eq!(first_name(DOOR_TREE), "CDoorCreationDataNode");
        assert_eq!(last_name(DOOR_TREE), "CPhysicalScriptMigrationDataNode");
        assert_eq!(first_name(HELI_TREE), "CVehicleCreationDataNode");
        assert_eq!(last_name(HELI_TREE), "CVehicleProximityMigrationDataNode");
        assert_eq!(first_name(OBJECT_TREE), "CObjectCreationDataNode");
        assert_eq!(last_name(OBJECT_TREE), "CPhysicalScriptMigrationDataNode");
        assert_eq!(first_name(PED_TREE), "CPedCreationDataNode");
        assert_eq!(last_name(PED_TREE), "CPedTaskSequenceDataNode");
        assert_eq!(first_name(PICKUP_TREE), "CPickupCreationDataNode");
        assert_eq!(last_name(PICKUP_TREE), "CPhysicalScriptMigrationDataNode");
        assert_eq!(
            first_name(PICKUP_PLACEMENT_TREE),
            "CPickupPlacementCreationDataNode"
        );
        assert_eq!(
            last_name(PICKUP_PLACEMENT_TREE),
            "CPickupPlacementStateDataNode"
        );
        assert_eq!(first_name(PLANE_TREE), "CVehicleCreationDataNode");
        assert_eq!(last_name(PLANE_TREE), "CVehicleProximityMigrationDataNode");
        assert_eq!(first_name(SUBMARINE_TREE), "CVehicleCreationDataNode");
        assert_eq!(
            last_name(SUBMARINE_TREE),
            "CVehicleProximityMigrationDataNode"
        );
        assert_eq!(first_name(PLAYER_TREE), "CPlayerCreationDataNode");
        assert_eq!(last_name(PLAYER_TREE), "CPhysicalScriptMigrationDataNode");
        assert_eq!(first_name(TRAIN_TREE), "CVehicleCreationDataNode");
        assert_eq!(last_name(TRAIN_TREE), "CVehicleProximityMigrationDataNode");
    }

    #[test]
    fn tree_for_covers_all_variants() {
        for ty in NetObjEntityType::ALL {
            let tree = tree_for(ty);
            assert!(!tree.is_empty(), "{ty:?} has an empty tree");
            // Root is always ParentNode<NodeIds<127, 0, 0>, ...> at depth 0.
            assert_eq!(tree[0].depth, 0);
            assert_eq!(tree[0].ids, (127, 0, 0));
            assert!(matches!(tree[0].kind, NodeKind::Parent));
        }
    }

    #[test]
    fn trailer_aliases_automobile_tree() {
        assert!(std::ptr::eq(
            tree_for(NetObjEntityType::Trailer).as_ptr(),
            AUTOMOBILE_TREE.as_ptr()
        ));
    }

    #[test]
    fn from_u8_round_trips() {
        for ty in NetObjEntityType::ALL {
            assert_eq!(NetObjEntityType::from_u8(ty as u8), Some(ty));
        }
        assert_eq!(NetObjEntityType::from_u8(14), None);
        assert_eq!(NetObjEntityType::from_u8(255), None);
    }

    #[test]
    fn depths_are_consistent() {
        // A node's depth may increase by at most 1 relative to its predecessor,
        // and only right after a Parent node.
        for ty in NetObjEntityType::ALL {
            let tree = tree_for(ty);
            for w in tree.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                assert!(
                    b.depth <= a.depth + 1,
                    "{ty:?}: depth jump {} -> {}",
                    a.depth,
                    b.depth
                );
                if b.depth == a.depth + 1 {
                    assert!(
                        matches!(a.kind, NodeKind::Parent),
                        "{ty:?}: child follows a non-parent node"
                    );
                }
            }
        }
    }
}
