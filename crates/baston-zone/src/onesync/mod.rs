//! Server-authoritative OneSync game state.
//!
//! The Rust counterpart of the parts of `ServerGameState` that make the server
//! *parse* entity state instead of relaying it: an object-id-keyed entity
//! registry fed by inbound `netClones` streams, object-id leasing, and ack
//! generation. Interest management and the outbound per-client tick live in
//! the NG scheduler (task 6); this module owns ingestion + arbitration.
//!
//! Everything here is transport-agnostic: the gateway hands raw `msgRoute`
//! payloads in and gets ack packets (and, later, per-client clone packets) out.

mod ingest;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use baston_protocol::debug_info::ObjectIdUsage;
use baston_protocol::rage::clone::NetObjEntityType;
use baston_protocol::rage::object_ids;
use baston_protocol::rage::reliability::{GameStateAck, GameStateNAck};
use baston_protocol::rage::sync_parse::{world_position, EntityNodeState, GameBuild, SyncNodeData};
use baston_protocol::rage::sync_write;

use rayon::prelude::*;

use crate::{
    interest_ng::{ClientView, EntitySnapshot, InterestConfig, SpatialIndex, TickPlan},
    routing_bucket::{LockdownMode, RoutingBucketRegistry},
};

/// Squared 3D distance — comparison only, so the square root is wasted work.
fn squared_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    dx * dx + dy * dy + dz * dz
}

/// The largest usable object id without the length hack (13-bit native space).
/// Server-created entities scan downward from here; client ids are leased
/// upward from 1.
pub const MAX_OBJECT_ID_NATIVE: u16 = 8191;
/// With the length hack (OneSync Beyond) ids widen to the full 16-bit space.
pub const MAX_OBJECT_ID_BEYOND: u16 = u16::MAX - 1;

/// A server-tracked networked entity. The sync-tree blob is stored opaquely
/// (the server only semantically parses a subset of nodes elsewhere); the
/// registry's job is identity, ownership, and freshness.
#[derive(Debug, Clone)]
pub struct ServerEntity {
    pub object_id: u16,
    pub uniqifier: u16,
    /// Net id of the current owner (the source that last created/synced it).
    pub owner: u32,
    pub entity_type: NetObjEntityType,
    pub creation_token: u32,
    /// Latest raw *sync* blob (bit-packed, as received).
    pub data: Vec<u8>,
    /// The entity's *create* blob, retained separately for the lifetime of the
    /// entity.
    ///
    /// A create and a sync are not interchangeable payloads: they are walked
    /// with different `sync_type`/`obj_type` parameters, so their presence bits
    /// and node sets differ. Replaying the latest sync blob inside a create
    /// record — which is what a single buffer forces — makes every client that
    /// enters scope after the entity's first sync mis-parse it.
    pub create_data: Vec<u8>,
    /// Server frame index at last update — the delta baseline.
    pub frame_index: u64,
    /// World position driving interest management, decoded from the entity's
    /// own sync tree (see [`baston_protocol::rage::sync_parse`]). Only
    /// meaningful when [`Self::position_known`] is set.
    pub position: [f32; 3],
    /// Whether a position node has ever been decoded for this entity. An
    /// entity without one is not placeable, so interest management must skip
    /// it rather than treat it as sitting at the world origin.
    pub position_known: bool,
    /// Last decoded sector indices. Sync records carry the sector and the
    /// in-sector offset independently, so both halves are retained and
    /// recombined on every update.
    pub sector: [i32; 3],
    /// Last decoded quantised offset within [`Self::sector`].
    pub sector_position: [f32; 3],
    /// Linear velocity used by relative-closing interest priority.
    pub velocity: [f32; 3],
    /// Routing bucket captured at creation and preserved across syncs.
    pub routing_bucket: u32,
    /// Health decoded from the entity's own sync tree. This is the server's
    /// own reading of the value, not something a client asserted separately —
    /// which is what makes it usable for authority decisions.
    pub health: Option<f32>,
    pub max_health: Option<f32>,
    pub armour: Option<f32>,
    /// Model hash, decoded from the creation node.
    pub model: Option<u32>,
    /// Vehicle this ped was created inside and its seat, from the creation
    /// node. The ped game state supersedes it once the ped syncs.
    pub vehicle_seat: Option<(u16, u8)>,
    /// The client that created the entity. Kept across ownership migration,
    /// because that is exactly what `NetworkGetFirstEntityOwner` asks about.
    pub first_owner: u32,
    /// Everything else the sync tree carries: vehicle state and appearance,
    /// ped occupancy and tasks, attachment, visibility.
    pub nodes: EntityNodeState,
    /// Heading in degrees, decoded from the entity's orientation node.
    pub heading: Option<f32>,
    /// Heading a ped is turning towards, when it reported one.
    pub desired_heading: Option<f32>,
    /// Created by a server script rather than by a client.
    ///
    /// The server authors such an entity and hands simulation to a client, but
    /// it stays the server's: when its owner leaves it is reassigned, not
    /// destroyed — the engine's `ShouldServerKeepEntity`.
    pub server_owned: bool,
}

impl ServerEntity {
    /// Fold a decoded clone record into the entity's state.
    ///
    /// A sync record only carries the nodes that changed, so every field is
    /// merged rather than replaced. In particular either half of the position
    /// may be absent, and the world position is recomputed from the retained
    /// halves whenever either one changes — the port of `CalculatePosition`.
    pub(crate) fn apply_sync_data(&mut self, decoded: &SyncNodeData) {
        let mut position_changed = false;
        if let Some(sector) = decoded.sector {
            self.sector = sector;
            position_changed = true;
        }
        if let Some(offset) = decoded.sector_position {
            self.sector_position = offset;
            position_changed = true;
        }
        if position_changed {
            self.position = world_position(self.sector, self.sector_position);
            self.position_known = true;
        }
        if let Some(velocity) = decoded.velocity {
            self.velocity = velocity;
        }
        if let Some(health) = decoded.health {
            self.health = Some(health);
        }
        if let Some(max_health) = decoded.max_health {
            self.max_health = Some(max_health);
        }
        if let Some(armour) = decoded.armour {
            self.armour = Some(armour);
        }
        if let Some(model) = decoded.model {
            self.model = Some(model);
        }
        if let Some(seat) = decoded.vehicle_seat {
            self.vehicle_seat = Some(seat);
        }
        if let Some(heading) = decoded.heading {
            self.heading = Some(heading);
        }
        if let Some(heading) = decoded.desired_heading {
            self.desired_heading = Some(heading);
        }
        self.nodes.merge(decoded);
    }
}

/// Result of ingesting one inbound clone packet: the ack packets to send back
/// (already framed as `msgPackedAcks`) plus bookkeeping counters.
#[derive(Debug, Default)]
pub struct IngestOutcome {
    pub ack_packets: Vec<Vec<u8>>,
    pub creates: u32,
    pub syncs: u32,
    pub removes: u32,
    /// Takeover requests observed (objectId → requested target net id);
    /// arbitration is applied by [`ServerGameState`] before returning.
    pub takeovers: Vec<(u16, u16)>,
    /// Client-originated mutations rejected by routing policy or arbitration.
    pub rejected_mutations: u32,
}

#[derive(Debug)]
pub struct ClientTick {
    pub source: u32,
    pub packets: Vec<Vec<u8>>,
}

/// Per-client object-id lease + sync bookkeeping.
struct ClientState {
    /// Object ids leased to this client (awaiting or holding a live entity).
    leased: Vec<u16>,
    /// The client's current frame index (from type-6 records).
    frame_index: u64,
    /// The client's ack timestamp (from type-5 records).
    ack_ts: u32,
    /// Outbound sparse view (interest management + delta baselines).
    view: ClientView,
}

impl ClientState {
    fn new(net_id: u16) -> Self {
        Self {
            leased: Vec::new(),
            frame_index: 0,
            ack_ts: 0,
            view: ClientView::new(net_id),
        }
    }
}

/// Allocation state of one object id. A single authoritative state machine
/// replaces the old `id_used`/`id_leased` twin bitmaps (audit ROB-4): an id is
/// either free, leased to a client via `GetFreeObjectIds`, or consumed by a
/// live entity. A create on a leased id promotes `Leased → Used` (the lease is
/// consumed), so the two flags can never diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum IdState {
    #[default]
    Free,
    Leased,
    Used,
}

/// Server-authoritative game state for one routing bucket / zone.
pub struct ServerGameState {
    entities: HashMap<u16, ServerEntity>,
    clients: HashMap<u32, ClientState>,
    /// Allocation state of every object id.
    ids: Vec<IdState>,
    max_object_id: u16,
    big_mode: bool,
    length_hack: bool,
    /// Enforced game build, which selects the sync-node layouts to decode
    /// against. Must match `server.enforce_game_build`, since that is the
    /// build every connected client runs.
    build: GameBuild,
    frame_index: u64,
    /// Monotonic creation token for server-authored entities. Clients supply
    /// their own; the server needs one that cannot collide with a re-create.
    creation_token: u32,
    routing_buckets: RoutingBucketRegistry,
}

impl ServerGameState {
    pub fn new(big_mode: bool, length_hack: bool) -> Self {
        Self::with_build(big_mode, length_hack, GameBuild::default())
    }

    /// Build a game state that decodes sync trees for a specific game build.
    pub fn with_build(big_mode: bool, length_hack: bool, build: GameBuild) -> Self {
        let max_object_id = if length_hack {
            MAX_OBJECT_ID_BEYOND
        } else {
            MAX_OBJECT_ID_NATIVE
        };
        Self {
            entities: HashMap::new(),
            clients: HashMap::new(),
            ids: vec![IdState::Free; max_object_id as usize + 1],
            max_object_id,
            big_mode,
            length_hack,
            build,
            frame_index: 1,
            creation_token: 0,
            routing_buckets: RoutingBucketRegistry::default(),
        }
    }

    pub fn entity(&self, object_id: u16) -> Option<&ServerEntity> {
        self.entities.get(&object_id)
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Entities the server itself authored, rather than adopted from a client.
    pub fn server_owned_count(&self) -> usize {
        self.entities.values().filter(|e| e.server_owned).count()
    }

    /// Entities currently simulated by `source`.
    pub fn owned_by(&self, source: u32) -> usize {
        self.entities.values().filter(|e| e.owner == source).count()
    }

    /// How many entities are cloned to `source` right now. `None` for a
    /// source that is not a registered client.
    pub fn client_scope_len(&self, source: u32) -> Option<usize> {
        self.clients.get(&source).map(|c| c.view.scope_len())
    }

    /// The frame index `source` last acknowledged. The distance to
    /// [`Self::frame_index`] is how far behind that client's view is.
    pub fn client_frame_index(&self, source: u32) -> Option<u64> {
        self.clients.get(&source).map(|c| c.frame_index)
    }

    /// Occupancy of the object-id space.
    ///
    /// Exhaustion is silent from the outside — clients simply stop being able
    /// to create entities — so the split between *used* (a live entity) and
    /// *leased* (handed out, not created on yet) is what distinguishes a full
    /// world from clients hoarding ids they never spend.
    pub fn object_id_usage(&self) -> ObjectIdUsage {
        let mut used = 0;
        let mut leased = 0;
        // Id 0 is "no object" and `lease_object_ids` starts at 1; counting the
        // whole vector would report one permanently unavailable free id.
        for state in &self.ids[1..=self.max_object_id as usize] {
            match state {
                IdState::Used => used += 1,
                IdState::Leased => leased += 1,
                IdState::Free => {}
            }
        }
        ObjectIdUsage {
            used,
            leased,
            free: u32::from(self.max_object_id) - used - leased,
            max: u32::from(self.max_object_id),
        }
    }

    /// Iterate every tracked entity.
    ///
    /// Consumers that need fields interest management does not care about —
    /// health, model, occupancy — read them here instead of paying for a
    /// second [`Self::world_snapshot`] copy.
    pub fn entities(&self) -> impl Iterator<Item = &ServerEntity> + '_ {
        self.entities.values()
    }

    pub fn routing_buckets(&self) -> RoutingBucketRegistry {
        self.routing_buckets.clone()
    }

    pub fn set_player_routing_bucket(&self, source: u32, bucket: u32) {
        self.routing_buckets.set_player_bucket(source, bucket);
    }

    pub fn set_entity_routing_bucket(&mut self, object_id: u16, bucket: u32) {
        self.routing_buckets.set_entity_bucket(object_id, bucket);
        if let Some(entity) = self.entities.get_mut(&object_id) {
            entity.routing_bucket = bucket;
        }
    }

    pub fn set_routing_bucket_lockdown(&self, bucket: u32, mode: LockdownMode) {
        self.routing_buckets.set_lockdown(bucket, mode);
    }

    pub fn set_routing_bucket_population_enabled(&self, bucket: u32, enabled: bool) {
        self.routing_buckets.set_population_enabled(bucket, enabled);
    }

    /// Advance the server frame index (called once per sync tick).
    pub fn tick(&mut self) {
        self.frame_index = self.frame_index.wrapping_add(1);
    }

    pub fn frame_index(&self) -> u64 {
        self.frame_index
    }

    /// Register a connected client (idempotent).
    pub fn add_client(&mut self, source: u32) {
        self.clients
            .entry(source)
            .or_insert_with(|| ClientState::new(source as u16));
    }

    /// Create a networked entity owned by the server.
    ///
    /// The server authors the entity's create payload itself — model, position
    /// — but does not simulate it: like the engine, it hands simulation to a
    /// client. The entity starts ownerless and
    /// [`Self::reassign_ownerless_server_entities`] gives it to the nearest
    /// player; until then it is tracked but not cloned to anyone.
    ///
    /// Returns the network id, or `None` when the object-id space is exhausted
    /// or the payload could not be authored.
    pub fn spawn_server_entity(
        &mut self,
        entity_type: NetObjEntityType,
        model: u32,
        position: [f32; 3],
        heading: f32,
        dynamic: bool,
    ) -> Option<u16> {
        let object_id = self.allocate_server_object_id()?;
        self.spawn_server_entity_with_id(object_id, entity_type, model, position, heading, dynamic)
            .then_some(object_id)
    }

    /// Create a server entity on an id reserved elsewhere.
    ///
    /// A script needs its handle back the instant it calls `CreateVehicle`, so
    /// the id is reserved on the scripting side and the entity is authored
    /// here on the next tick. Returns `false` when the id is already taken,
    /// which is the only way the two allocators can disagree.
    pub fn spawn_server_entity_with_id(
        &mut self,
        object_id: u16,
        entity_type: NetObjEntityType,
        model: u32,
        position: [f32; 3],
        heading: f32,
        dynamic: bool,
    ) -> bool {
        if object_id == 0
            || object_id > self.max_object_id
            || self.ids[object_id as usize] != IdState::Free
        {
            return false;
        }
        self.creation_token = self.creation_token.wrapping_add(1);
        let creation_token = self.creation_token;

        let nodes = match entity_type {
            NetObjEntityType::Ped | NetObjEntityType::Player => {
                sync_write::author_ped(model, position, heading)
            }
            ty if ty.is_vehicle() => {
                sync_write::author_vehicle(model, position, heading, creation_token)
            }
            _ => sync_write::author_object(model, position, heading, dynamic),
        };
        let Some(create_data) =
            sync_write::write_sync_tree(entity_type, true, self.length_hack, self.build, &nodes)
        else {
            tracing::error!(
                target: "onesync",
                ?entity_type,
                "could not author a create payload — the entity is not created"
            );
            return false;
        };

        let (sector, sector_position) = sync_write::sector_of(position);
        self.ids[object_id as usize] = IdState::Used;
        self.entities.insert(
            object_id,
            ServerEntity {
                object_id,
                uniqifier: creation_token as u16,
                // No simulator yet: assigned on the next reassignment pass.
                owner: 0,
                entity_type,
                creation_token,
                create_data,
                data: Vec::new(),
                frame_index: self.frame_index,
                position,
                position_known: true,
                sector,
                sector_position,
                velocity: [0.0; 3],
                routing_bucket: 0,
                health: None,
                max_health: None,
                armour: None,
                model: Some(model),
                vehicle_seat: None,
                // Nobody created it but the server itself.
                first_owner: 0,
                nodes: EntityNodeState::default(),
                heading: Some(heading),
                desired_heading: None,
                server_owned: true,
            },
        );
        true
    }

    /// Remove an entity regardless of owner (`DeleteEntity` from a script).
    pub fn despawn_entity(&mut self, object_id: u16) -> bool {
        if self.entities.remove(&object_id).is_none() {
            return false;
        }
        self.ids[object_id as usize] = IdState::Free;
        self.routing_buckets.remove_entity(object_id);
        true
    }

    /// Allocate an object id for a server-created entity.
    ///
    /// Server ids are taken from the top of the space downward while clients
    /// lease from the bottom upward, so the two allocators only meet when the
    /// space is genuinely full.
    fn allocate_server_object_id(&mut self) -> Option<u16> {
        let mut id = self.max_object_id;
        while id > 0 {
            if self.ids[id as usize] == IdState::Free {
                return Some(id);
            }
            id -= 1;
        }
        tracing::error!(
            target: "onesync",
            "object id space exhausted: cannot create a server entity"
        );
        None
    }

    /// Give every ownerless server entity to the nearest player.
    ///
    /// A server-created entity has no simulator until a client adopts it, and
    /// an entity nobody simulates is inert. Reassignment also repairs entities
    /// whose owner disconnected.
    pub fn reassign_ownerless_server_entities(&mut self) {
        let players: Vec<(u32, [f32; 3])> = self
            .entities
            .values()
            .filter(|e| e.entity_type == NetObjEntityType::Player && e.position_known)
            .map(|e| (e.owner, e.position))
            .collect();
        if players.is_empty() {
            return;
        }
        for entity in self.entities.values_mut() {
            if !entity.server_owned || entity.owner != 0 {
                continue;
            }
            let nearest = players.iter().min_by(|(_, a), (_, b)| {
                let da = squared_distance(*a, entity.position);
                let db = squared_distance(*b, entity.position);
                da.total_cmp(&db)
            });
            if let Some((source, _)) = nearest {
                entity.owner = *source;
                tracing::debug!(
                    target: "onesync",
                    object_id = entity.object_id,
                    owner = *source,
                    "server entity assigned to a simulating client"
                );
            }
        }
    }

    /// Drop a client and release every object id it held. Entities it owned
    /// become ownerless (arbitration/migration handled by the NG scheduler).
    /// Returns the object ids of entities the client owned.
    pub fn remove_client(&mut self, source: u32) -> Vec<u16> {
        if let Some(state) = self.clients.remove(&source) {
            for id in state.leased {
                // Only release ids still in the leased state; an id promoted to
                // Used by a create stays claimed while its entity lives (it may
                // have been taken over by another client).
                if self.ids[id as usize] == IdState::Leased {
                    self.ids[id as usize] = IdState::Free;
                }
            }
        }
        let mut orphaned = Vec::new();
        for (&id, ent) in &self.entities {
            if ent.owner == source {
                orphaned.push(id);
            }
        }
        for id in &orphaned {
            // A server-created entity outlives its simulator: it goes back to
            // ownerless and the next reassignment pass hands it to another
            // player. Destroying it would delete a scripted vehicle the moment
            // whoever happened to be driving it disconnected.
            if self.entities.get(id).is_some_and(|ent| ent.server_owned) {
                if let Some(ent) = self.entities.get_mut(id) {
                    ent.owner = 0;
                }
                continue;
            }
            self.entities.remove(id);
            self.ids[*id as usize] = IdState::Free;
            self.routing_buckets.remove_entity(*id);
        }
        self.routing_buckets.remove_player(source);
        orphaned
    }

    /// Lease up to `n` free object ids to a client (low-to-high scan), matching
    /// `GetFreeObjectIds`. Returns the ids and the ready-to-send `msgObjectIds`
    /// packet.
    pub fn lease_object_ids(&mut self, source: u32, n: usize) -> (Vec<u16>, Vec<u8>) {
        let mut ids = Vec::with_capacity(n);
        let mut id: u16 = 1;
        while ids.len() < n && id < self.max_object_id {
            if self.ids[id as usize] == IdState::Free {
                self.ids[id as usize] = IdState::Leased;
                ids.push(id);
            }
            id += 1;
        }
        let state = self
            .clients
            .entry(source)
            .or_insert_with(|| ClientState::new(source as u16));
        state.leased.extend_from_slice(&ids);
        let packet = object_ids::build_object_ids(&ids);
        (ids, packet)
    }

    /// The per-request lease size for this mode (6 big / 32 otherwise).
    pub fn ids_per_request(&self) -> usize {
        object_ids::ids_per_request(self.big_mode)
    }

    /// Override an entity's world position from outside the clone stream
    /// (server-authored placement, tests).
    ///
    /// The sector halves are deliberately left untouched: for a client-owned
    /// entity the owner's next sync is authoritative and must win, so an
    /// override only holds until the next positional clone record.
    pub fn set_entity_position(&mut self, object_id: u16, position: [f32; 3]) {
        if let Some(ent) = self.entities.get_mut(&object_id) {
            ent.position = position;
            ent.position_known = true;
        }
    }

    pub fn set_entity_velocity(&mut self, object_id: u16, velocity: [f32; 3]) {
        if let Some(ent) = self.entities.get_mut(&object_id) {
            ent.velocity = velocity;
        }
    }

    /// Seed positions for player entities the clone stream has not placed yet.
    ///
    /// Positions are decoded from each entity's own sync tree, so this is a
    /// **fallback only**: it fills entities whose position is still unknown and
    /// never overwrites decoded state. It exists so a client that connects
    /// before its first positional clone record is still routable.
    pub fn seed_unknown_player_positions(&mut self, focus: &HashMap<u32, [f32; 3]>) {
        for ent in self.entities.values_mut() {
            if ent.entity_type == NetObjEntityType::Player && !ent.position_known {
                if let Some(&pos) = focus.get(&ent.owner) {
                    ent.position = pos;
                    ent.position_known = true;
                }
            }
        }
    }

    /// Each client's viewpoint, taken from the Player entity it owns.
    ///
    /// This is the authoritative focus for interest management: it comes from
    /// the client's own clone stream, not from a cooperating resource, so it
    /// works with a stock FiveM client and cannot be silently absent.
    pub fn client_focus_map(&self) -> HashMap<u32, ([f32; 3], [f32; 3])> {
        let mut focus = HashMap::with_capacity(self.clients.len());
        for ent in self.entities.values() {
            if ent.entity_type == NetObjEntityType::Player && ent.position_known {
                focus.insert(ent.owner, (ent.position, ent.velocity));
            }
        }
        focus
    }

    /// The list of connected client net ids (for driving the outbound tick).
    pub fn client_sources(&self) -> Vec<u32> {
        self.clients.keys().copied().collect()
    }

    /// Deterministically ordered object ids without cloning entity payloads.
    /// Used by control-plane metadata synchronization before the data-plane
    /// snapshot is built.
    pub fn entity_ids(&self) -> Vec<u16> {
        let mut ids: Vec<_> = self.entities.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Snapshot the world for interest management.
    ///
    /// Entities whose position has never been decoded are excluded: they have
    /// no place in a spatial query, and including them at the world origin
    /// would make every client see them as neighbours of spawn.
    pub fn world_snapshot(&self) -> Vec<EntitySnapshot> {
        let mut snapshot: Vec<_> = self
            .entities
            .values()
            // An entity with no simulating client cannot be cloned: the clone
            // record carries an owner net id, and `0` is nobody. A freshly
            // created server entity waits here for its first reassignment.
            .filter(|e| e.position_known && e.owner != 0)
            .map(|e| EntitySnapshot {
                object_id: e.object_id,
                uniqifier: e.uniqifier,
                owner: e.owner,
                position: e.position,
                velocity: e.velocity,
                entity_type: e.entity_type,
                routing_bucket: e.routing_bucket,
                frame_index: e.frame_index,
                data_len: e.data.len(),
            })
            .collect();
        snapshot.sort_unstable_by_key(|entity| entity.object_id);
        snapshot
    }

    /// Run the outbound sync tick for one client and return the framed
    /// `msgPackedClones` packets to send. `focus` is the client's position.
    /// Produces create/sync/remove records via the NG interest manager, cut as
    /// per-client deltas against the client's view.
    pub fn tick_client(
        &mut self,
        source: u32,
        focus: [f32; 3],
        cfg: &InterestConfig,
    ) -> Vec<Vec<u8>> {
        let world = self.world_snapshot();
        let index = SpatialIndex::build(&world, cfg.query_radius());
        let Some(state) = self.clients.get_mut(&source) else {
            return Vec::new();
        };
        let routing_bucket = self.routing_buckets.player_bucket(source);
        let plan =
            state
                .view
                .tick_with_context(focus, [0.0; 3], routing_bucket, &world, &index, cfg);
        Self::encode_plan(
            &self.entities,
            self.frame_index,
            self.length_hack,
            state,
            &plan,
        )
    }

    /// Plan all connected clients against one immutable world snapshot.
    ///
    /// Rayon mutates disjoint per-client views on its persistent work-stealing
    /// pool. Packet encoding then reads the authoritative registry without
    /// moving ENet ownership away from the gateway thread.
    /// `fallback_focus` only covers clients whose own Player entity has not
    /// been placed by the clone stream yet; decoded positions always win.
    pub fn tick_clients(
        &mut self,
        fallback_focus: &HashMap<u32, [f32; 3]>,
        cfg: &InterestConfig,
    ) -> Vec<ClientTick> {
        let world = self.world_snapshot();
        let index = SpatialIndex::build(&world, cfg.query_radius());
        let focus = self.client_focus_map();
        let buckets: HashMap<u32, u32> = self
            .clients
            .keys()
            .map(|source| (*source, self.routing_buckets.player_bucket(*source)))
            .collect();
        let mut plans: Vec<(u32, TickPlan)> = self
            .clients
            .par_iter_mut()
            .map(|(source, state)| {
                let (client_focus, client_velocity) =
                    focus.get(source).copied().unwrap_or_else(|| {
                        (
                            fallback_focus.get(source).copied().unwrap_or([0.0; 3]),
                            [0.0; 3],
                        )
                    });
                let bucket = buckets.get(source).copied().unwrap_or(0);
                (
                    *source,
                    state.view.tick_with_context(
                        client_focus,
                        client_velocity,
                        bucket,
                        &world,
                        &index,
                        cfg,
                    ),
                )
            })
            .collect();
        plans.sort_unstable_by_key(|(source, _)| *source);

        let entities = &self.entities;
        let frame_index = self.frame_index;
        let length_hack = self.length_hack;
        plans
            .into_iter()
            .filter_map(|(source, plan)| {
                let state = self.clients.get(&source)?;
                let packets = Self::encode_plan(entities, frame_index, length_hack, state, &plan);
                (!packets.is_empty()).then_some(ClientTick { source, packets })
            })
            .collect()
    }

    fn encode_plan(
        entities: &HashMap<u16, ServerEntity>,
        frame_index: u64,
        length_hack: bool,
        state: &ClientState,
        plan: &TickPlan,
    ) -> Vec<Vec<u8>> {
        use baston_protocol::rage::clone::{rec, OutboundClone};
        use baston_protocol::rage::packet::{PackedWriter, MSG_PACKED_CLONES};

        if plan.creates.is_empty() && plan.syncs.is_empty() && plan.removes.is_empty() {
            return Vec::new();
        }

        let mut writer = PackedWriter::new(MSG_PACKED_CLONES, frame_index, length_hack);
        let timestamp = frame_index; // server clock proxy; refined at wire time

        // `state` (borrow of self.clients) and `self.entities` are disjoint
        // fields, so both can be read here without aliasing.
        for (sync_type, ids) in [(rec::CREATE, &plan.creates), (rec::SYNC, &plan.syncs)] {
            for &object_id in ids {
                let Some(e) = entities.get(&object_id) else {
                    continue;
                };
                let base = state.view.base_frame(object_id).unwrap_or(0);
                // Each record type carries the blob authored for it. An entity
                // with no sync blob yet simply has no delta to send — emitting
                // its create payload inside a sync record would mis-frame it.
                let data = if sync_type == rec::CREATE {
                    &e.create_data
                } else if e.data.is_empty() {
                    continue;
                } else {
                    &e.data
                };
                let clone = OutboundClone {
                    sync_type,
                    object_id: e.object_id,
                    owner_net_id: e.owner as u16,
                    entity_type: (sync_type == rec::CREATE).then_some(e.entity_type),
                    creation_token: e.creation_token,
                    uniqifier: e.uniqifier,
                    dependent_frame_index: base,
                    timestamp: timestamp as u32,
                    first_frame_update: false,
                    data,
                };
                writer.write_clone(timestamp, &clone);
            }
        }
        for &(object_id, uniqifier) in &plan.removes {
            writer.write_remove(false, object_id, uniqifier);
        }

        writer.finish()
    }

    /// Apply a `gameStateNAck` (NAK mode): roll the client's delta baselines
    /// back so the next tick re-sends what was missed. No frame-snapshot
    /// backlog is consulted, so the client can never be dropped for a stale
    /// NACK.
    pub fn apply_nack(&mut self, source: u32, nack: &GameStateNAck) {
        let Some(state) = self.clients.get_mut(&source) else {
            return;
        };
        if let Some((first, _last)) = nack.missing_frames {
            // Re-send everything since the first missing frame.
            state.view.rollback_all(first.saturating_sub(1));
        }
        for entry in &nack.ignore_list {
            state
                .view
                .rollback(entry.object_id, entry.last_frame, false);
        }
        for &object_id in &nack.recreate_list {
            state.view.force_recreate(object_id);
        }
    }

    /// Apply a `gameStateAck` (ARQ mode). NG relies on NAK, so this only nudges
    /// the client's confirmed frame and honors any residual ignore/recreate.
    pub fn apply_ack(&mut self, source: u32, ack: &GameStateAck) {
        let Some(state) = self.clients.get_mut(&source) else {
            return;
        };
        state.view.ack_frame(ack.frame_index);
        for entry in &ack.ignore_list {
            state
                .view
                .rollback(entry.object_id, entry.last_frame, false);
        }
        for &object_id in &ack.recreate_list {
            state.view.force_recreate(object_id);
        }
    }
}
