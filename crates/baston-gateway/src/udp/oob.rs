//! Out-of-band (OOB) datagram handling on the game socket.
//!
//! Before the ENet connection, the FiveM client probes the server with raw
//! Quake-style datagrams: `0xFFFFFFFF` + `getinfo <challenge>`
//! (`NetLibrary::SendOutOfBand`, state CS_FETCHING). The server must answer
//! `0xFFFFFFFF` + `infoResponse\n\key\value...` from the same socket
//! (`outofbandhandlers/GetInfoOutOfBand.h`) or the client aborts with
//! "Failed to get info from server".
//!
//! [`OobSocket`] wraps the UDP socket given to `rusty_enet`, answering OOB
//! datagrams inline and hiding them from the ENet protocol state machine.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;

use baston_protocol::PlayerDirectory;
use rusty_enet as enet;

const OOB_MARKER: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

/// Static server descriptors used in the `infoResponse`.
pub struct OobInfo {
    pub hostname: String,
    pub max_clients: u32,
    pub players: Arc<PlayerDirectory>,
}

pub struct OobSocket {
    inner: UdpSocket,
    info: OobInfo,
}

impl OobSocket {
    pub fn new(inner: UdpSocket, info: OobInfo) -> Self {
        Self { inner, info }
    }

    fn handle_oob(&mut self, from: SocketAddr, data: &[u8]) {
        let Ok(text) = std::str::from_utf8(data) else {
            return;
        };
        if let Some(rest) = text.strip_prefix("getinfo") {
            // GetInfoOutOfBand.h: challenge = data up to first space/newline.
            let challenge = rest.trim_start().split([' ', '\n']).next().unwrap_or("");
            let response = format!(
                "infoResponse\n\\sv_maxclients\\{}\\clients\\{}\\challenge\\{}\\gamename\\CitizenFX\\protocol\\4\\hostname\\{}\\gametype\\Roleplay\\mapname\\Los Santos\\iv\\0",
                self.info.max_clients,
                self.info.players.count(),
                challenge,
                self.info.hostname,
            );
            self.send_oob(from, response.as_bytes());
            tracing::debug!(target: "udp", %from, "answered OOB getinfo");
        } else {
            tracing::debug!(target: "udp", %from, oob = %text.chars().take(32).collect::<String>(), "unhandled OOB");
        }
    }

    fn send_oob(&mut self, to: SocketAddr, payload: &[u8]) {
        let mut out = OOB_MARKER.to_vec();
        out.extend_from_slice(payload);
        if let Err(e) = self.inner.send_to(&out, to) {
            tracing::warn!(target: "udp", %to, error = %e, "OOB send failed");
        }
    }
}

impl enet::Socket for OobSocket {
    type Address = SocketAddr;
    type Error = std::io::Error;

    fn init(&mut self, socket_options: enet::SocketOptions) -> Result<(), Self::Error> {
        enet::Socket::init(&mut self.inner, socket_options)
    }

    fn send(&mut self, address: SocketAddr, buffer: &[u8]) -> Result<usize, Self::Error> {
        enet::Socket::send(&mut self.inner, address, buffer)
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; enet::MTU_MAX],
    ) -> Result<Option<(SocketAddr, enet::PacketReceived)>, Self::Error> {
        loop {
            match enet::Socket::receive(&mut self.inner, buffer)? {
                Some((addr, enet::PacketReceived::Complete(len)))
                    if len >= 4 && buffer[..4] == OOB_MARKER =>
                {
                    self.handle_oob(addr, &buffer[4..len]);
                    // OOB consumed — keep draining for a real ENet packet.
                }
                other => return Ok(other),
            }
        }
    }
}
