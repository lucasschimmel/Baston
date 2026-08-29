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
}

impl WorldControl for GatewayWorldControl {
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

    #[test]
    fn submitted_commands_reach_the_receiver() {
        let (control, mut rx) = GatewayWorldControl::new();
        let command = WorldCommand::Despawn { network_id: 42 };

        control.submit(command);

        assert_eq!(rx.try_recv().ok(), Some(command));
    }
}
