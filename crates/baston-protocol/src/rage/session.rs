//! OneSync session-layer primitives: the virtual slot allocator and the world
//! grid. Both are pure logic, decoupled from the live gateway so they can be
//! unit-tested and reused by zone/gateway integration.
//!
//! Reference: `ServerGameState.cpp` slot virtualization (1642-1723 / 1525-1582)
//! and world-grid ownership (2200-2517), `ServerGameState.h` (1603-1642).

use std::collections::HashMap;

/// Physical GTA player slots (`kGamePlayerCap`). Slot 31 is reserved (every
/// client writes its own sync data as player index 31), so it is never handed
/// out to a *remote* player.
pub const GAME_PLAYER_CAP: usize = 128;

/// The reserved local-player slot index.
pub const RESERVED_LOCAL_SLOT: u8 = 31;

/// Big-mode virtual slot allocator. In OneSync Infinity the server tracks up to
/// `MAX_CLIENTS` connections but only maps the *relevant* remote players onto
/// the 128 physical game slots (skipping 31). A slot is claimed when a remote
/// player enters scope and released when they leave, driving the
/// `playerEnteredScope` / `playerLeftScope` events.
#[derive(Debug, Default)]
pub struct SlotAllocator {
    /// slot index → owning net id.
    slots: HashMap<u8, u32>,
    /// net id → assigned slot (reverse index).
    by_net: HashMap<u32, u8>,
}

impl SlotAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    /// The slot currently assigned to `net_id`, if any.
    pub fn slot_of(&self, net_id: u32) -> Option<u8> {
        self.by_net.get(&net_id).copied()
    }

    /// Whether `net_id` currently holds a slot (is "in scope" globally).
    pub fn is_assigned(&self, net_id: u32) -> bool {
        self.by_net.contains_key(&net_id)
    }

    pub fn assigned_count(&self) -> usize {
        self.by_net.len()
    }

    /// Assign the lowest free slot (0..128, skipping 31) to `net_id`. Returns
    /// the slot, or `None` if all game slots are full. Idempotent: re-assigning
    /// an already-mapped net id returns its existing slot.
    pub fn assign(&mut self, net_id: u32) -> Option<u8> {
        if let Some(&existing) = self.by_net.get(&net_id) {
            return Some(existing);
        }
        for slot in 0..GAME_PLAYER_CAP as u8 {
            if slot == RESERVED_LOCAL_SLOT {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(e) = self.slots.entry(slot) {
                e.insert(net_id);
                self.by_net.insert(net_id, slot);
                return Some(slot);
            }
        }
        None
    }

    /// Release `net_id`'s slot (scope exit / disconnect). Returns the freed
    /// slot if one was held.
    pub fn release(&mut self, net_id: u32) -> Option<u8> {
        let slot = self.by_net.remove(&net_id)?;
        self.slots.remove(&slot);
        Some(slot)
    }
}

/// World-grid geometry. The GTA plane spans −8192..+8192 on each axis; a sector
/// is `WORLD_GRID_CELL` units wide, giving `WORLD_GRID_DIM` cells per axis.
pub const WORLD_OFFSET: f32 = 8192.0;
pub const WORLD_GRID_CELL: f32 = 150.0;
pub const WORLD_GRID_DIM: usize = 256;
/// Half-extent of the claim window around a player's focus (SGS.cpp:2383).
pub const WORLD_GRID_CLAIM_RADIUS: f32 = 299.0;
/// Per-client world-grid slice budget (`WorldGridState::entries`).
pub const WORLD_GRID_ENTRIES: usize = 32;

/// Convert a world coordinate on one axis to a sector index, clamped to the
/// grid. Mirrors `std::max(pos + 8192, 0) / 150` (SGS.cpp:2383-2386).
pub fn sector_index(coord: f32) -> usize {
    let s = ((coord + WORLD_OFFSET).max(0.0) / WORLD_GRID_CELL) as usize;
    s.min(WORLD_GRID_DIM - 1)
}

/// Inclusive sector range `[min, max]` on one axis for a focus coordinate's
/// ±[`WORLD_GRID_CLAIM_RADIUS`] window.
pub fn claim_range(coord: f32) -> (usize, usize) {
    (
        sector_index(coord - WORLD_GRID_CLAIM_RADIUS),
        sector_index(coord + WORLD_GRID_CLAIM_RADIUS),
    )
}

/// One owned cell in a client's world-grid slice (`WorldGridEntry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldGridEntry {
    pub sector_x: u8,
    pub sector_y: u8,
    pub net_id: u16,
}

/// A single routing bucket's world grid: an acceleration table mapping each
/// cell to its owning net id (`0xFFFF` = unowned), plus per-client owned-cell
/// slices. Population-parenting decisions read `owner_of`.
pub struct WorldGrid {
    /// `accel.netIDs[x][y]` — flattened. `u16::MAX` means unowned.
    accel: Vec<u16>,
    /// net id → its owned cells (≤ [`WORLD_GRID_ENTRIES`]).
    slices: HashMap<u16, Vec<WorldGridEntry>>,
}

impl Default for WorldGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl WorldGrid {
    pub fn new() -> Self {
        Self {
            accel: vec![u16::MAX; WORLD_GRID_DIM * WORLD_GRID_DIM],
            slices: HashMap::new(),
        }
    }

    #[inline]
    fn idx(x: usize, y: usize) -> usize {
        x * WORLD_GRID_DIM + y
    }

    /// The net id owning cell `(x, y)`, or `None` if unowned.
    pub fn owner_of(&self, x: usize, y: usize) -> Option<u16> {
        match self.accel[Self::idx(x, y)] {
            u16::MAX => None,
            n => Some(n),
        }
    }

    pub fn owned_cells(&self, net_id: u16) -> &[WorldGridEntry] {
        self.slices.get(&net_id).map_or(&[], |v| v.as_slice())
    }

    /// Update `net_id`'s claimed cells to exactly cover the window around its
    /// focus `(x, y)`, claiming only currently-unowned cells and up to its
    /// [`WORLD_GRID_ENTRIES`] budget, and releasing cells it no longer covers.
    /// Returns `true` if the client's slice changed (a delta to replicate via
    /// `msgWorldGrid3`). Mirrors the claim/disown pass (SGS.cpp:2200-2273).
    pub fn update_focus(&mut self, net_id: u16, focus_x: f32, focus_y: f32) -> bool {
        let (min_x, max_x) = claim_range(focus_x);
        let (min_y, max_y) = claim_range(focus_y);

        let previous = self.slices.get(&net_id).cloned().unwrap_or_default();

        // Release cells the client used to own but no longer covers.
        for e in &previous {
            let inside = (e.sector_x as usize) >= min_x
                && (e.sector_x as usize) <= max_x
                && (e.sector_y as usize) >= min_y
                && (e.sector_y as usize) <= max_y;
            if !inside {
                let i = Self::idx(e.sector_x as usize, e.sector_y as usize);
                if self.accel[i] == net_id {
                    self.accel[i] = u16::MAX;
                }
            }
        }

        // Claim covered, unowned cells up to budget.
        let mut kept: Vec<WorldGridEntry> = previous
            .iter()
            .copied()
            .filter(|e| {
                (e.sector_x as usize) >= min_x
                    && (e.sector_x as usize) <= max_x
                    && (e.sector_y as usize) >= min_y
                    && (e.sector_y as usize) <= max_y
            })
            .collect();

        for x in min_x..=max_x {
            for y in min_y..=max_y {
                if kept.len() >= WORLD_GRID_ENTRIES {
                    break;
                }
                let i = Self::idx(x, y);
                if self.accel[i] == u16::MAX {
                    self.accel[i] = net_id;
                    kept.push(WorldGridEntry {
                        sector_x: x as u8,
                        sector_y: y as u8,
                        net_id,
                    });
                } else if self.accel[i] == net_id
                    && !kept
                        .iter()
                        .any(|e| e.sector_x as usize == x && e.sector_y as usize == y)
                {
                    kept.push(WorldGridEntry {
                        sector_x: x as u8,
                        sector_y: y as u8,
                        net_id,
                    });
                }
            }
        }

        let changed = kept != previous;
        self.slices.insert(net_id, kept);
        changed
    }

    /// Drop all of `net_id`'s claims (disconnect / zone handoff).
    pub fn remove_client(&mut self, net_id: u16) {
        if let Some(cells) = self.slices.remove(&net_id) {
            for e in cells {
                let i = Self::idx(e.sector_x as usize, e.sector_y as usize);
                if self.accel[i] == net_id {
                    self.accel[i] = u16::MAX;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_skip_reserved_31() {
        let mut a = SlotAllocator::new();
        let mut seen = Vec::new();
        for net in 0..40u32 {
            seen.push(a.assign(net).unwrap());
        }
        assert!(!seen.contains(&RESERVED_LOCAL_SLOT));
        // First 31 slots are 0..=30, then jump to 32.
        assert_eq!(seen[30], 30);
        assert_eq!(seen[31], 32);
    }

    #[test]
    fn slot_assign_is_idempotent_and_releasable() {
        let mut a = SlotAllocator::new();
        let s = a.assign(7).unwrap();
        assert_eq!(a.assign(7), Some(s));
        assert_eq!(a.assigned_count(), 1);
        assert_eq!(a.release(7), Some(s));
        assert!(!a.is_assigned(7));
        // Freed slot is reused.
        assert_eq!(a.assign(9), Some(s));
    }

    #[test]
    fn slot_exhaustion_returns_none() {
        let mut a = SlotAllocator::new();
        // 127 assignable slots (128 minus reserved 31).
        for net in 0..127u32 {
            assert!(a.assign(net).is_some(), "net {net}");
        }
        assert_eq!(a.assign(1000), None);
        // Releasing one frees exactly one.
        a.release(0);
        assert!(a.assign(1000).is_some());
    }

    #[test]
    fn sector_math_matches_engine_formula() {
        // Origin maps to sector 8192/150 = 54.
        assert_eq!(sector_index(0.0), 54);
        // Far negative clamps to 0.
        assert_eq!(sector_index(-9000.0), 0);
        // In-range positive: (9000 + 8192) / 150 = 114.
        assert_eq!(sector_index(9000.0), 114);
        // Beyond the grid clamps to the last cell.
        assert_eq!(sector_index(40000.0), WORLD_GRID_DIM - 1);
    }

    #[test]
    fn focus_claims_unowned_cells_and_reports_delta() {
        let mut g = WorldGrid::new();
        assert!(g.update_focus(1, 0.0, 0.0));
        // Re-running with the same focus claims nothing new → no delta.
        assert!(!g.update_focus(1, 0.0, 0.0));
        // The central cell is owned by net 1.
        let (sx, sy) = (sector_index(0.0), sector_index(0.0));
        assert_eq!(g.owner_of(sx, sy), Some(1));
    }

    #[test]
    fn cells_are_not_stolen_between_clients() {
        let mut g = WorldGrid::new();
        g.update_focus(1, 0.0, 0.0);
        // A second client at the same focus cannot take cells already owned.
        g.update_focus(2, 0.0, 0.0);
        let (sx, sy) = (sector_index(0.0), sector_index(0.0));
        assert_eq!(g.owner_of(sx, sy), Some(1));
        // Client 2 still owns nothing at the contested center but may own the
        // fringe cells client 1 didn't reach within its 32-cell budget.
        assert!(g.owned_cells(2).iter().all(|e| e.net_id == 2));
    }

    #[test]
    fn moving_focus_releases_old_cells() {
        let mut g = WorldGrid::new();
        g.update_focus(1, 0.0, 0.0);
        let (old_x, old_y) = (sector_index(0.0), sector_index(0.0));
        // Move far away; the old center cell should be released.
        g.update_focus(1, 5000.0, 5000.0);
        assert_eq!(g.owner_of(old_x, old_y), None);
    }

    #[test]
    fn remove_client_frees_all_its_cells() {
        let mut g = WorldGrid::new();
        g.update_focus(3, 100.0, 100.0);
        assert!(!g.owned_cells(3).is_empty());
        g.remove_client(3);
        assert!(g.owned_cells(3).is_empty());
        let (sx, sy) = (sector_index(100.0), sector_index(100.0));
        assert_eq!(g.owner_of(sx, sy), None);
    }

    #[test]
    fn claim_budget_is_capped() {
        let mut g = WorldGrid::new();
        // A wide window would cover ~4x4=16.. up to more cells; ensure never
        // more than the 32-entry budget.
        g.update_focus(1, 0.0, 0.0);
        assert!(g.owned_cells(1).len() <= WORLD_GRID_ENTRIES);
    }
}
