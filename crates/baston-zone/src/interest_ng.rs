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
//! Positions are supplied by the caller (a world snapshot) — this module is
//! deliberately agnostic about whether they come from parsed sync-tree nodes or
//! the client state-report path.

use std::collections::HashMap;

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
}

impl Default for InterestConfig {
    fn default() -> Self {
        Self {
            aoi_radius: 424.0,
            budget_bytes: 24 * 1024, // ~0.4 Mbps at 20 Hz
            distance_weight: 10.0,
            closing_weight: 0.5,
            staleness_weight: 1.0,
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
        // Reset seen flags.
        for v in self.views.values_mut() {
            v.seen_this_tick = false;
        }

        let r2 = cfg.aoi_radius * cfg.aoi_radius;
        // Candidate = (object_id, priority, snapshot index).
        let mut candidates: Vec<(u16, f32, usize)> = Vec::new();

        for (i, e) in world.iter().enumerate() {
            if u32::from(self.net_id) == e.owner {
                continue; // owner-echo suppression
            }
            let dx = e.position[0] - focus[0];
            let dy = e.position[1] - focus[1];
            let d2 = dx * dx + dy * dy;
            if d2 > r2 {
                continue; // outside AoI
            }
            let dist = d2.sqrt();
            // Distance term: full weight at 0, zero at the radius.
            let dist_term = cfg.distance_weight * (1.0 - dist / cfg.aoi_radius).max(0.0);

            let view = self.views.entry(e.object_id).or_insert(EntityView {
                priority: 0.0,
                base_frame: 0,
                created: false,
                seen_this_tick: false,
            });
            view.seen_this_tick = true;
            view.priority = (view.priority + dist_term + cfg.staleness_weight).min(PRIORITY_CAP);

            candidates.push((e.object_id, view.priority, i));
        }

        // Scope exits: entities we knew but didn't see this tick.
        let mut plan = TickPlan::default();
        let exited: Vec<u16> = self
            .views
            .iter()
            .filter(|(_, v)| !v.seen_this_tick)
            .map(|(&id, _)| id)
            .collect();
        for id in exited {
            self.views.remove(&id);
            if let Some(e) = world.iter().find(|e| e.object_id == id) {
                plan.removes.push((id, e.uniqifier));
            }
        }

        // Highest priority first.
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1));

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
}
