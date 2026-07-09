//! Channel model — Root + "Game Channel N" + temporary channels.
//!
//! Mirrors the FXServer/umurmur channel semantics (`ServerMumbleVoice.cpp`,
//! `channel.c`): a permanent Root (id 0), script-created "Game Channel N"
//! permanent channels (`MUMBLE_CREATE_CHANNEL`), and temporary channels that
//! vanish when their last member leaves. pma-voice drives proximity via the
//! Root channel + voice targets, so this stays deliberately simple.

use std::collections::{BTreeMap, BTreeSet};

/// The always-present root channel id (proximity chat lives here).
pub const ROOT_CHANNEL: u32 = 0;

/// One channel's state.
#[derive(Debug, Clone)]
pub struct Channel {
    pub id: u32,
    pub name: String,
    pub parent: u32,
    pub temporary: bool,
    /// Sessions currently *in* the channel.
    members: BTreeSet<u32>,
    /// Sessions listening to this channel without being in it (ChannelListener).
    listeners: BTreeSet<u32>,
}

impl Channel {
    /// Sessions physically in the channel.
    pub fn members(&self) -> impl Iterator<Item = u32> + '_ {
        self.members.iter().copied()
    }

    /// Sessions listening to (but not in) the channel.
    pub fn listeners(&self) -> impl Iterator<Item = u32> + '_ {
        self.listeners.iter().copied()
    }
}

/// Store of all channels. Not thread-safe on its own — the voice manager wraps
/// it in the appropriate lock; kept lock-free here for unit-testability.
#[derive(Debug)]
pub struct ChannelStore {
    channels: BTreeMap<u32, Channel>,
}

impl Default for ChannelStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelStore {
    /// A fresh store with only the Root channel.
    pub fn new() -> Self {
        let mut channels = BTreeMap::new();
        channels.insert(
            ROOT_CHANNEL,
            Channel {
                id: ROOT_CHANNEL,
                name: "Root".to_owned(),
                parent: ROOT_CHANNEL,
                temporary: false,
                members: BTreeSet::new(),
                listeners: BTreeSet::new(),
            },
        );
        Self { channels }
    }

    /// Create a permanent "Game Channel N" under Root, matching FXServer's
    /// `MUMBLE_CREATE_CHANNEL` naming. Idempotent: an existing id is returned
    /// unchanged.
    pub fn create_game_channel(&mut self, id: u32) -> &Channel {
        self.channels.entry(id).or_insert_with(|| Channel {
            id,
            name: format!("Game Channel {id}"),
            parent: ROOT_CHANNEL,
            temporary: false,
            members: BTreeSet::new(),
            listeners: BTreeSet::new(),
        });
        &self.channels[&id]
    }

    /// Whether a channel exists (backs `MUMBLE_DOES_CHANNEL_EXIST`).
    pub fn exists(&self, id: u32) -> bool {
        self.channels.contains_key(&id)
    }

    pub fn get(&self, id: u32) -> Option<&Channel> {
        self.channels.get(&id)
    }

    /// Iterate every channel (for the ChannelState burst at login).
    pub fn iter(&self) -> impl Iterator<Item = &Channel> {
        self.channels.values()
    }

    /// Move `session` into `channel_id`, removing it from any previous channel.
    /// Auto-creates a temporary channel for an unknown id (Mumble lets clients
    /// enter transient channels). Returns the channel the session left, if any.
    pub fn join(&mut self, session: u32, channel_id: u32) -> Option<u32> {
        let previous = self.leave(session);
        self.channels
            .entry(channel_id)
            .or_insert_with(|| Channel {
                id: channel_id,
                name: format!("Game Channel {channel_id}"),
                parent: ROOT_CHANNEL,
                temporary: true,
                members: BTreeSet::new(),
                listeners: BTreeSet::new(),
            })
            .members
            .insert(session);
        previous
    }

    /// Remove `session` from whatever channel it is in. Cleans up an emptied
    /// temporary channel. Returns the channel id it was in.
    pub fn leave(&mut self, session: u32) -> Option<u32> {
        let mut left = None;
        for ch in self.channels.values_mut() {
            if ch.members.remove(&session) {
                left = Some(ch.id);
                break;
            }
        }
        if let Some(id) = left {
            self.gc_temporary(id);
        }
        left
    }

    /// Add/remove a ChannelListener subscription (listen without joining).
    pub fn set_listening(&mut self, session: u32, channel_id: u32, listening: bool) {
        if let Some(ch) = self.channels.get_mut(&channel_id) {
            if listening {
                ch.listeners.insert(session);
            } else {
                ch.listeners.remove(&session);
            }
        }
    }

    /// Drop a session everywhere (disconnect): membership + all listens.
    pub fn drop_session(&mut self, session: u32) {
        let in_channel = self.leave(session);
        for ch in self.channels.values_mut() {
            ch.listeners.remove(&session);
        }
        let _ = in_channel;
    }

    fn gc_temporary(&mut self, id: u32) {
        if id == ROOT_CHANNEL {
            return;
        }
        if let Some(ch) = self.channels.get(&id) {
            if ch.temporary && ch.members.is_empty() && ch.listeners.is_empty() {
                self.channels.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_only_root() {
        let s = ChannelStore::new();
        assert!(s.exists(ROOT_CHANNEL));
        assert_eq!(s.iter().count(), 1);
    }

    #[test]
    fn create_game_channel_is_idempotent_and_named() {
        let mut s = ChannelStore::new();
        assert_eq!(s.create_game_channel(6743).name, "Game Channel 6743");
        s.create_game_channel(6743);
        assert_eq!(s.iter().count(), 2); // root + one
    }

    #[test]
    fn join_moves_between_channels() {
        let mut s = ChannelStore::new();
        s.join(1, ROOT_CHANNEL);
        assert_eq!(
            s.get(ROOT_CHANNEL).unwrap().members().collect::<Vec<_>>(),
            vec![1]
        );
        let left = s.join(1, 100);
        assert_eq!(left, Some(ROOT_CHANNEL));
        assert_eq!(s.get(100).unwrap().members().collect::<Vec<_>>(), vec![1]);
        assert_eq!(s.get(ROOT_CHANNEL).unwrap().members().count(), 0);
    }

    #[test]
    fn temporary_channel_is_gced_when_empty() {
        let mut s = ChannelStore::new();
        s.join(1, 500); // auto temporary
        assert!(s.exists(500));
        s.leave(1);
        assert!(!s.exists(500), "empty temporary channel is removed");
    }

    #[test]
    fn permanent_game_channel_survives_emptying() {
        let mut s = ChannelStore::new();
        s.create_game_channel(7);
        s.join(1, 7);
        s.leave(1);
        assert!(s.exists(7), "permanent channel persists");
    }

    #[test]
    fn listeners_tracked_and_dropped() {
        let mut s = ChannelStore::new();
        s.create_game_channel(9);
        s.set_listening(1, 9, true);
        assert_eq!(s.get(9).unwrap().listeners().collect::<Vec<_>>(), vec![1]);
        s.drop_session(1);
        assert_eq!(s.get(9).unwrap().listeners().count(), 0);
    }
}
