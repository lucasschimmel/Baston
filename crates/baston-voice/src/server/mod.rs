//! The voice server runtime: TLS control channel + UDP voice transport.
//!
//! Owns the sockets and tasks; the protocol *logic* (framing, crypto, session/
//! channel/target state, routing) lives in the sibling sans-IO modules. The
//! gateway spawns this once and keeps a [`VoiceHandle`] for player lifecycle
//! and the `MUMBLE_*` script natives.

mod control;
mod tls;
mod udp;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::channel::ChannelStore;
use crate::crypto::CryptState;
use crate::framing::{self, MessageType};
use crate::proto;
use crate::router::{self, NoCulling};
use crate::session::SessionRegistry;
use crate::voice;

/// Voice server bind configuration.
#[derive(Debug, Clone)]
pub struct VoiceServerConfig {
    pub bind: IpAddr,
    /// TCP (TLS control) + UDP (voice) port. `0` binds an ephemeral port
    /// (tests); the actual port is available via [`VoiceHandle::port`].
    pub port: u16,
}

/// All mutable voice state, behind one mutex. Critical sections are short and
/// never await; outbound control frames go through per-client unbounded
/// channels so nothing blocks under the lock.
pub struct VoiceState {
    pub sessions: SessionRegistry,
    pub channels: ChannelStore,
    /// Per-session TCP writer (pre-framed bytes).
    control_tx: HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>,
    /// Per-session OCB2 crypt state (created at Authenticate).
    crypt: HashMap<u32, CryptState>,
    /// Last seen UDP address per session, and the reverse index for demux.
    udp_addr: HashMap<u32, SocketAddr>,
    addr_index: HashMap<SocketAddr, u32>,
    /// `NETWORK_SET_VOICE_PROXIMITY_OVERRIDE` per player.
    proximity_overrides: HashMap<u32, [f32; 3]>,
}

impl VoiceState {
    fn new() -> Self {
        Self {
            sessions: SessionRegistry::default(),
            channels: ChannelStore::new(),
            control_tx: HashMap::new(),
            crypt: HashMap::new(),
            udp_addr: HashMap::new(),
            addr_index: HashMap::new(),
            proximity_overrides: HashMap::new(),
        }
    }

    /// Queue a pre-framed control message to one session (best-effort).
    fn send_control(&self, session: u32, frame: Vec<u8>) {
        if let Some(tx) = self.control_tx.get(&session) {
            let _ = tx.send(frame);
        }
    }

    /// Queue a control message to every established session except `skip`.
    fn broadcast_control(&self, frame: &[u8], skip: Option<u32>) {
        for (&session, tx) in &self.control_tx {
            if Some(session) == skip {
                continue;
            }
            let _ = tx.send(frame.to_vec());
        }
    }
}

/// Cloneable handle to the running voice server: player lifecycle hooks and
/// the state surface the `MUMBLE_*` natives need.
#[derive(Clone)]
pub struct VoiceHandle {
    state: Arc<Mutex<VoiceState>>,
    port: u16,
}

impl std::fmt::Debug for VoiceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceHandle")
            .field("port", &self.port)
            .finish()
    }
}

impl VoiceHandle {
    /// The actual bound port (differs from the config only when it was 0).
    pub fn port(&self) -> u16 {
        self.port
    }

    fn lock(&self) -> MutexGuard<'_, VoiceState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Player left the game server: tear the voice session down and tell the
    /// other clients. Safe to call for players that never connected to voice.
    pub fn on_player_dropped(&self, netid: u32) {
        let frame = {
            let mut st = self.lock();
            if st.sessions.remove(netid).is_none() {
                return;
            }
            st.channels.drop_session(netid);
            st.crypt.remove(&netid);
            st.control_tx.remove(&netid);
            st.proximity_overrides.remove(&netid);
            if let Some(addr) = st.udp_addr.remove(&netid) {
                st.addr_index.remove(&addr);
            }
            let remove = proto::UserRemove {
                session: netid,
                actor: None,
                reason: None,
                ban: None,
            };
            let frame = framing::encode(MessageType::UserRemove, &remove).ok();
            if let Some(f) = &frame {
                st.broadcast_control(f, None);
            }
            frame
        };
        let _ = frame;
        tracing::debug!(target: "voice", netid, "voice session dropped");
    }

    /// `MUMBLE_CREATE_CHANNEL` — create a permanent game channel and announce
    /// it to connected clients.
    pub fn create_channel(&self, id: u32) {
        let mut st = self.lock();
        if st.channels.exists(id) {
            return;
        }
        let ch = st.channels.create_game_channel(id);
        let msg = proto::ChannelState {
            channel_id: Some(ch.id),
            parent: Some(ch.parent),
            name: Some(ch.name.clone()),
            temporary: Some(false),
            ..Default::default()
        };
        if let Ok(frame) = framing::encode(MessageType::ChannelState, &msg) {
            st.broadcast_control(&frame, None);
        }
    }

    /// `MUMBLE_DOES_CHANNEL_EXIST`.
    pub fn channel_exists(&self, id: u32) -> bool {
        self.lock().channels.exists(id)
    }

    /// `MUMBLE_SET_PLAYER_MUTED` — server-forced mute; broadcast the change.
    pub fn set_player_muted(&self, netid: u32, muted: bool) {
        let mut st = self.lock();
        let Some(session) = st.sessions.get_mut(netid) else {
            return;
        };
        session.mute = muted;
        let msg = proto::UserState {
            session: Some(netid),
            mute: Some(muted),
            ..Default::default()
        };
        if let Ok(frame) = framing::encode(MessageType::UserState, &msg) {
            st.broadcast_control(&frame, None);
        }
    }

    /// `MUMBLE_IS_PLAYER_MUTED`.
    pub fn is_player_muted(&self, netid: u32) -> bool {
        self.lock().sessions.get(netid).is_some_and(|s| s.mute)
    }

    /// `NETWORK_SET_VOICE_PROXIMITY_OVERRIDE_FOR_PLAYER`.
    pub fn set_proximity_override(&self, netid: u32, position: Option<[f32; 3]>) {
        let mut st = self.lock();
        match position {
            Some(p) => {
                st.proximity_overrides.insert(netid, p);
            }
            None => {
                st.proximity_overrides.remove(&netid);
            }
        }
    }

    /// `NETWORK_GET_VOICE_PROXIMITY_OVERRIDE_FOR_PLAYER` (zero when unset).
    pub fn proximity_override(&self, netid: u32) -> [f32; 3] {
        self.lock()
            .proximity_overrides
            .get(&netid)
            .copied()
            .unwrap_or([0.0; 3])
    }
}

/// Spawn the voice server: TLS acceptor + TCP accept loop + UDP receive loop.
/// The returned handle is the integration surface; the tasks run for the
/// process lifetime.
pub async fn spawn(cfg: VoiceServerConfig) -> std::io::Result<VoiceHandle> {
    let state = Arc::new(Mutex::new(VoiceState::new()));

    let tcp = tokio::net::TcpListener::bind((cfg.bind, cfg.port)).await?;
    let port = tcp.local_addr()?.port();
    // UDP shares the port *number* with TCP (separate socket, standard Mumble).
    let udp = Arc::new(UdpSocket::bind((cfg.bind, port)).await?);

    let acceptor = tls::make_acceptor()
        .map_err(|e| std::io::Error::other(format!("voice TLS setup failed: {e}")))?;

    tokio::spawn(control::accept_loop(
        tcp,
        acceptor,
        Arc::clone(&state),
        Arc::clone(&udp),
    ));
    tokio::spawn(udp::recv_loop(Arc::clone(&udp), Arc::clone(&state)));

    tracing::info!(target: "voice", port, "voice server listening (TCP+TLS control, UDP voice)");
    Ok(VoiceHandle { state, port })
}

/// Route one plaintext voice datagram from `speaker` to its recipients.
/// Shared by the UDP path and the TCP `UDPTunnel` fallback. Recipients with a
/// known UDP address get an encrypted datagram; the rest get a `UDPTunnel`
/// control frame.
fn route_voice(state: &Mutex<VoiceState>, udp: &UdpSocket, speaker: u32, datagram: &[u8]) {
    let Some(packet) = voice::parse(datagram) else {
        return;
    };
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
    let recipients = router::resolve(speaker, &packet, &st.sessions, &st.channels, &NoCulling);
    for r in recipients {
        let out = voice::encode_outbound(u64::from(speaker), &packet, r.with_position);
        let addr = st.udp_addr.get(&r.session).copied();
        match (addr, st.crypt.get_mut(&r.session)) {
            (Some(addr), Some(crypt)) => {
                let encrypted = crypt.encrypt(&out);
                // Non-blocking send; a full socket buffer just drops the frame
                // (voice is loss-tolerant by design).
                let _ = udp.try_send_to(&encrypted, addr);
            }
            _ => {
                let tunnel = proto::UdpTunnel { packet: out };
                if let Ok(frame) = framing::encode(MessageType::UdpTunnel, &tunnel) {
                    st.send_control(r.session, frame);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_binds_ephemeral_port_and_handle_answers_natives() {
        let handle = spawn(VoiceServerConfig {
            bind: "127.0.0.1".parse().unwrap(),
            port: 0,
        })
        .await
        .expect("spawn");
        assert_ne!(handle.port(), 0);

        // Channel natives work without any connected client.
        assert!(!handle.channel_exists(42));
        handle.create_channel(42);
        assert!(handle.channel_exists(42));

        // Mute of an unknown player is a no-op; query returns false.
        handle.set_player_muted(7, true);
        assert!(!handle.is_player_muted(7));

        // Proximity override round-trip.
        assert_eq!(handle.proximity_override(7), [0.0; 3]);
        handle.set_proximity_override(7, Some([1.0, 2.0, 3.0]));
        assert_eq!(handle.proximity_override(7), [1.0, 2.0, 3.0]);
        handle.set_proximity_override(7, None);
        assert_eq!(handle.proximity_override(7), [0.0; 3]);
    }
}
