//! Zone-side write access to the gateway's authoritative world.
//!
//! Zones run the resources, but the world clients actually talk to lives in the
//! gateway process. So a `CreateVehicle` in a zone has to cross a process
//! boundary — and it has to do so without blocking, because the native returns
//! its handle on the spot.
//!
//! Two halves, and the split is the whole design:
//!
//! - **Ids are leased ahead.** The gateway carves a block out of its own
//!   descending allocator and hands it over. The zone then mints from that
//!   block with a mutex and no I/O, so the native answers immediately. Because
//!   every block comes from the one allocator, blocks are exclusive by
//!   construction: two zones cannot mint the same id, and a zone cannot collide
//!   with a client lease.
//! - **Mutations are shipped asynchronously.** `submit` queues; a single task
//!   drains, batches, and sends. One task means the order a script wrote is the
//!   order the world sees — a `Despawn` can never overtake the `Spawn` it
//!   undoes.
//!
//! When the block runs dry the native returns 0, the invalid handle, rather
//! than a plausible number for an entity that will never exist. See
//! `create_entity_handle` in `baston-scripting`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use baston_protocol::mesh::gateway_service_client::GatewayServiceClient;
use baston_protocol::mesh::{
    world_command::Command, DespawnEntity, EntityClass, LeaseNetworkIdsRequest, SpawnEntity,
    WorldCommand as WireCommand, WorldCommandBatch,
};
use baston_scripting::{ScriptEntityType, WorldCommand, WorldControl};
use tokio::sync::{mpsc, Notify};
use tonic::transport::Channel;

/// Ids requested per lease.
///
/// The native id space is 8192 wide in total and shared with client leases, so
/// a block is deliberately modest: refilling costs one RPC off the hot path,
/// while a greedy block would starve the zones that register later.
const BLOCK_SIZE: u32 = 256;

/// Refill once the pool drops to this. Early enough that the RPC completes
/// long before a script can drain the remainder.
const LOW_WATER: u32 = 64;

/// Queue depth for pending mutations, matching the gateway's own.
const COMMAND_CAPACITY: usize = 4096;

/// Commands per batch. A script spawning in a loop then costs one round trip
/// per 64 entities instead of one per entity.
const BATCH_MAX: usize = 64;

/// Attempts before a batch is given up on. Retrying in place is what keeps
/// ordering: the drain task does not move on until the batch lands or dies.
const SEND_ATTEMPTS: u32 = 3;

/// Wait after a failed lease when the gateway is merely unreachable.
const LEASE_RETRY: Duration = Duration::from_secs(2);

/// Wait after the gateway reports the id space exhausted. Nothing the zone
/// does will help; retry rarely enough to stay out of the log.
const LEASE_EXHAUSTED_RETRY: Duration = Duration::from_secs(30);

/// One leased block, consumed downward from `next`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdBlock {
    next: u32,
    remaining: u32,
}

impl IdBlock {
    fn take(&mut self) -> Option<u32> {
        // 0 is the invalid handle. The gateway's allocator never grants a block
        // that reaches it, but a block is cheap to make honest on its own.
        if self.remaining == 0 || self.next == 0 {
            return None;
        }
        let id = self.next;
        self.remaining -= 1;
        self.next = self.next.saturating_sub(1);
        Some(id)
    }
}

/// Blocks held by this zone. Several can be live at once: a refill arrives
/// while the previous block still has ids, and discarding that remainder would
/// leak a slice of a 8192-wide space every time.
#[derive(Debug, Default)]
struct IdPool {
    blocks: VecDeque<IdBlock>,
    remaining: u32,
}

impl IdPool {
    fn take(&mut self) -> Option<u32> {
        while let Some(block) = self.blocks.front_mut() {
            if let Some(id) = block.take() {
                self.remaining -= 1;
                return Some(id);
            }
            // A block is normally abandoned empty; one abandoned with a count
            // left hit the bottom of the space, so drop its claim too rather
            // than leaving the pool believing in ids it cannot produce.
            let abandoned = self.blocks.pop_front().map_or(0, |block| block.remaining);
            self.remaining = self.remaining.saturating_sub(abandoned);
        }
        None
    }

    fn push(&mut self, block: IdBlock) {
        self.remaining += block.remaining;
        self.blocks.push_back(block);
    }
}

/// The write side handed to the zone's script host.
#[derive(Debug)]
pub struct ZoneWorldControl {
    zone_id: String,
    pool: Mutex<IdPool>,
    refill: Arc<Notify>,
    tx: mpsc::Sender<WorldCommand>,
}

/// Why a zone has no world to create entities in.
#[derive(Debug)]
pub enum WorldUnavailable {
    /// The gateway answered, and the answer is no — it has no authoritative
    /// world (OneSync off). Permanent for this run: do not wire a control.
    Refused(String),
}

impl ZoneWorldControl {
    /// Lease a first block and start the refill and drain tasks.
    ///
    /// `Err` means the gateway has no authoritative world; the caller should
    /// leave the script host on `NoWorldControl` and say so at boot. A gateway
    /// that is merely unreachable is *not* an error: the control is wired with
    /// an empty pool and heals when the refill lands, because a zone outliving
    /// a gateway restart is normal.
    pub async fn connect(
        zone_id: String,
        gateway: GatewayServiceClient<Channel>,
    ) -> Result<Arc<Self>, WorldUnavailable> {
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let control = Arc::new(Self {
            zone_id: zone_id.clone(),
            pool: Mutex::new(IdPool::default()),
            refill: Arc::new(Notify::new()),
            tx,
        });

        let mut initial = gateway.clone();
        match lease_block(&mut initial, &zone_id).await {
            Lease::Granted(block) => control.pool().push(block),
            Lease::Refused(message) => return Err(WorldUnavailable::Refused(message)),
            Lease::Unavailable => {
                tracing::warn!(target: "zone", zone = %zone_id,
                    "could not lease network ids at boot — entity creation is unavailable \
                     until the gateway answers");
            }
        }

        tokio::spawn(run_refill(
            Arc::clone(&control),
            gateway.clone(),
            zone_id.clone(),
        ));
        tokio::spawn(run_drain(rx, gateway, zone_id));
        Ok(control)
    }

    fn pool(&self) -> std::sync::MutexGuard<'_, IdPool> {
        self.pool.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl WorldControl for ZoneWorldControl {
    fn is_authoritative(&self) -> bool {
        true
    }

    fn reserve_network_id(&self) -> Option<u32> {
        let (id, remaining) = {
            let mut pool = self.pool();
            (pool.take(), pool.remaining)
        };
        // Ask for more outside the lock, and ask on the way down rather than at
        // zero: a refill is a round trip, and a script that spawns in a loop
        // would otherwise drain the tail before it lands.
        if remaining <= LOW_WATER {
            self.refill.notify_one();
        }
        if id.is_none() {
            metrics::counter!("zone_network_id_exhausted_total").increment(1);
        }
        id
    }

    fn submit(&self, command: WorldCommand) {
        if self.tx.try_send(command).is_err() {
            tracing::error!(
                target: "zone",
                zone = %self.zone_id,
                "world command dropped: the entity command queue is full"
            );
            metrics::counter!("zone_world_commands_dropped_total").increment(1);
        }
    }
}

enum Lease {
    Granted(IdBlock),
    /// The gateway has no authoritative world. Permanent.
    Refused(String),
    /// Transient: unreachable, or the space is momentarily exhausted.
    Unavailable,
}

async fn lease_block(gateway: &mut GatewayServiceClient<Channel>, zone_id: &str) -> Lease {
    let req = LeaseNetworkIdsRequest {
        zone_id: zone_id.to_owned(),
        count: BLOCK_SIZE,
    };
    match gateway.lease_network_ids(req).await {
        Ok(resp) => {
            let resp = resp.into_inner();
            if resp.ok && resp.granted > 0 {
                tracing::info!(target: "zone", zone = %zone_id,
                    "leased {} network ids ({}..={})",
                    resp.granted, resp.first_id - resp.granted + 1, resp.first_id);
                Lease::Granted(IdBlock {
                    next: resp.first_id,
                    remaining: resp.granted,
                })
            } else if resp.message.contains("onesync") {
                Lease::Refused(resp.message)
            } else {
                tracing::error!(target: "zone", zone = %zone_id,
                    "network id lease refused: {}", resp.message);
                Lease::Unavailable
            }
        }
        Err(status) => {
            tracing::warn!(target: "zone", zone = %zone_id, error = %status,
                "network id lease failed");
            metrics::counter!("zone_network_id_lease_failures_total").increment(1);
            Lease::Unavailable
        }
    }
}

/// Top the pool up whenever a reservation drops it below the low-water mark.
async fn run_refill(
    control: Arc<ZoneWorldControl>,
    mut gateway: GatewayServiceClient<Channel>,
    zone_id: String,
) {
    let refill = Arc::clone(&control.refill);
    loop {
        refill.notified().await;
        // One notification can arrive per reservation while the pool is low;
        // top up to the mark rather than once per notification.
        while control.pool().remaining <= LOW_WATER {
            match lease_block(&mut gateway, &zone_id).await {
                Lease::Granted(block) => {
                    control.pool().push(block);
                    metrics::counter!("zone_network_id_refills_total").increment(1);
                }
                Lease::Refused(message) => {
                    // The gateway lost its world (restarted with onesync off).
                    // Stop asking; the natives now return the invalid handle.
                    tracing::error!(target: "zone", zone = %zone_id,
                        "network id leases refused permanently: {message}");
                    return;
                }
                Lease::Unavailable => {
                    tokio::time::sleep(LEASE_RETRY).await;
                    // Back off harder once the retry itself keeps failing, so
                    // an exhausted space does not spin.
                    if control.pool().remaining == 0 {
                        tokio::time::sleep(LEASE_EXHAUSTED_RETRY).await;
                    }
                }
            }
        }
    }
}

/// Drain the command queue into batched RPCs.
///
/// Single task on purpose: sequential sends are what make the batch order the
/// script's order. Concurrency here would let a `Despawn` overtake its `Spawn`.
async fn run_drain(
    mut rx: mpsc::Receiver<WorldCommand>,
    mut gateway: GatewayServiceClient<Channel>,
    zone_id: String,
) {
    let mut batch: Vec<WorldCommand> = Vec::with_capacity(BATCH_MAX);
    while let Some(first) = rx.recv().await {
        batch.push(first);
        while batch.len() < BATCH_MAX {
            match rx.try_recv() {
                Ok(command) => batch.push(command),
                Err(_) => break,
            }
        }
        send_batch(&mut gateway, &zone_id, &batch).await;
        batch.clear();
    }
}

async fn send_batch(
    gateway: &mut GatewayServiceClient<Channel>,
    zone_id: &str,
    batch: &[WorldCommand],
) {
    let request = WorldCommandBatch {
        zone_id: zone_id.to_owned(),
        commands: batch.iter().copied().map(to_wire).collect(),
    };
    for attempt in 1..=SEND_ATTEMPTS {
        match gateway.clone().submit_world_commands(request.clone()).await {
            Ok(resp) if resp.get_ref().ok => {
                metrics::counter!("zone_world_commands_sent_total").increment(batch.len() as u64);
                return;
            }
            Ok(resp) => {
                // The gateway understood and said no — retrying will not help.
                tracing::error!(target: "zone", zone = %zone_id,
                    "world commands refused: {}", resp.get_ref().message);
                break;
            }
            Err(status) if attempt < SEND_ATTEMPTS => {
                tracing::warn!(target: "zone", zone = %zone_id, error = %status,
                    "world command batch failed (attempt {attempt}) — retrying");
                tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
            }
            Err(status) => {
                tracing::error!(target: "zone", zone = %zone_id, error = %status,
                    "world command batch failed after {SEND_ATTEMPTS} attempts");
                break;
            }
        }
    }
    // The ids are burned either way: the script already holds them, and the
    // gateway may or may not have applied part of the batch. Say how many
    // entities the world is missing rather than leaving it to be discovered.
    tracing::error!(target: "zone", zone = %zone_id,
        "{} world command(s) dropped — those entities do not exist", batch.len());
    metrics::counter!("zone_world_commands_dropped_total").increment(batch.len() as u64);
}

fn to_wire(command: WorldCommand) -> WireCommand {
    let command = match command {
        WorldCommand::Spawn {
            network_id,
            entity_type,
            model,
            position,
            heading,
            dynamic,
        } => Command::Spawn(SpawnEntity {
            network_id,
            entity_class: entity_class(entity_type) as i32,
            model,
            x: position[0],
            y: position[1],
            z: position[2],
            heading,
            dynamic,
        }),
        WorldCommand::Despawn { network_id } => Command::Despawn(DespawnEntity { network_id }),
    };
    WireCommand {
        command: Some(command),
    }
}

fn entity_class(entity_type: ScriptEntityType) -> EntityClass {
    match entity_type {
        ScriptEntityType::Ped => EntityClass::Ped,
        ScriptEntityType::Vehicle => EntityClass::Vehicle,
        ScriptEntityType::Object => EntityClass::Object,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_of(blocks: &[(u32, u32)]) -> IdPool {
        let mut pool = IdPool::default();
        for (next, remaining) in blocks {
            pool.push(IdBlock {
                next: *next,
                remaining: *remaining,
            });
        }
        pool
    }

    #[test]
    fn a_block_is_consumed_downward() {
        let mut pool = pool_of(&[(8191, 3)]);
        assert_eq!(pool.take(), Some(8191));
        assert_eq!(pool.take(), Some(8190));
        assert_eq!(pool.take(), Some(8189));
        assert_eq!(pool.take(), None);
        assert_eq!(pool.remaining, 0);
    }

    /// A refill arriving before the current block is empty must not throw the
    /// remainder away — the id space is only 8192 wide.
    #[test]
    fn a_refill_does_not_discard_the_current_block() {
        let mut pool = pool_of(&[(8191, 2), (7000, 2)]);
        assert_eq!(pool.remaining, 4);
        assert_eq!(pool.take(), Some(8191));
        assert_eq!(pool.take(), Some(8190));
        // Exhausted block dropped, the next one takes over.
        assert_eq!(pool.take(), Some(7000));
        assert_eq!(pool.take(), Some(6999));
        assert_eq!(pool.take(), None);
    }

    /// 0 is the invalid handle, so a block must stop at 1 even if its count
    /// says otherwise — and the pool must stop believing in the difference.
    #[test]
    fn a_block_never_hands_out_the_invalid_handle() {
        let mut pool = pool_of(&[(1, 3)]);
        assert_eq!(pool.take(), Some(1));
        assert_eq!(pool.take(), None);
        assert_eq!(pool.remaining, 0, "the unusable remainder is dropped too");
    }

    #[test]
    fn spawn_survives_the_round_trip_through_the_wire_type() {
        let command = WorldCommand::Spawn {
            network_id: 4242,
            entity_type: ScriptEntityType::Vehicle,
            model: 0xDEAD_BEEF,
            position: [1.0, 2.0, 3.0],
            heading: 90.0,
            dynamic: true,
        };
        let Some(Command::Spawn(wire)) = to_wire(command).command else {
            panic!("expected a spawn");
        };
        assert_eq!(wire.network_id, 4242);
        assert_eq!(wire.entity_class, EntityClass::Vehicle as i32);
        assert_eq!(wire.model, 0xDEAD_BEEF);
        assert_eq!((wire.x, wire.y, wire.z), (1.0, 2.0, 3.0));
        assert_eq!(wire.heading, 90.0);
        assert!(wire.dynamic);
    }

    /// The wire value and `GET_ENTITY_TYPE` are deliberately the same numbers.
    #[test]
    fn entity_classes_match_the_native_values() {
        assert_eq!(
            entity_class(ScriptEntityType::Ped) as i32,
            i32::from(ScriptEntityType::Ped.as_native())
        );
        assert_eq!(
            entity_class(ScriptEntityType::Vehicle) as i32,
            i32::from(ScriptEntityType::Vehicle.as_native())
        );
        assert_eq!(
            entity_class(ScriptEntityType::Object) as i32,
            i32::from(ScriptEntityType::Object.as_native())
        );
    }
}
