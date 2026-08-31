//! Gateway-side write access to the authoritative world.
//!
//! Scripts run on their own isolate threads while [`ServerGameState`] lives
//! behind `&mut` on the UDP task. So a `CreateVehicle` cannot mutate the world
//! directly: it reserves a network id — which the script needs back
//! immediately — and queues the actual creation for the next sync tick.
//!
//! ## Why the id is reserved here and not by the game state
//!
//! `CreateVehicle` returns a handle synchronously. Deferring the id to the
//! tick would mean handing the script nothing usable. So the reservation is an
//! atomic counter walking **down** from the top of the id space, mirroring
//! [`ServerGameState`]'s own server allocator, while clients lease **upward**
//! from the bottom. The two only meet when the space is genuinely exhausted.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use baston_scripting::{ScriptEntityType, WorldCommand, WorldControl};
use baston_zone::onesync::MAX_OBJECT_ID_NATIVE;
use tokio::sync::mpsc;

/// Queue depth for pending world mutations.
///
/// Entity creation is a script-driven event, not a per-frame one, so this only
/// fills if a resource spawns in a tight loop — in which case dropping is the
/// right answer, loudly.
const COMMAND_CAPACITY: usize = 4096;

/// The write side handed to the script host.
pub struct GatewayWorldControl {
    tx: mpsc::Sender<WorldCommand>,
    /// Next id to hand out, walking downward.
    next_id: AtomicU32,
}

impl GatewayWorldControl {
    /// Build the control surface and the receiver the UDP task drains.
    #[must_use]
    pub fn new() -> (Arc<Self>, mpsc::Receiver<WorldCommand>) {
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        (
            Arc::new(Self {
                tx,
                next_id: AtomicU32::new(u32::from(MAX_OBJECT_ID_NATIVE)),
            }),
            rx,
        )
    }

    /// Carve a block of ids out of this same descending allocator.
    ///
    /// A zone cannot round-trip to the gateway inside `CreateVehicle` — the
    /// native returns its handle on the spot — so it leases ahead and picks
    /// from its block with no I/O.
    ///
    /// Taking the block from *this* counter, rather than partitioning the id
    /// space up front, is what makes the blocks exclusive without any
    /// coordination: one allocator is the single authority, gateway-local
    /// reservations and every zone's block come out of the same descending
    /// sequence, and client leases walk up from the other end. Two zones
    /// cannot mint the same id, so "spawn refused: id already in use" is not a
    /// failure mode a zone can reach.
    ///
    /// Returns `(highest, granted)`: the block is `highest` down to
    /// `highest - granted + 1`. `granted` can be smaller than `count` when the
    /// space is nearly gone, and `None` means nothing is left.
    pub fn reserve_block(&self, count: u32) -> Option<(u32, u32)> {
        if count == 0 {
            return None;
        }
        let mut granted = 0;
        // The closure re-runs on CAS contention; the last run is the one that
        // stuck, so `granted` always matches the value `fetch_update` returns.
        let highest = self
            .next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current == 0 {
                    return None;
                }
                // Never hand out 0 — it is the invalid handle.
                granted = count.min(current);
                Some(current - granted)
            })
            .ok()?;
        (granted > 0).then_some((highest, granted))
    }
}

impl WorldControl for GatewayWorldControl {
    fn is_authoritative(&self) -> bool {
        true
    }

    fn reserve_network_id(&self) -> Option<u32> {
        // `fetch_update` so an exhausted space stays exhausted instead of
        // wrapping around and colliding with live ids.
        self.next_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current > 0).then(|| current - 1)
            })
            .ok()
            .filter(|id| *id > 0)
    }

    fn submit(&self, command: WorldCommand) {
        if self.tx.try_send(command).is_err() {
            tracing::error!(
                target: "onesync",
                "world command dropped: the entity command queue is full"
            );
            metrics::counter!("world_commands_dropped_total").increment(1);
        }
    }
}

/// Translate a script entity class into the network object type used on the
/// wire. Vehicles are created as automobiles: the sync tree is shared by the
/// whole automobile family, and a script that wants a boat or a heli gets the
/// right handling from the model itself.
#[must_use]
pub fn net_object_type(
    entity_type: ScriptEntityType,
) -> baston_protocol::rage::clone::NetObjEntityType {
    use baston_protocol::rage::clone::NetObjEntityType;
    match entity_type {
        ScriptEntityType::Ped => NetObjEntityType::Ped,
        ScriptEntityType::Vehicle => NetObjEntityType::Automobile,
        ScriptEntityType::Object => NetObjEntityType::Object,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_reserved_downward_and_never_repeat() {
        let (control, _rx) = GatewayWorldControl::new();

        let first = control.reserve_network_id().unwrap();
        let second = control.reserve_network_id().unwrap();

        assert_eq!(first, u32::from(MAX_OBJECT_ID_NATIVE));
        assert_eq!(second, first - 1);
    }

    /// An exhausted space must stay exhausted: wrapping would hand out ids
    /// that live entities already hold.
    #[test]
    fn exhaustion_is_terminal() {
        let (control, _rx) = GatewayWorldControl::new();
        control.next_id.store(1, Ordering::Release);

        assert_eq!(control.reserve_network_id(), Some(1));
        assert_eq!(control.reserve_network_id(), None);
        assert_eq!(control.reserve_network_id(), None);
    }

    /// Blocks and single reservations share one counter, so a zone's ids can
    /// never collide with the gateway's own.
    #[test]
    fn blocks_come_out_of_the_same_descending_sequence() {
        let (control, _rx) = GatewayWorldControl::new();
        let top = u32::from(MAX_OBJECT_ID_NATIVE);

        let (highest, granted) = control.reserve_block(4).unwrap();
        assert_eq!((highest, granted), (top, 4));
        // The block owns top..=top-3, so the next single id is below it.
        assert_eq!(control.reserve_network_id(), Some(top - 4));

        let (next_highest, _) = control.reserve_block(4).unwrap();
        assert_eq!(next_highest, top - 5);
    }

    /// A block that would run past the end is truncated, never wrapped, and
    /// never includes 0.
    #[test]
    fn a_block_is_truncated_at_the_end_of_the_space() {
        let (control, _rx) = GatewayWorldControl::new();
        control.next_id.store(3, Ordering::Release);

        assert_eq!(control.reserve_block(10), Some((3, 3)));
        assert_eq!(control.reserve_block(1), None);
        assert_eq!(control.reserve_network_id(), None);
    }

    #[test]
    fn a_zero_length_block_is_refused() {
        let (control, _rx) = GatewayWorldControl::new();
        assert_eq!(control.reserve_block(0), None);
    }

    #[test]
    fn submitted_commands_reach_the_receiver() {
        let (control, mut rx) = GatewayWorldControl::new();
        let command = WorldCommand::Despawn { network_id: 42 };

        control.submit(command);

        assert_eq!(rx.try_recv().ok(), Some(command));
    }
}
