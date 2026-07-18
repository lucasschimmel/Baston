//! The TCP(TLS) control channel: accept loop, per-client task, and the Mumble
//! handshake state machine (Version → Authenticate → CryptSetup/ChannelState/
//! UserState burst → ServerSync).
//!
//! The client task is generic over the stream so the state machine is testable
//! on `tokio::io::duplex` without TLS.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

use crate::channel::ROOT_CHANNEL;
use crate::framing::{self, MessageType, Preamble, MAX_MSGSIZE, PREAMBLE_SIZE};
use crate::proto;
use crate::session::parse_netid;
use crate::target::{ChannelTarget, Target};
use crate::PROTOCOL_VERSION;

use super::{tls, VoiceState};

/// Accept TCP connections, run the TLS handshake, and hand each client to
/// [`run_client`].
pub(super) async fn accept_loop(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    state: Arc<Mutex<VoiceState>>,
    udp: Arc<UdpSocket>,
) {
    loop {
        let (sock, peer) = match tcp.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(target: "voice", error = %e, "voice TCP accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let state = Arc::clone(&state);
        let udp = Arc::clone(&udp);
        tokio::spawn(async move {
            match acceptor.accept(sock).await {
                Ok(tls_stream) => run_client(tls_stream, state, udp).await,
                Err(e) => {
                    tracing::debug!(target: "voice", %peer, error = %e, "voice TLS handshake failed");
                }
            }
        });
    }
}

/// Per-client connection driver. Generic over the stream for tests.
pub(super) async fn run_client<S>(stream: S, state: Arc<Mutex<VoiceState>>, udp: Arc<UdpSocket>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if wr.write_all(&frame).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    let mut conn = ClientConn {
        session: None,
        out_tx: out_tx.clone(),
    };

    // Read loop: 6-byte preamble + bounded payload.
    let mut preamble = [0u8; PREAMBLE_SIZE];
    loop {
        if rd.read_exact(&mut preamble).await.is_err() {
            break;
        }
        let Some(pre) = Preamble::parse(&preamble) else {
            break;
        };
        if pre.len as usize > MAX_MSGSIZE {
            tracing::debug!(target: "voice", len = pre.len, "oversized voice control frame; closing");
            break;
        }
        let mut payload = vec![0u8; pre.len as usize];
        if rd.read_exact(&mut payload).await.is_err() {
            break;
        }
        let Some(ty) = MessageType::from_u16(pre.ty) else {
            continue; // unknown tag: skip the frame, keep the connection
        };
        if !conn.handle_message(ty, &payload, &state, &udp) {
            break;
        }
    }

    // Disconnect: tear the session down and tell the others.
    if let Some(netid) = conn.session {
        teardown(&state, netid);
    }
    drop(out_tx);
    let _ = writer.await;
}

/// Remove every trace of a session and broadcast `UserRemove`.
fn teardown(state: &Mutex<VoiceState>, netid: u32) {
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
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
    if let Ok(frame) = framing::encode(MessageType::UserRemove, &remove) {
        st.broadcast_control(&frame, None);
    }
    tracing::debug!(target: "voice", netid, "voice client disconnected");
}

/// One client's control-channel state.
struct ClientConn {
    /// `Some(netid)` once authenticated.
    session: Option<u32>,
    out_tx: mpsc::UnboundedSender<Vec<u8>>,
}

impl ClientConn {
    fn send<M: prost::Message>(&self, ty: MessageType, msg: &M) {
        if let Ok(frame) = framing::encode(ty, msg) {
            let _ = self.out_tx.send(frame);
        }
    }

    /// Handle one control message. Returns `false` to close the connection.
    fn handle_message(
        &mut self,
        ty: MessageType,
        payload: &[u8],
        state: &Arc<Mutex<VoiceState>>,
        udp: &UdpSocket,
    ) -> bool {
        match ty {
            MessageType::Version => {
                self.send(
                    MessageType::Version,
                    &proto::Version {
                        version: Some(PROTOCOL_VERSION),
                        release: Some("baston-voice".to_owned()),
                        os: Some(std::env::consts::OS.to_owned()),
                        os_version: None,
                    },
                );
                true
            }
            MessageType::Authenticate => self.handle_authenticate(payload, state),
            MessageType::Ping => {
                let Ok(ping) = framing::decode::<proto::Ping>(ty, payload) else {
                    return true;
                };
                let (good, late, lost) = self
                    .session
                    .and_then(|s| {
                        let st = state.lock().unwrap_or_else(|e| e.into_inner());
                        st.crypt.get(&s).map(|c| (c.good, c.late, c.lost))
                    })
                    .unwrap_or((0, 0, 0));
                self.send(
                    MessageType::Ping,
                    &proto::Ping {
                        timestamp: ping.timestamp,
                        good: Some(good),
                        late: Some(late),
                        lost: Some(lost),
                        resync: Some(0),
                        ..Default::default()
                    },
                );
                true
            }
            MessageType::UserState => {
                if let Ok(us) = framing::decode::<proto::UserState>(ty, payload) {
                    self.handle_user_state(us, state);
                }
                true
            }
            MessageType::VoiceTarget => {
                if let Ok(vt) = framing::decode::<proto::VoiceTarget>(ty, payload) {
                    self.handle_voice_target(vt, state);
                }
                true
            }
            MessageType::UdpTunnel => {
                // Voice over TCP (UDP blocked): the tunneled datagram is
                // plaintext — route it exactly like a decrypted UDP packet.
                let Some(speaker) = self.session else {
                    return true;
                };
                if let Ok(tunnel) = framing::decode::<proto::UdpTunnel>(ty, payload) {
                    super::route_voice(state, udp, speaker, &tunnel.packet);
                }
                true
            }
            MessageType::CryptSetup => {
                let Some(netid) = self.session else {
                    return true;
                };
                let Ok(cs) = framing::decode::<proto::CryptSetup>(ty, payload) else {
                    return true;
                };
                let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                match cs.client_nonce.as_deref() {
                    Some(nonce) if nonce.len() == 16 => {
                        // Client-initiated resync: adopt its nonce.
                        if let Some(crypt) = st.crypt.get_mut(&netid) {
                            crypt.decrypt_iv.copy_from_slice(nonce);
                        }
                    }
                    _ => {
                        // Empty CryptSetup = the client asks for our nonce.
                        if let Some(crypt) = st.crypt.get(&netid) {
                            let reply = proto::CryptSetup {
                                key: None,
                                client_nonce: None,
                                server_nonce: Some(crypt.encrypt_iv.to_vec()),
                            };
                            drop(st);
                            self.send(MessageType::CryptSetup, &reply);
                        }
                    }
                }
                true
            }
            MessageType::PermissionQuery => {
                // Permissive: FiveM's embedded server has no ACLs.
                let Ok(pq) = framing::decode::<proto::PermissionQuery>(ty, payload) else {
                    return true;
                };
                self.send(
                    MessageType::PermissionQuery,
                    &proto::PermissionQuery {
                        channel_id: pq.channel_id,
                        permissions: Some(0x0f07_ffff),
                        flush: Some(false),
                    },
                );
                true
            }
            // Everything else (TextMessage, UserStats, ...) is out of scope
            // for the FiveM surface: ignore and keep the connection.
            _ => true,
        }
    }

    fn handle_authenticate(&mut self, payload: &[u8], state: &Arc<Mutex<VoiceState>>) -> bool {
        let Ok(auth) = framing::decode::<proto::Authenticate>(MessageType::Authenticate, payload)
        else {
            return false;
        };
        let username = auth.username.unwrap_or_default();
        let Some(netid) = parse_netid(&username) else {
            self.send(
                MessageType::Reject,
                &proto::Reject {
                    reason: Some("expected a FiveM \"[netId]\" username".to_owned()),
                    ..Default::default()
                },
            );
            return false;
        };

        // Fresh OCB2 material for this session.
        let mut key = [0u8; 16];
        let mut server_nonce = [0u8; 16];
        let mut client_nonce = [0u8; 16];
        if tls::secure_random(&mut key).is_err()
            || tls::secure_random(&mut server_nonce).is_err()
            || tls::secure_random(&mut client_nonce).is_err()
        {
            return false;
        }

        let (channel_frames, user_frames, self_state_frame) = {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            // A reconnect replaces the previous session (stale socket).
            st.sessions.insert(netid, username.clone());
            st.channels.join(netid, ROOT_CHANNEL);
            st.crypt.insert(
                netid,
                crate::crypto::CryptState::new(&key, &server_nonce, &client_nonce),
            );
            st.control_tx.insert(netid, self.out_tx.clone());

            let channel_frames: Vec<Vec<u8>> = st
                .channels
                .iter()
                .filter_map(|ch| {
                    framing::encode(
                        MessageType::ChannelState,
                        &proto::ChannelState {
                            channel_id: Some(ch.id),
                            parent: (ch.id != ROOT_CHANNEL).then_some(ch.parent),
                            name: Some(ch.name.clone()),
                            temporary: Some(ch.temporary),
                            ..Default::default()
                        },
                    )
                    .ok()
                })
                .collect();

            let mut user_frames = Vec::new();
            for ch in st.channels.iter() {
                for member in ch.members() {
                    if let Some(sess) = st.sessions.get(member) {
                        if let Ok(frame) = framing::encode(
                            MessageType::UserState,
                            &proto::UserState {
                                session: Some(sess.id),
                                name: Some(sess.name.clone()),
                                channel_id: Some(ch.id),
                                mute: Some(sess.mute),
                                deaf: Some(sess.deaf),
                                self_deaf: Some(sess.self_deaf),
                                ..Default::default()
                            },
                        ) {
                            user_frames.push(frame);
                        }
                    }
                }
            }

            // Announce the newcomer to everyone else.
            let self_state = proto::UserState {
                session: Some(netid),
                name: Some(username.clone()),
                channel_id: Some(ROOT_CHANNEL),
                ..Default::default()
            };
            let self_frame = framing::encode(MessageType::UserState, &self_state).ok();
            if let Some(f) = &self_frame {
                st.broadcast_control(f, Some(netid));
            }
            (channel_frames, user_frames, self_frame)
        };

        // Login burst, in umurmur's order.
        self.send(
            MessageType::CryptSetup,
            &proto::CryptSetup {
                key: Some(key.to_vec()),
                client_nonce: Some(client_nonce.to_vec()),
                server_nonce: Some(server_nonce.to_vec()),
            },
        );
        self.send(
            MessageType::CodecVersion,
            &proto::CodecVersion {
                alpha: -2147483637,
                beta: 0,
                prefer_alpha: true,
                opus: Some(true),
            },
        );
        for frame in channel_frames {
            let _ = self.out_tx.send(frame);
        }
        for frame in user_frames {
            let _ = self.out_tx.send(frame);
        }
        let _ = self_state_frame; // already broadcast; own state was in user_frames
        self.send(
            MessageType::ServerSync,
            &proto::ServerSync {
                session: Some(netid),
                max_bandwidth: Some(128_000),
                welcome_text: Some("BASTON voice".to_owned()),
                permissions: Some(0x0f07_ffff),
            },
        );

        self.session = Some(netid);
        tracing::debug!(target: "voice", netid, "voice client authenticated");
        true
    }

    fn handle_user_state(&mut self, us: proto::UserState, state: &Arc<Mutex<VoiceState>>) {
        let Some(netid) = self.session else {
            return;
        };
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());

        let mut echo = proto::UserState {
            session: Some(netid),
            ..Default::default()
        };
        let mut channel_announce: Option<Vec<u8>> = None;

        if let Some(sess) = st.sessions.get_mut(netid) {
            if let Some(v) = us.self_mute {
                // Mumble couples self-mute into `mute` client-side; we track
                // the server-forced flag separately and only echo self state.
                echo.self_mute = Some(v);
            }
            if let Some(v) = us.self_deaf {
                sess.self_deaf = v;
                echo.self_deaf = Some(v);
            }
            if let Some(ctx) = us.plugin_context {
                sess.context = Some(ctx);
            }
        }

        if let Some(channel_id) = us.channel_id {
            if !st.channels.exists(channel_id) {
                // pma-voice joins channels it never explicitly created.
                let ch = st.channels.create_game_channel(channel_id);
                channel_announce = framing::encode(
                    MessageType::ChannelState,
                    &proto::ChannelState {
                        channel_id: Some(ch.id),
                        parent: Some(ch.parent),
                        name: Some(ch.name.clone()),
                        temporary: Some(false),
                        ..Default::default()
                    },
                )
                .ok();
            }
            st.channels.join(netid, channel_id);
            echo.channel_id = Some(channel_id);
        }

        for ch in us.listening_channel_add {
            st.channels.set_listening(netid, ch, true);
        }
        for ch in us.listening_channel_remove {
            st.channels.set_listening(netid, ch, false);
        }

        if let Some(frame) = channel_announce {
            st.broadcast_control(&frame, None);
        }
        if let Ok(frame) = framing::encode(MessageType::UserState, &echo) {
            st.broadcast_control(&frame, None);
        }
    }

    fn handle_voice_target(&mut self, vt: proto::VoiceTarget, state: &Arc<Mutex<VoiceState>>) {
        let Some(netid) = self.session else {
            return;
        };
        let Some(id) = vt.id else {
            return;
        };
        let Ok(id8) = u8::try_from(id) else {
            return;
        };
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sess) = st.sessions.get_mut(netid) else {
            return;
        };
        if vt.targets.is_empty() {
            sess.targets.clear(id8);
            return;
        }
        let mut target = Target::default();
        for t in vt.targets {
            target.sessions.extend(t.session.iter().copied());
            if let Some(channel_id) = t.channel_id {
                target.channels.push(ChannelTarget {
                    channel_id,
                    links: t.links.unwrap_or(false),
                    children: t.children.unwrap_or(false),
                });
            }
        }
        sess.targets.set(id8, target);
    }
}
