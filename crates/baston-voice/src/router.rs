//! The routing brain — resolve one inbound voice packet into a recipient list.
//!
//! This is where BASTON does better than umurmur's per-packet linear scan: the
//! routing decision is expressed as pure data (`Recipient`s) computed from the
//! session/channel/target state, so the hot path can precompute and cache it
//! (V2) and layer server-authoritative culling on top (V3) without touching the
//! wire format.
//!
//! Target semantics (umurmur `Client_voiceMsg`):
//! - `Normal`   → everyone in the speaker's channel (+ its listeners), minus
//!   the speaker; positional block stripped when contexts differ.
//! - `Whisper(n)` → the union of the target's explicit sessions and the members
//!   (+ listeners) of the target's channels.
//! - `Loopback` → the speaker only (mic test).

use crate::channel::ChannelStore;
use crate::session::SessionRegistry;
use crate::voice::VoicePacket;
use crate::VoiceTargetKind;

use std::collections::BTreeSet;

/// One resolved recipient and whether it should receive positional data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipient {
    pub session: u32,
    /// True → forward the full packet (with position); false → strip position.
    pub with_position: bool,
}

/// Optional area-of-interest oracle (V3). Given the speaker and a candidate
/// listener, decides whether the listener is close enough to hear. When absent
/// (V1/V2), everyone in scope hears — matching stock pma-voice behaviour.
pub trait AoiOracle {
    /// Returns `Some(true/false)` to force include/exclude by proximity, or
    /// `None` to defer to default routing (no position data available yet).
    fn audible(&self, speaker: u32, listener: u32) -> Option<bool>;
}

/// A no-op oracle: proximity culling disabled (V1/V2 default).
pub struct NoCulling;
impl AoiOracle for NoCulling {
    fn audible(&self, _speaker: u32, _listener: u32) -> Option<bool> {
        None
    }
}

/// Compute the recipients for `packet` sent by `speaker`.
///
/// `aoi` is consulted only for `Normal` (proximity) talk — whisper/loopback are
/// explicit and bypass culling, as pma-voice expects.
pub fn resolve(
    speaker: u32,
    packet: &VoicePacket,
    sessions: &SessionRegistry,
    channels: &ChannelStore,
    aoi: &dyn AoiOracle,
) -> Vec<Recipient> {
    // A muted (server-forced) speaker is silent.
    match sessions.get(speaker) {
        Some(s) if s.can_speak() => {}
        _ => return Vec::new(),
    }

    match packet.target {
        VoiceTargetKind::Loopback => vec![Recipient {
            session: speaker,
            with_position: false,
        }],
        VoiceTargetKind::Normal => {
            let mut out = Vec::new();
            let Some(channel_id) = channels
                .iter()
                .find(|c| c.members().any(|m| m == speaker))
                .map(|c| c.id)
            else {
                return out;
            };
            if let Some(ch) = channels.get(channel_id) {
                let candidates = ch.members().chain(ch.listeners());
                for listener in candidates {
                    if listener == speaker {
                        continue;
                    }
                    if !can_deliver(speaker, listener, sessions, aoi, true) {
                        continue;
                    }
                    out.push(recipient(speaker, listener, packet, sessions));
                }
            }
            dedup(out)
        }
        VoiceTargetKind::Whisper(id) => {
            let mut set = BTreeSet::new();
            if let Some(target) = sessions.get(speaker).and_then(|s| s.targets.get(id)) {
                for &sess in &target.sessions {
                    set.insert(sess);
                }
                for ct in &target.channels {
                    if let Some(ch) = channels.get(ct.channel_id) {
                        for m in ch.members().chain(ch.listeners()) {
                            set.insert(m);
                        }
                    }
                }
            }
            let mut out = Vec::new();
            for listener in set {
                if listener == speaker {
                    continue;
                }
                // Whisper bypasses proximity culling (explicit intent).
                if !can_deliver(speaker, listener, sessions, aoi, false) {
                    continue;
                }
                out.push(recipient(speaker, listener, packet, sessions));
            }
            out
        }
    }
}

/// Can we deliver from `speaker` to `listener`? Gates on the listener's
/// deaf/self-deaf flags and (for proximity only) the AoI oracle.
fn can_deliver(
    speaker: u32,
    listener: u32,
    sessions: &SessionRegistry,
    aoi: &dyn AoiOracle,
    apply_aoi: bool,
) -> bool {
    match sessions.get(listener) {
        Some(s) if s.can_hear() => {}
        _ => return false,
    }
    if apply_aoi {
        if let Some(audible) = aoi.audible(speaker, listener) {
            return audible;
        }
    }
    true
}

/// Build a recipient with the positional decision: position is forwarded only
/// when the packet carries one AND the two sessions share a context.
fn recipient(
    speaker: u32,
    listener: u32,
    packet: &VoicePacket,
    sessions: &SessionRegistry,
) -> Recipient {
    let with_position = packet.has_position() && sessions.contexts_match(speaker, listener);
    Recipient {
        session: listener,
        with_position,
    }
}

fn dedup(mut v: Vec<Recipient>) -> Vec<Recipient> {
    v.sort_by_key(|r| r.session);
    v.dedup_by_key(|r| r.session);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pds::write_u64;
    use crate::{VoicePacketType, VoiceTargetKind};

    fn packet(target: VoiceTargetKind, pos: Option<[f32; 3]>) -> VoicePacket {
        let mut p = Vec::new();
        p.push((VoicePacketType::Opus as u8) << 5 | target.to_bits());
        write_u64(&mut p, 1);
        write_u64(&mut p, 3);
        p.extend_from_slice(&[1, 2, 3]);
        if let Some(c) = pos {
            for v in c {
                p.extend_from_slice(&v.to_be_bytes());
            }
        }
        crate::voice::parse(&p).unwrap()
    }

    fn setup() -> (SessionRegistry, ChannelStore) {
        let mut s = SessionRegistry::default();
        for id in 1..=3 {
            s.insert(id, format!("[{id}]"));
        }
        let mut c = ChannelStore::new();
        c.join(1, crate::channel::ROOT_CHANNEL);
        c.join(2, crate::channel::ROOT_CHANNEL);
        c.join(3, crate::channel::ROOT_CHANNEL);
        (s, c)
    }

    #[test]
    fn normal_talk_reaches_channel_peers_not_self() {
        let (s, c) = setup();
        let pkt = packet(VoiceTargetKind::Normal, None);
        let r = resolve(1, &pkt, &s, &c, &NoCulling);
        let ids: Vec<u32> = r.iter().map(|x| x.session).collect();
        assert_eq!(ids, vec![2, 3]);
        assert!(r.iter().all(|x| !x.with_position));
    }

    #[test]
    fn position_forwarded_only_on_context_match() {
        let (mut s, c) = setup();
        s.get_mut(1).unwrap().context = Some(b"ls".to_vec());
        s.get_mut(2).unwrap().context = Some(b"ls".to_vec()); // matches 1
                                                              // session 3 has no context → position stripped
        let pkt = packet(VoiceTargetKind::Normal, Some([1.0, 2.0, 3.0]));
        let r = resolve(1, &pkt, &s, &c, &NoCulling);
        let p2 = r.iter().find(|x| x.session == 2).unwrap();
        let p3 = r.iter().find(|x| x.session == 3).unwrap();
        assert!(p2.with_position, "matching context keeps position");
        assert!(!p3.with_position, "no context strips position");
    }

    #[test]
    fn loopback_echoes_to_speaker_only() {
        let (s, c) = setup();
        let pkt = packet(VoiceTargetKind::Loopback, None);
        let r = resolve(1, &pkt, &s, &c, &NoCulling);
        assert_eq!(
            r,
            vec![Recipient {
                session: 1,
                with_position: false
            }]
        );
    }

    #[test]
    fn deaf_listener_is_skipped() {
        let (mut s, c) = setup();
        s.get_mut(2).unwrap().self_deaf = true;
        let pkt = packet(VoiceTargetKind::Normal, None);
        let r = resolve(1, &pkt, &s, &c, &NoCulling);
        assert_eq!(r.iter().map(|x| x.session).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn muted_speaker_is_silent() {
        let (mut s, c) = setup();
        s.get_mut(1).unwrap().mute = true;
        let pkt = packet(VoiceTargetKind::Normal, None);
        assert!(resolve(1, &pkt, &s, &c, &NoCulling).is_empty());
    }

    #[test]
    fn whisper_targets_explicit_sessions_and_channels() {
        let (mut s, mut c) = setup();
        c.create_game_channel(50);
        c.join(3, 50); // 3 moves to channel 50
                       // target 5: explicit session 2 + channel 50 (contains 3)
        s.get_mut(1).unwrap().targets.set(
            5,
            crate::target::Target {
                sessions: vec![2],
                channels: vec![crate::target::ChannelTarget {
                    channel_id: 50,
                    links: false,
                    children: false,
                }],
            },
        );
        let pkt = packet(VoiceTargetKind::Whisper(5), None);
        let r = resolve(1, &pkt, &s, &c, &NoCulling);
        let ids: Vec<u32> = r.iter().map(|x| x.session).collect();
        assert_eq!(ids, vec![2, 3]);
    }

    #[test]
    fn aoi_oracle_culls_normal_talk() {
        struct OnlyTwo;
        impl AoiOracle for OnlyTwo {
            fn audible(&self, _s: u32, listener: u32) -> Option<bool> {
                Some(listener == 2)
            }
        }
        let (s, c) = setup();
        let pkt = packet(VoiceTargetKind::Normal, None);
        let r = resolve(1, &pkt, &s, &c, &OnlyTwo);
        assert_eq!(r.iter().map(|x| x.session).collect::<Vec<_>>(), vec![2]);
    }
}
