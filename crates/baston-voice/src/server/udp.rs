//! The UDP voice socket: ping probe, OCB2 demux/decrypt, and routing.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;

use crate::{VoicePacketType, PROTOCOL_VERSION};

use super::VoiceState;

/// Maximum voice datagram we accept (mirrors Mumble's UDP buffer).
const MAX_DATAGRAM: usize = 1024;

pub(super) async fn recv_loop(udp: Arc<UdpSocket>, state: Arc<Mutex<VoiceState>>) {
    let mut buf = [0u8; MAX_DATAGRAM];
    loop {
        let (len, peer) = match udp.recv_from(&mut buf).await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!(target: "voice", error = %e, "voice UDP recv failed");
                continue;
            }
        };
        let datagram = &buf[..len];

        // Plaintext server-list ping probe: 4 zero bytes + 8-byte ident.
        if len == 12 && datagram[..4] == [0, 0, 0, 0] {
            let reply = ping_probe_reply(&state, &datagram[4..12]);
            let _ = udp.try_send_to(&reply, peer);
            continue;
        }

        // Encrypted voice/ping datagram.
        let Some((speaker, plain)) = decrypt_from(&state, peer, datagram) else {
            continue;
        };
        if plain
            .first()
            .and_then(|b| VoicePacketType::from_bits(b >> 5))
            == Some(VoicePacketType::Ping)
        {
            // Encrypted UDP ping: echo it back (keeps the client on UDP).
            let echo = {
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                st.crypt.get_mut(&speaker).map(|c| c.encrypt(&plain))
            };
            if let Some(echo) = echo {
                let _ = udp.try_send_to(&echo, peer);
            }
            continue;
        }
        super::route_voice(&state, &udp, speaker, &plain);
    }
}

/// Identify the sender and decrypt. Known addresses use their session's crypt
/// state directly; an unknown address is matched by trying every session
/// (standard murmur behaviour — this binds the client's UDP flow after NAT).
fn decrypt_from(
    state: &Mutex<VoiceState>,
    peer: SocketAddr,
    datagram: &[u8],
) -> Option<(u32, Vec<u8>)> {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&session) = st.addr_index.get(&peer) {
        let plain = st.crypt.get_mut(&session)?.decrypt(datagram)?;
        return Some((session, plain));
    }
    // Unknown source address: bounded try-all over connected sessions.
    let candidates: Vec<u32> = st.crypt.keys().copied().collect();
    for session in candidates {
        if let Some(plain) = st.crypt.get_mut(&session).and_then(|c| c.decrypt(datagram)) {
            if let Some(old) = st.udp_addr.insert(session, peer) {
                st.addr_index.remove(&old);
            }
            st.addr_index.insert(peer, session);
            tracing::debug!(target: "voice", session, %peer, "voice UDP flow bound");
            return Some((session, plain));
        }
    }
    None
}

/// Server-list probe reply: version, echoed ident, user count, max users,
/// allowed bandwidth (all big-endian, murmur wire format).
fn ping_probe_reply(state: &Mutex<VoiceState>, ident: &[u8]) -> Vec<u8> {
    let users = {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.sessions.len() as u32
    };
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    out.extend_from_slice(ident);
    out.extend_from_slice(&users.to_be_bytes());
    out.extend_from_slice(&128u32.to_be_bytes());
    out.extend_from_slice(&128_000u32.to_be_bytes());
    out
}
