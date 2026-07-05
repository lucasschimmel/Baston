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

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use baston_protocol::PlayerDirectory;
use rusty_enet as enet;

const OOB_MARKER: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

// `getinfo` is answerable by any unauthenticated source and the reply is
// larger than the request, so it's a textbook UDP reflection/amplification
// vector. A per-source-IP token bucket caps how often we amplify, and the
// echoed challenge is truncated so an attacker can't inflate the reply size.
const OOB_RATE_PER_SEC: f32 = 5.0;
const OOB_BURST: f32 = 5.0;
const OOB_MAX_TRACKED_IPS: usize = 8192;
const OOB_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const OOB_MAX_CHALLENGE_LEN: usize = 64;

struct Bucket {
    tokens: f32,
    last: Instant,
}

/// Per-IP token bucket with a bounded table so spoofed source IPs can't grow
/// memory without limit.
struct OobRateLimiter {
    buckets: HashMap<IpAddr, Bucket>,
    last_sweep: Instant,
}

impl OobRateLimiter {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }

    /// Consume one token for `ip`; returns `true` if a reply is allowed.
    fn allow(&mut self, ip: IpAddr) -> bool {
        let now = Instant::now();
        self.maybe_sweep(now);
        // Table full and this is a new IP: refuse rather than grow unbounded.
        if self.buckets.len() >= OOB_MAX_TRACKED_IPS && !self.buckets.contains_key(&ip) {
            return false;
        }
        let bucket = self.buckets.entry(ip).or_insert(Bucket {
            tokens: OOB_BURST,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f32();
        bucket.tokens = (bucket.tokens + elapsed * OOB_RATE_PER_SEC).min(OOB_BURST);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Periodically drop fully-refilled (idle) buckets to reclaim memory.
    fn maybe_sweep(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_sweep) < OOB_SWEEP_INTERVAL {
            return;
        }
        self.last_sweep = now;
        self.buckets.retain(|_, b| {
            let elapsed = now.saturating_duration_since(b.last).as_secs_f32();
            (b.tokens + elapsed * OOB_RATE_PER_SEC) < OOB_BURST
        });
    }
}

/// Static server descriptors used in the `infoResponse`.
pub struct OobInfo {
    pub hostname: String,
    pub max_clients: u32,
    pub players: Arc<PlayerDirectory>,
}

pub struct OobSocket {
    inner: UdpSocket,
    info: OobInfo,
    rate: OobRateLimiter,
}

impl OobSocket {
    pub fn new(inner: UdpSocket, info: OobInfo) -> Self {
        Self {
            inner,
            info,
            rate: OobRateLimiter::new(),
        }
    }

    fn handle_oob(&mut self, from: SocketAddr, data: &[u8]) {
        let Ok(text) = std::str::from_utf8(data) else {
            return;
        };
        if let Some(rest) = text.strip_prefix("getinfo") {
            if !self.rate.allow(from.ip()) {
                tracing::debug!(target: "udp", %from, "OOB getinfo rate-limited");
                return;
            }
            // GetInfoOutOfBand.h: challenge = data up to first space/newline,
            // capped so the echo can't be used to inflate the reply size.
            let challenge: String = rest
                .trim_start()
                .split([' ', '\n'])
                .next()
                .unwrap_or("")
                .chars()
                .take(OOB_MAX_CHALLENGE_LEN)
                .collect();
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
