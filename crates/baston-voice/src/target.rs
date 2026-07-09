//! VoiceTarget (whisper) registry.
//!
//! A client registers up to 30 voice targets (`VoiceTarget` message, id 1..=30;
//! 0 = normal channel talk, 31 = loopback). Each target is a set of explicit
//! session ids plus a set of channel ids. When the client speaks with that
//! target id in the packet header, the server whispers to exactly those
//! recipients. This is the mechanism pma-voice uses for radios.
//!
//! Ported from umurmur's `voicetarget.{h,c}`.

use std::collections::BTreeMap;

/// The whisper target id range (`VoiceTarget.id`). 0 and 31 are reserved.
pub const MIN_TARGET_ID: u8 = 1;
pub const MAX_TARGET_ID: u8 = 30;

/// One registered target: explicit sessions + channels.
#[derive(Debug, Clone, Default)]
pub struct Target {
    pub sessions: Vec<u32>,
    pub channels: Vec<ChannelTarget>,
}

/// A channel included in a whisper target, with link/child flags.
#[derive(Debug, Clone, Copy)]
pub struct ChannelTarget {
    pub channel_id: u32,
    pub links: bool,
    pub children: bool,
}

/// Per-client set of registered voice targets, keyed by target id.
#[derive(Debug, Default)]
pub struct TargetRegistry {
    targets: BTreeMap<u8, Target>,
}

impl TargetRegistry {
    /// Register (or replace) a target id from a decoded `VoiceTarget` message.
    /// Out-of-range ids are ignored (defensive: a client can't grow our state
    /// beyond 1..=30).
    pub fn set(&mut self, id: u8, target: Target) {
        if (MIN_TARGET_ID..=MAX_TARGET_ID).contains(&id) {
            self.targets.insert(id, target);
        }
    }

    /// Clear a single target id.
    pub fn clear(&mut self, id: u8) {
        self.targets.remove(&id);
    }

    /// Clear every registered target (client reset / disconnect).
    pub fn clear_all(&mut self) {
        self.targets.clear();
    }

    pub fn get(&self, id: u8) -> Option<&Target> {
        self.targets.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_retrieves_a_target() {
        let mut r = TargetRegistry::default();
        r.set(
            3,
            Target {
                sessions: vec![10, 11],
                channels: vec![ChannelTarget {
                    channel_id: 5,
                    links: true,
                    children: false,
                }],
            },
        );
        let t = r.get(3).unwrap();
        assert_eq!(t.sessions, vec![10, 11]);
        assert_eq!(t.channels.len(), 1);
        assert!(t.channels[0].links);
    }

    #[test]
    fn rejects_out_of_range_ids() {
        let mut r = TargetRegistry::default();
        r.set(0, Target::default());
        r.set(31, Target::default());
        r.set(200, Target::default());
        assert!(r.get(0).is_none());
        assert!(r.get(31).is_none());
    }

    #[test]
    fn clear_and_clear_all() {
        let mut r = TargetRegistry::default();
        r.set(
            1,
            Target {
                sessions: vec![1],
                ..Default::default()
            },
        );
        r.set(
            2,
            Target {
                sessions: vec![2],
                ..Default::default()
            },
        );
        r.clear(1);
        assert!(r.get(1).is_none());
        assert!(r.get(2).is_some());
        r.clear_all();
        assert!(r.get(2).is_none());
    }
}
