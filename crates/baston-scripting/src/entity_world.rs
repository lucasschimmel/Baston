//! Script-visible view of the authoritative networked world.
//!
//! Server natives need to answer questions about entities — who owns this
//! vehicle, where is that ped, what type is this handle — but the authoritative
//! state lives in the game-state task, behind `&mut` and on the packet hot
//! path. Locking it from the script runtime would put every native call in
//! contention with ingestion.
//!
//! So the authority publishes a read-optimised mirror here once per sync tick,
//! exactly as routing state is mirrored through [`crate::RoutingControl`], and
//! natives read it lock-free. The mirror is at most one tick stale, which is
//! the same freshness a script would get from any other server-side observation
//! of a client-owned entity.
//!
//! ## Handles are network ids
//!
//! A script handle *is* the OneSync object id. That is the only entity identity
//! the server and every client already agree on, so `NetworkGetNetworkIdFromEntity`
//! is the identity function and an entity argument can be sent to a client
//! verbatim for it to resolve locally. No handle translation table exists, and
//! none is needed.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

/// Entity classes as reported by `GET_ENTITY_TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScriptEntityType {
    Ped = 1,
    Vehicle = 2,
    Object = 3,
}

impl ScriptEntityType {
    /// The integer `GET_ENTITY_TYPE` returns.
    #[must_use]
    pub fn as_native(self) -> u8 {
        self as u8
    }
}

/// What the mirror knows about one networked entity.
///
/// Deliberately narrow: it holds what the server can derive today from decoded
/// sync trees. Fields are added as node decoders land, and a native that needs
/// one that is missing stays unimplemented rather than inventing a value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntitySummary {
    /// Network id, which is also the script handle.
    pub network_id: u32,
    /// Net id of the client that owns (and therefore simulates) the entity.
    pub owner: u32,
    pub entity_type: ScriptEntityType,
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub routing_bucket: u32,
    /// Health as the server decoded it from the entity's own sync tree.
    /// `None` means the entity has not reported one yet — natives must say so
    /// rather than substituting a plausible number.
    pub health: Option<f32>,
    pub armour: Option<f32>,
    /// Model hash, once a creation node has been seen.
    pub model: Option<u32>,
}

struct Entry {
    summary: EntitySummary,
    /// Publication generation this entry was last seen in; entries left behind
    /// by a generation are pruned.
    revision: u64,
}

/// Lock-free mirror of the networked world, shared by the script host.
#[derive(Default)]
pub struct EntityWorldView {
    entities: DashMap<u32, Entry>,
    revision: AtomicU64,
    /// Player net id → the ped entity it owns, for `GetPlayerPed`.
    player_peds: DashMap<u32, u32>,
}

impl EntityWorldView {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the mirror's contents with the authority's current world.
    ///
    /// Upserts every entity of the incoming generation, then prunes anything
    /// the authority no longer reports — so a despawned entity stops existing
    /// for scripts within one tick, without rebuilding the map.
    pub fn publish(&self, entities: impl IntoIterator<Item = EntitySummary>) {
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        self.player_peds.clear();
        for summary in entities {
            if summary.entity_type == ScriptEntityType::Ped {
                // Last writer wins; a client owns exactly one player ped.
                self.player_peds.insert(summary.owner, summary.network_id);
            }
            self.entities
                .entry(summary.network_id)
                .and_modify(|entry| {
                    entry.summary = summary;
                    entry.revision = revision;
                })
                .or_insert(Entry { summary, revision });
        }
        self.entities.retain(|_, entry| entry.revision == revision);
    }

    /// Publication counter — lets callers cheaply detect a stale read.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn get(&self, network_id: u32) -> Option<EntitySummary> {
        self.entities.get(&network_id).map(|entry| entry.summary)
    }

    #[must_use]
    pub fn exists(&self, network_id: u32) -> bool {
        self.entities.contains_key(&network_id)
    }

    /// The client that owns the entity, or `None` when the entity is unknown.
    ///
    /// This is what `NETWORK_GET_ENTITY_OWNER` answers, and what decides which
    /// client a context-routed native is dispatched to.
    #[must_use]
    pub fn owner(&self, network_id: u32) -> Option<u32> {
        self.entities
            .get(&network_id)
            .map(|entry| entry.summary.owner)
    }

    /// The ped entity owned by a player (`GET_PLAYER_PED`).
    #[must_use]
    pub fn player_ped(&self, player: u32) -> Option<u32> {
        self.player_peds.get(&player).map(|entry| *entry)
    }

    /// Every known handle of one class, ascending — `GET_ALL_VEHICLES` & co.
    /// Sorted so repeated calls give scripts a stable order.
    #[must_use]
    pub fn ids_of_type(&self, entity_type: ScriptEntityType) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .entities
            .iter()
            .filter(|entry| entry.summary.entity_type == entity_type)
            .map(|entry| entry.summary.network_id)
            .collect();
        ids.sort_unstable();
        ids
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

/// What a script asked the authoritative world to do.
///
/// Creation and deletion are mutations of state that lives on the game-state
/// task, so they are submitted as commands and applied on its next tick rather
/// than reaching across a lock. The network id is chosen up front — a script
/// needs a handle back from `CreateVehicle` immediately, not one tick later.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorldCommand {
    Spawn {
        network_id: u32,
        entity_type: ScriptEntityType,
        model: u32,
        position: [f32; 3],
        /// Heading in degrees.
        heading: f32,
        /// Objects only: whether the object simulates physics.
        dynamic: bool,
    },
    Despawn {
        network_id: u32,
    },
}

/// Write access to the authoritative world, from the scripting side.
pub trait WorldControl: Send + Sync {
    /// Reserve a network id for a server-created entity, or `None` when the id
    /// space is exhausted. Reserved ids are never handed out twice.
    fn reserve_network_id(&self) -> Option<u32>;

    /// Queue a mutation for the authoritative world.
    fn submit(&self, command: WorldCommand);
}

/// No authoritative world wired (OneSync off): entity creation is refused
/// rather than silently pretended.
pub struct NoWorldControl;

impl WorldControl for NoWorldControl {
    fn reserve_network_id(&self) -> Option<u32> {
        None
    }

    fn submit(&self, _command: WorldCommand) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: u32, owner: u32, entity_type: ScriptEntityType) -> EntitySummary {
        EntitySummary {
            network_id: id,
            owner,
            entity_type,
            position: [id as f32, 0.0, 0.0],
            velocity: [0.0; 3],
            routing_bucket: 0,
            health: None,
            armour: None,
            model: None,
        }
    }

    #[test]
    fn publish_exposes_owner_and_type() {
        let view = EntityWorldView::new();
        view.publish([
            summary(10, 1, ScriptEntityType::Ped),
            summary(11, 1, ScriptEntityType::Vehicle),
        ]);

        assert_eq!(view.owner(10), Some(1));
        assert_eq!(view.owner(11), Some(1));
        assert_eq!(view.owner(12), None, "unknown handles have no owner");
        assert_eq!(view.get(11).unwrap().entity_type, ScriptEntityType::Vehicle);
        assert_eq!(view.ids_of_type(ScriptEntityType::Vehicle), vec![11]);
    }

    /// A despawned entity must stop existing for scripts, otherwise natives
    /// keep answering about a world that no longer exists.
    #[test]
    fn republishing_prunes_entities_the_authority_dropped() {
        let view = EntityWorldView::new();
        view.publish([
            summary(10, 1, ScriptEntityType::Ped),
            summary(11, 1, ScriptEntityType::Vehicle),
        ]);
        assert_eq!(view.len(), 2);

        view.publish([summary(10, 1, ScriptEntityType::Ped)]);

        assert!(view.exists(10));
        assert!(!view.exists(11), "the vehicle is gone");
        assert_eq!(view.len(), 1);
    }

    #[test]
    fn ownership_changes_are_reflected() {
        let view = EntityWorldView::new();
        view.publish([summary(11, 1, ScriptEntityType::Vehicle)]);
        assert_eq!(view.owner(11), Some(1));

        view.publish([summary(11, 2, ScriptEntityType::Vehicle)]);

        assert_eq!(view.owner(11), Some(2), "takeover is visible to scripts");
        assert_eq!(view.len(), 1, "the entity was updated, not duplicated");
    }

    #[test]
    fn player_ped_resolves_and_clears() {
        let view = EntityWorldView::new();
        view.publish([
            summary(10, 7, ScriptEntityType::Ped),
            summary(11, 7, ScriptEntityType::Vehicle),
        ]);
        assert_eq!(view.player_ped(7), Some(10));

        // The player left: its ped is no longer published.
        view.publish([summary(11, 7, ScriptEntityType::Vehicle)]);

        assert_eq!(view.player_ped(7), None);
    }

    #[test]
    fn revision_advances_per_publication() {
        let view = EntityWorldView::new();
        let before = view.revision();
        view.publish([summary(10, 1, ScriptEntityType::Ped)]);
        view.publish([summary(10, 1, ScriptEntityType::Ped)]);
        assert_eq!(view.revision(), before + 2);
    }

    #[test]
    fn empty_publication_clears_the_world() {
        let view = EntityWorldView::new();
        view.publish([summary(10, 1, ScriptEntityType::Ped)]);
        view.publish([]);
        assert!(view.is_empty());
        assert_eq!(view.player_ped(1), None);
    }
}
