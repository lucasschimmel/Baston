//! Voice sessions — the per-connected-client state and the session⇄netId map.
//!
//! BASTON assigns the Mumble **session id = the player's netId (source)**. The
//! stock client authenticates with username `"[netId]"` and FiveM's server
//! natives address players by server id (`MumbleSetVolumeOverrideByServerId`,
//! pma-voice), so making session == netId keeps the whole surface trivial and
//! avoids a second id space.
//!
//! This module owns only the *logical* session state (flags, channel, target
//! registry, positional context). The TLS transport and crypto live elsewhere.

use std::collections::HashMap;

use crate::target::TargetRegistry;

/// Parse a Mumble username of the form `"[netId]"` (optionally `"[netId] name"`)
/// into the netId. Returns `None` if it doesn't match — a non-FiveM client.
pub fn parse_netid(username: &str) -> Option<u32> {
    let inner = username.strip_prefix('[')?;
    let end = inner.find(']')?;
    inner[..end].parse().ok()
}

/// One connected voice client.
#[derive(Debug)]
pub struct Session {
    /// Session id == player netId (source).
    pub id: u32,
    pub name: String,
    /// Admin mute (server-forced) — packets from this session are dropped.
    pub mute: bool,
    /// Self-deaf — this session receives no audio.
    pub self_deaf: bool,
    /// Admin deaf.
    pub deaf: bool,
    /// Positional-audio context. Two sessions exchange position only when their
    /// contexts match (umurmur `strcmp(src->context, dst->context)`).
    pub context: Option<Vec<u8>>,
    /// This client's whisper targets.
    pub targets: TargetRegistry,
}

impl Session {
    fn new(id: u32, name: String) -> Self {
        Self {
            id,
            name,
            mute: false,
            self_deaf: false,
            deaf: false,
            context: None,
            targets: TargetRegistry::default(),
        }
    }

    /// Whether this session may currently transmit voice.
    pub fn can_speak(&self) -> bool {
        !self.mute
    }

    /// Whether this session should receive audio.
    pub fn can_hear(&self) -> bool {
        !self.deaf && !self.self_deaf
    }
}

/// Registry of all connected voice sessions, keyed by id (== netId).
#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: HashMap<u32, Session>,
}

impl SessionRegistry {
    /// Register a newly authenticated client. Session id == netId.
    pub fn insert(&mut self, netid: u32, name: String) -> &mut Session {
        self.sessions
            .entry(netid)
            .or_insert_with(|| Session::new(netid, name))
    }

    pub fn get(&self, id: u32) -> Option<&Session> {
        self.sessions.get(&id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut Session> {
        self.sessions.get_mut(&id)
    }

    /// Remove a session on disconnect.
    pub fn remove(&mut self, id: u32) -> Option<Session> {
        self.sessions.remove(&id)
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Do two sessions share a positional context (position may be exchanged)?
    pub fn contexts_match(&self, a: u32, b: u32) -> bool {
        match (self.get(a), self.get(b)) {
            (Some(sa), Some(sb)) => match (&sa.context, &sb.context) {
                (Some(ca), Some(cb)) => ca == cb,
                _ => false,
            },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fivem_usernames() {
        assert_eq!(parse_netid("[7]"), Some(7));
        assert_eq!(parse_netid("[42] Lucas"), Some(42));
        assert_eq!(parse_netid("nobody"), None);
        assert_eq!(parse_netid("[abc]"), None);
    }

    #[test]
    fn insert_get_remove() {
        let mut r = SessionRegistry::default();
        r.insert(7, "[7]".into());
        assert_eq!(r.len(), 1);
        assert!(r.get(7).unwrap().can_speak());
        r.remove(7);
        assert!(r.is_empty());
    }

    #[test]
    fn mute_and_deaf_flags_gate_speech_and_hearing() {
        let mut r = SessionRegistry::default();
        let s = r.insert(1, "[1]".into());
        s.mute = true;
        assert!(!r.get(1).unwrap().can_speak());
        let s = r.get_mut(1).unwrap();
        s.mute = false;
        s.self_deaf = true;
        assert!(r.get(1).unwrap().can_speak());
        assert!(!r.get(1).unwrap().can_hear());
    }

    #[test]
    fn context_match_requires_equal_nonempty_contexts() {
        let mut r = SessionRegistry::default();
        r.insert(1, "[1]".into()).context = Some(b"los-santos".to_vec());
        r.insert(2, "[2]".into()).context = Some(b"los-santos".to_vec());
        r.insert(3, "[3]".into()).context = Some(b"other".to_vec());
        r.insert(4, "[4]".into()); // no context
        assert!(r.contexts_match(1, 2));
        assert!(!r.contexts_match(1, 3));
        assert!(!r.contexts_match(1, 4));
    }
}
