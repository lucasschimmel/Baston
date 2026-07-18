//! Inbound `netClones` ingestion and arbitration for [`ServerGameState`].

use baston_protocol::rage::clone::{write_ack_record, write_end, InboundRecord, NetObjEntityType};
use baston_protocol::rage::packet::{decode_incoming, pack_frame, FrameIndex, MSG_PACKED_ACKS};
use baston_protocol::rage::MessageBuffer;

use super::{ClientState, IdState, IngestOutcome, ServerEntity, ServerGameState};

impl ServerGameState {
    /// Ingest one inbound `msgRoute` clone payload (the `[u32 type][lz4]` tail).
    /// Updates the registry and returns ack packets. Returns an empty outcome
    /// if the payload isn't a clone/ack stream or fails to decode.
    pub fn ingest_clone_payload(&mut self, source: u32, payload: &[u8]) -> IngestOutcome {
        let Some(incoming) = decode_incoming(payload) else {
            return IngestOutcome::default();
        };
        self.add_client(source);

        let mut ack = MessageBuffer::new(4096).with_length_hack(self.length_hack);
        let mut outcome = IngestOutcome::default();

        for record in incoming.records {
            match record {
                InboundRecord::Clone {
                    is_create,
                    object_id,
                    uniqifier,
                    entity_type,
                    creation_token,
                    data,
                } => {
                    if object_id == 0xFFFF {
                        continue; // "that's not an object ID, that's a snail!"
                    }
                    if is_create {
                        // Only claim the object id if the create was actually
                        // accepted — a rejected create (owner conflict or
                        // invalid entity type, both client-forgeable) must not
                        // permanently mark the id used, or the id space leaks.
                        if self.apply_create(
                            source,
                            object_id,
                            uniqifier,
                            entity_type,
                            creation_token,
                            data,
                        ) {
                            self.ids[object_id as usize] = IdState::Used;
                        }
                        write_ack_record(&mut ack, 1, object_id, uniqifier);
                        outcome.creates += 1;
                    } else {
                        self.apply_sync(source, object_id, uniqifier, data);
                        write_ack_record(&mut ack, 2, object_id, uniqifier);
                        outcome.syncs += 1;
                    }
                }
                InboundRecord::Remove {
                    object_id,
                    uniqifier,
                } => {
                    // Ack the remove regardless of acceptance (SGS.cpp:3155).
                    write_ack_record(&mut ack, 3, object_id, uniqifier);
                    self.apply_remove(source, object_id, uniqifier);
                    outcome.removes += 1;
                }
                InboundRecord::Takeover {
                    target_client,
                    object_id,
                } => {
                    let target = if target_client == 0 {
                        source as u16
                    } else {
                        target_client
                    };
                    outcome.takeovers.push((object_id, target));
                    self.apply_takeover(source, object_id, target);
                }
                InboundRecord::Timestamp(ts) => {
                    let state = self
                        .clients
                        .entry(source)
                        .or_insert_with(|| ClientState::new(source as u16));
                    if state.ack_ts == 0 || state.ack_ts < ts {
                        state.ack_ts = ts;
                    }
                    // Echo the timestamp ack (type 5) as the engine does.
                    ack.write_bits_single(5, 3);
                    ack.write_bits_single(ts, 32);
                }
                InboundRecord::Index(idx) => {
                    self.clients
                        .entry(source)
                        .or_insert_with(|| ClientState::new(source as u16))
                        .frame_index = u64::from(idx);
                }
            }
        }

        write_end(&mut ack);
        let used = ack.data_length();
        if used > 0 {
            let frame = FrameIndex {
                last_fragment: true,
                current_fragment: 0,
                frame_index: self.clients.get(&source).map_or(0, |c| c.frame_index),
            };
            outcome
                .ack_packets
                .push(pack_frame(MSG_PACKED_ACKS, frame, &ack.buffer()[..used]));
        }
        outcome
    }

    /// Returns `true` if the entity was created/refreshed, `false` if the
    /// create was rejected (so the caller must not claim the object id).
    fn apply_create(
        &mut self,
        source: u32,
        object_id: u16,
        uniqifier: u16,
        entity_type: Option<NetObjEntityType>,
        creation_token: u32,
        data: Vec<u8>,
    ) -> bool {
        // Duplicate create for a live entity with a different owner: reject
        // (SGS.cpp:3427). Same owner refreshes.
        if let Some(existing) = self.entities.get(&object_id) {
            if existing.owner != source || existing.uniqifier != uniqifier {
                return false;
            }
        }
        let Some(entity_type) = entity_type else {
            return false; // invalid type → no sync tree → drop (SGS.cpp:3417)
        };
        let frame_index = self.frame_index;
        // Preserve a known position across a same-owner re-create.
        let position = self
            .entities
            .get(&object_id)
            .map_or([0.0; 3], |e| e.position);
        self.entities.insert(
            object_id,
            ServerEntity {
                object_id,
                uniqifier,
                owner: source,
                entity_type,
                creation_token,
                data,
                frame_index,
                position,
            },
        );
        true
    }

    fn apply_sync(&mut self, source: u32, object_id: u16, uniqifier: u16, data: Vec<u8>) {
        let frame_index = self.frame_index;
        if let Some(ent) = self.entities.get_mut(&object_id) {
            // Wrong uniqifier or wrong owner → ignore (SGS.cpp:3454, 3491).
            if ent.uniqifier != uniqifier || ent.owner != source {
                return;
            }
            if !data.is_empty() {
                ent.data = data;
                ent.frame_index = frame_index;
            }
        }
    }

    fn apply_remove(&mut self, source: u32, object_id: u16, uniqifier: u16) {
        if let Some(ent) = self.entities.get(&object_id) {
            // Only the owner may remove, and the uniqifier must match.
            if ent.owner == source && ent.uniqifier == uniqifier {
                self.entities.remove(&object_id);
                self.ids[object_id as usize] = IdState::Free;
            }
        }
    }

    fn apply_takeover(&mut self, sender: u32, object_id: u16, target: u16) {
        if let Some(ent) = self.entities.get_mut(&object_id) {
            // The sender must currently own the entity (SGS.cpp:3128).
            if ent.owner == sender {
                ent.owner = u32::from(target);
            }
        }
    }
}
