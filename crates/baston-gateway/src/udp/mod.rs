//! Game-transport server: ENet over UDP (jalon B3).
//!
//! FiveM's game channel is standard ENet 1.3 with 2 channels
//! (`GameServerNet.ENet.cpp`). The ENet reliability layer (sequencing, ACKs,
//! retransmission, fragmentation, keep-alive) is provided by `rusty_enet`;
//! BASTON implements the FiveM message layer on top
//! (`GameServer::ProcessPacket`): `u32 LE` message type + payload.
//!
//! The ENet host is `!Sync` state-machine style, so it lives in one tokio
//! task; outbound traffic goes through an mpsc command channel.
//!
//! Module layout: [`handle`] holds the public handle + command channel,
//! [`server`] the ENet host task and outbound path, [`inbound`] the inbound
//! message dispatch, and [`oob`] the out-of-band query socket.

mod handle;
mod inbound;
mod oob;
mod server;

pub use handle::{ControlPlaneHandle, SyncPlaneHandle, UdpCommand, UdpError, UdpHandle};
pub use oob::{OobInfo, OobSocket};
pub use server::{spawn, spawn_with_mesh, spawn_with_mesh_on, spawn_with_net};
