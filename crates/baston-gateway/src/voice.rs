//! Bridges the baston-voice server handle to the scripting `MUMBLE_*`
//! natives. Newtype needed because both the trait (baston-scripting) and the
//! handle (baston-voice) are foreign here.

use baston_scripting::VoiceControl;
use baston_voice::server::VoiceHandle;

/// The gateway's [`VoiceControl`] implementation over the running voice
/// server.
pub struct GatewayVoice(pub VoiceHandle);

impl VoiceControl for GatewayVoice {
    fn create_channel(&self, id: u32) {
        self.0.create_channel(id);
    }

    fn channel_exists(&self, id: u32) -> bool {
        self.0.channel_exists(id)
    }

    fn set_player_muted(&self, netid: u32, muted: bool) {
        self.0.set_player_muted(netid, muted);
    }

    fn is_player_muted(&self, netid: u32) -> bool {
        self.0.is_player_muted(netid)
    }

    fn set_proximity_override(&self, netid: u32, position: Option<[f32; 3]>) {
        self.0.set_proximity_override(netid, position);
    }

    fn proximity_override(&self, netid: u32) -> [f32; 3] {
        self.0.proximity_override(netid)
    }
}
