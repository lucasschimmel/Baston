//! OneSync-NG interest management: per-client sparse views, priority
//! accumulators, and a per-tick bandwidth budget.
//!
//! This replaces OneSync Infinity's fixed distance bands (12.5/50/150/250/500
//! ms) with a Tribes/Halo-style priority model: every (client, entity) pair
//! accrues priority each tick from distance, relative velocity, entity type and
//! staleness; each tick we sort by priority and fill the client's byte budget
//! with the entities that matter most. Relevance is recomputed for *every*
//! client every tick from a spatial view (no rotation), so scope entry/exit is
//! reactive at all population sizes.
//!
//! Candidates come from a [`SpatialIndex`] rebuilt once per tick over the world
//! snapshot, so a tick costs O(clients × neighbours) rather than
//! O(clients × entities).
//!
//! Positions are supplied by the caller in that snapshot. In OneSync-NG they
//! are decoded from each entity's own sync tree
//! ([`baston_protocol::rage::sync_parse`]); this module stays agnostic about
//! their origin.

use std::collections::HashMap;

use baston_protocol::rage::clone::NetObjEntityType;

/// Ceiling on a view's accumulated priority. Priority only needs to establish a
/// *relative* ordering, and f32 loses integer precision past ~1.7e7 (so
/// `+staleness_weight` would silently become a no-op and stall the anti-
/// starvation term). Cap well below that; the ordering is unaffected.
const PRIORITY_CAP: f32 = 1.0e6;

/// A minimal per-entity snapshot the interest manager reasons over. The heavy
/// sync-tree blob stays in the registry; here we only need identity, owner,
/// position and freshness.
#[derive(Debug, Clone, Copy)]
pub struct EntitySnapshot {
    pub object_id: u16,
    pub uniqifier: u16,
    pub owner: u32,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub entity_type: NetObjEntityType,
    pub routing_bucket: u32,
    /// Server frame index at the entity's last update (the delta baseline).
    pub frame_index: u64,
    /// Payload size in bytes — used for budget accounting.
    pub data_len: usize,
}

/// Tuning for the priority model and the per-client budget.
#[derive(Debug, Clone, Copy)]
pub struct InterestConfig {
    /// Area-of-interest radius (meters). Entities beyond are not relevant.
    pub aoi_radius: f32,
    /// Bytes a single client may receive per sync tick.
    pub budget_bytes: usize,
    /// Priority gained per tick at zero distance (linearly fades to 0 at
    /// `aoi_radius`).
    pub distance_weight: f32,
    /// Extra priority per meter/second of closing speed toward the client.
    pub closing_weight: f32,
    /// Flat priority added each tick an entity goes unsent (anti-starvation).
    pub staleness_weight: f32,
    /// Extra retention radius for entities already in scope.
    pub hysteresis_m: f32,
    /// Maximum bytes spent on scope removals in one tick.
    pub remove_budget_bytes: usize,
}

impl InterestConfig {
    /// The widest distance at which an entity can still be relevant: the AoI
    /// radius plus the retention margin granted to entities already in scope.
    /// Spatial queries must use this, not `aoi_radius`, or an entity held by
    /// hysteresis would silently drop out of the candidate set.
    #[must_use]
    pub fn query_radius(&self) -> f32 {
        self.aoi_radius + self.hysteresis_m
    }
}

impl Default for InterestConfig {
    fn default() -> Self {
        Self {
            aoi_radius: 424.0,
            budget_bytes: 24 * 1024, // ~0.4 Mbps at 20 Hz
            distance_weight: 10.0,
            closing_weight: 0.5,
            staleness_weight: 1.0,
            hysteresis_m: 20.0,
            remove_budget_bytes: 4 * 1024,
        }
    }
}

impl From<&baston_config::StateSyncConfig> for InterestConfig {
    fn from(value: &baston_config::StateSyncConfig) -> Self {
        Self {
            aoi_radius: value.aoi_radius,
            budget_bytes: value.interest_budget_bytes,
            distance_weight: value.interest_distance_weight,
            closing_weight: value.interest_closing_weight,
            staleness_weight: value.interest_staleness_weight,
            hysteresis_m: value.interest_hysteresis_m,
            remove_budget_bytes: value.interest_remove_budget_bytes,
        }
    }
}

/// Per-(client, entity) view state.
#[derive(Debug, Clone, Copy)]
struct EntityView {
    /// Accumulated, unspent priority.
    priority: f32,
    /// Last frame index we sent this client for this entity (delta baseline).
    base_frame: u64,
    /// Whether the client has been sent a create for this entity.
    created: bool,
    /// True if the entity was relevant this tick (for scope-exit detection).
    seen_this_tick: bool,
    uniqifier: u16,
}

/// Uniform-grid spatial index over one world snapshot.
///
/// Interest is a radius query, so scanning every entity for every client is
/// O(clients × entities) per tick — the dominant cost once a server holds more
/// than a few hundred entities. The grid is rebuilt once per tick from the
/// immutable snapshot and then queried by every client in parallel, which
/// turns the tick into O(clients × neighbours).
///
/// Cells are sized at or above the query radius, so the 3×3 block around a
/// focus point provably covers every entity within that radius. Membership is
/// stored as indices into the caller's snapshot slice, so the index owns no
/// entity data and stays cheap to build.
pub struct SpatialIndex {
    cell_size: f32,
    cells: HashMap<(i32, i32), Vec<u32>>,
}

impl SpatialIndex {
    /// Build an index whose cells are at least `radius` wide.
    ///
    /// `world` must not exceed `u32::MAX` entries; larger snapshots are
    /// truncated rather than silently mis-indexed, which cannot occur in
    /// practice (the object-id space caps the world at 65 535 entities).
    #[must_use]
    pub fn build(world: &[EntitySnapshot], radius: f32) -> Self {
        // A degenerate radius would collapse every entity into one cell and
        // reintroduce the linear scan; keep a sane floor.
        let cell_size = radius.max(1.0);
        let mut cells: HashMap<(i32, i32), Vec<u32>> = HashMap::new();
        let count = world.len().min(u32::MAX as usize);
        for (index, entity) in world.iter().take(count).enumerate() {
            cells
                .entry(cell_of(entity.position, cell_size))
                .or_default()
                .push(index as u32);
        }
        Self { cells, cell_size }
    }

    /// Visit every snapshot index within the query radius of `focus`.
    ///
    /// The callback may be invoked for entities slightly outside the radius —
    /// the caller already applies the exact distance test — but never misses
    /// one inside it.
    pub fn for_each_near(&self, focus: [f32; 3], mut visit: impl FnMut(u32)) {
        let (cx, cy) = cell_of(focus, self.cell_size);
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(bucket) = self.cells.get(&(cx + dx, cy + dy)) {
                    for &index in bucket {
                        visit(index);
                    }
                }
            }
        }
    }
}

/// Interest is a horizontal query, so the grid is 2D — a vertical shaft of
/// entities shares one cell, which is what a city with interiors wants.
fn cell_of(position: [f32; 3], cell_size: f32) -> (i32, i32) {
    (
        (position[0] / cell_size).floor() as i32,
        (position[1] / cell_size).floor() as i32,
    )
}

/// What the tick decided to send one client, in priority order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickPlan {
    /// Object ids to (re)create this tick — send a full create record.
    pub creates: Vec<u16>,
    /// Object ids to send a sync/delta for this tick.
    pub syncs: Vec<u16>,
    /// (object_id, uniqifier) pairs that left scope — send a remove.
    pub removes: Vec<(u16, u16)>,
}

/// One client's sparse view over the world.
pub struct ClientView {
    pub net_id: u16,
    views: HashMap<u16, EntityView>,
}

impl ClientView {
    pub fn new(net_id: u16) -> Self {
        Self {
            net_id,
            views: HashMap::new(),
        }
    }

    /// Number of entities currently in this client's scope.
    pub fn scope_len(&self) -> usize {
        self.views.len()
    }

    /// Recompute relevance and produce this tick's send plan.
    ///
    /// `focus` is the client's position; `world` is the current entity
    /// snapshot. Entities owned by this client are suppressed (owner-echo).
    /// Priority accrues for every relevant entity; the plan is filled in
    /// priority order until the byte budget is exhausted. Entities that fell
    /// out of scope since last tick are emitted as removes.
    pub fn tick(
        &mut self,
        focus: [f32; 3],
        world: &[EntitySnapshot],
        cfg: &InterestConfig,
    ) -> TickPlan {
        let index = SpatialIndex::build(world, cfg.query_radius());
        self.tick_with_context(focus, [0.0; 3], 0, world, &index, cfg)
    }

    /// Bucket-aware interest tick with relative velocity scoring.
    ///
    /// `index` must have been built over `world` with at least
    /// [`InterestConfig::query_radius`] cells, so the neighbourhood query can
    /// never miss a relevant entity.
    pub fn tick_with_context(
        &mut self,
        focus: [f32; 3],
        focus_velocity: [f32; 3],
        routing_bucket: u32,
        world: &[EntitySnapshot],
        index: &SpatialIndex,
        cfg: &InterestConfig,
    ) -> TickPlan {
        // Reset seen flags.
        for v in self.views.values_mut() {
            v.seen_this_tick = false;
        }

        // Candidate = (object_id, priority, snapshot index).
        let mut candidates: Vec<(u16, f32, usize)> = Vec::new();

        index.for_each_near(focus, |i| {
            let i = i as usize;
            let Some(e) = world.get(i) else {
                return;
            };
            if u32::from(self.net_id) == e.owner || e.routing_bucket != routing_bucket {
                return; // owner-echo suppression
            }
            let dx = e.position[0] - focus[0];
            let dy = e.position[1] - focus[1];
            let d2 = dx * dx + dy * dy;
            let already_scoped = self.views.contains_key(&e.object_id);
            let radius = cfg.aoi_radius
                + if already_scoped {
                    cfg.hysteresis_m
                } else {
                    0.0
                };
            if d2 > radius * radius {
                return; // outside AoI
            }
            let dist = d2.sqrt();
            // Distance term: full weight at 0, zero at the radius.
            let dist_term = cfg.distance_weight * (1.0 - dist / cfg.aoi_radius).max(0.0);
            let closing_speed = if dist > f32::EPSILON {
                let relative_velocity = [
                    e.velocity[0] - focus_velocity[0],
                    e.velocity[1] - focus_velocity[1],
                ];
                // AoI is horizontal, so closing velocity must use the same
                // metric. Mixing a Z component with a 2D denominator lets
                // vertical motion dominate priority near the focus point.
                let radial_velocity =
                    (relative_velocity[0] * dx + relative_velocity[1] * dy) / dist;
                (-radial_velocity).max(0.0)
            } else {
                0.0
            };
            let closing_term = cfg.closing_weight * closing_speed;

            let view = self.views.entry(e.object_id).or_insert(EntityView {
                priority: 0.0,
                base_frame: 0,
                created: false,
                seen_this_tick: false,
                uniqifier: e.uniqifier,
            });
            view.seen_this_tick = true;
            view.uniqifier = e.uniqifier;
            view.priority =
                (view.priority + dist_term + closing_term + cfg.staleness_weight).min(PRIORITY_CAP);

            candidates.push((e.object_id, view.priority, i));
        });

        // Scope exits: entities we knew but didn't see this tick.
        let mut plan = TickPlan::default();
        let mut exited: Vec<(u16, u16)> = self
            .views
            .iter()
            .filter(|(_, v)| !v.seen_this_tick)
            .map(|(&id, v)| (id, v.uniqifier))
            .collect();
        exited.sort_unstable_by_key(|(id, _)| *id);
        // A remove record is about 8 bytes. Keep unsent exits in the view so
        // they are retried next tick instead of causing an unbounded storm.
        let max_removes = (cfg.remove_budget_bytes / 8).max(1);
        for (id, uniqifier) in exited.into_iter().take(max_removes) {
            self.views.remove(&id);
            plan.removes.push((id, uniqifier));
        }

        // Highest priority first.
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut spent = 0usize;
        for (object_id, _prio, idx) in candidates {
            let e = &world[idx];
            // Record size estimate: fixed header (~22 bytes) + blob.
            let cost = 22 + e.data_len;
            // Always let the first send of the tick through, even if it alone
            // exceeds the budget — otherwise an entity whose cost > budget_bytes
            // (e.g. a large blob, or a mis-tuned budget) would be starved
            // forever. Once anything has been sent, the budget applies normally.
            if spent > 0 && spent + cost > cfg.budget_bytes {
                continue; // over budget this tick — keep accumulated priority
            }

            let view = self
                .views
                .get_mut(&object_id)
                .expect("candidate was inserted");
            if !view.created {
                plan.creates.push(object_id);
                view.created = true;
                view.base_frame = e.frame_index;
                view.priority = 0.0; // spent
                spent += cost;
            } else if e.frame_index > view.base_frame {
                plan.syncs.push(object_id);
                view.base_frame = e.frame_index;
                view.priority = 0.0; // spent
                spent += cost;
            }
            // else: created and unchanged — nothing to send, keep priority.
        }

        plan
    }

    /// The delta baseline for an entity (the frame this client was last sent).
    pub fn base_frame(&self, object_id: u16) -> Option<u64> {
        self.views.get(&object_id).map(|v| v.base_frame)
    }

    /// Roll the base frame back to `frame` for a NACK'd entity so the next tick
    /// re-sends everything since (NG reliability, task 5). Also forces a
    /// re-create if `recreate` is set.
    pub fn rollback(&mut self, object_id: u16, frame: u64, recreate: bool) {
        if let Some(v) = self.views.get_mut(&object_id) {
            v.base_frame = v.base_frame.min(frame);
            if recreate {
                v.created = false;
            }
        }
    }

    /// Roll *every* entity's base frame back to at most `frame` — the response
    /// to a missing-frame-range NACK. The next tick re-sends everything the
    /// client missed, with no server-side frame-snapshot backlog to consult.
    pub fn rollback_all(&mut self, frame: u64) {
        for v in self.views.values_mut() {
            v.base_frame = v.base_frame.min(frame);
        }
    }

    /// Force a re-create for `object_id` (recreate-list NACK).
    pub fn force_recreate(&mut self, object_id: u16) {
        if let Some(v) = self.views.get_mut(&object_id) {
            v.created = false;
            v.base_frame = 0;
        }
    }

    /// Positively acknowledge a frame (ARQ): entities whose baseline is at or
    /// below `frame` are confirmed delivered. Kept minimal — NG relies on NAK,
    /// so this just prevents needless resend churn.
    pub fn ack_frame(&mut self, _frame: u64) {
        // No backlog to erase; positive acks are advisory in NG's NAK model.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: u16, owner: u32, pos: [f32; 3], frame: u64) -> EntitySnapshot {
        EntitySnapshot {
            object_id: id,
            uniqifier: id,
            owner,
            position: pos,
            velocity: [0.0; 3],
            entity_type: NetObjEntityType::Object,
            routing_bucket: 0,
            frame_index: frame,
            data_len: 64,
        }
    }

    #[test]
    fn entity_in_range_gets_created_then_synced() {
        let cfg = InterestConfig::default();
        let mut view = ClientView::new(1);

        // Tick 1: entity owned by client 2, in range → create.
        let world = vec![snap(10, 2, [10.0, 0.0, 0.0], 5)];
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert_eq!(plan.creates, vec![10]);
        assert!(plan.syncs.is_empty());

        // Tick 2: same frame → nothing to send.
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert!(plan.creates.is_empty());
        assert!(plan.syncs.is_empty());

        // Tick 3: entity updated (newer frame) → sync.
        let world = vec![snap(10, 2, [10.0, 0.0, 0.0], 6)];
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert_eq!(plan.syncs, vec![10]);
    }

    #[test]
    fn owner_echo_is_suppressed() {
        let cfg = InterestConfig::default();
        let mut view = ClientView::new(1);
        // Entity owned by the client itself → never sent back to it.
        let world = vec![snap(10, 1, [5.0, 0.0, 0.0], 1)];
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert!(plan.creates.is_empty());
        assert_eq!(view.scope_len(), 0);
    }

    #[test]
    fn out_of_range_entity_is_not_relevant() {
        let cfg = InterestConfig {
            aoi_radius: 100.0,
            ..Default::default()
        };
        let mut view = ClientView::new(1);
        let world = vec![snap(10, 2, [500.0, 0.0, 0.0], 1)];
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert!(plan.creates.is_empty());
    }

    #[test]
    fn leaving_scope_emits_remove() {
        let cfg = InterestConfig {
            aoi_radius: 100.0,
            ..Default::default()
        };
        let mut view = ClientView::new(1);
        let near = vec![snap(10, 2, [10.0, 0.0, 0.0], 1)];
        view.tick([0.0, 0.0, 0.0], &near, &cfg);
        assert_eq!(view.scope_len(), 1);
        // Entity moves far away → remove.
        let far = vec![snap(10, 2, [900.0, 0.0, 0.0], 2)];
        let plan = view.tick([0.0, 0.0, 0.0], &far, &cfg);
        assert_eq!(plan.removes, vec![(10, 10)]);
        assert_eq!(view.scope_len(), 0);
    }

    #[test]
    fn budget_caps_sends_and_preserves_priority() {
        // Tiny budget: only one entity fits per tick. The nearer one wins, the
        // farther accumulates priority and goes out on a later tick.
        let cfg = InterestConfig {
            budget_bytes: 22 + 64,
            aoi_radius: 1000.0,
            ..Default::default()
        };
        let mut view = ClientView::new(1);
        let world = vec![
            snap(10, 2, [10.0, 0.0, 0.0], 1),  // near
            snap(11, 2, [400.0, 0.0, 0.0], 1), // far
        ];
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert_eq!(
            plan.creates,
            vec![10],
            "nearest entity fits the budget first"
        );
        // Second tick: entity 10 unchanged, entity 11 now has 2 ticks of
        // accumulated priority → it gets sent.
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert_eq!(plan.creates, vec![11]);
    }

    #[test]
    fn nearer_entity_outranks_farther_under_budget() {
        let cfg = InterestConfig {
            budget_bytes: 22 + 64,
            aoi_radius: 1000.0,
            ..Default::default()
        };
        let mut view = ClientView::new(1);
        // Farther entity listed first to prove ordering is by priority, not
        // input order.
        let world = vec![
            snap(11, 2, [400.0, 0.0, 0.0], 1),
            snap(10, 2, [10.0, 0.0, 0.0], 1),
        ];
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert_eq!(plan.creates, vec![10]);
    }

    #[test]
    fn rollback_forces_resend() {
        let cfg = InterestConfig::default();
        let mut view = ClientView::new(1);
        let world = vec![snap(10, 2, [10.0, 0.0, 0.0], 5)];
        view.tick([0.0, 0.0, 0.0], &world, &cfg); // create at frame 5
                                                  // No change → nothing to send.
        assert!(view.tick([0.0, 0.0, 0.0], &world, &cfg).syncs.is_empty());
        // NACK: roll base frame back below 5 → next tick re-syncs.
        view.rollback(10, 4, false);
        let plan = view.tick([0.0, 0.0, 0.0], &world, &cfg);
        assert_eq!(plan.syncs, vec![10]);
    }

    #[test]
    fn routing_buckets_are_isolated() {
        let mut view = ClientView::new(1);
        let mut entity = snap(10, 2, [10.0, 0.0, 0.0], 1);
        entity.routing_bucket = 2;
        let plan = view.tick_with_context(
            [0.0; 3],
            [0.0; 3],
            1,
            &[entity],
            &SpatialIndex::build(&[entity], InterestConfig::default().query_radius()),
            &InterestConfig::default(),
        );
        assert!(plan.creates.is_empty());
    }

    #[test]
    fn closing_velocity_breaks_equal_distance_priority_tie() {
        let cfg = InterestConfig {
            budget_bytes: 22 + 64,
            distance_weight: 0.0,
            closing_weight: 1.0,
            ..Default::default()
        };
        let mut view = ClientView::new(1);
        let mut approaching = snap(11, 2, [100.0, 0.0, 0.0], 1);
        approaching.velocity = [-20.0, 0.0, 0.0];
        let receding = snap(10, 2, [100.0, 0.0, 0.0], 1);
        let world = [receding, approaching];
        let index = SpatialIndex::build(&world, cfg.query_radius());
        let plan = view.tick_with_context([0.0; 3], [0.0; 3], 0, &world, &index, &cfg);
        assert_eq!(plan.creates, vec![11]);
    }

    #[test]
    fn scope_removes_are_budgeted_and_retried() {
        let cfg = InterestConfig {
            aoi_radius: 100.0,
            remove_budget_bytes: 8,
            ..Default::default()
        };
        let mut view = ClientView::new(1);
        let near = vec![
            snap(10, 2, [10.0, 0.0, 0.0], 1),
            snap(11, 2, [20.0, 0.0, 0.0], 1),
        ];
        view.tick([0.0; 3], &near, &cfg);
        let first = view.tick([0.0; 3], &[], &cfg);
        assert_eq!(first.removes.len(), 1);
        let second = view.tick([0.0; 3], &[], &cfg);
        assert_eq!(second.removes.len(), 1);
        assert_ne!(first.removes, second.removes);
    }
}
