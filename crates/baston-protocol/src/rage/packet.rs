//! Packet framing for the OneSync clone/ack streams.
//!
//! Each packed packet is `[u32 msgType][u64 frameIndex][lz4 body]` (the
//! `FlushBuffer` layout, SGS.cpp:747-762). The body is a bit-packed sequence of
//! records terminated by a type-7 `END`. This module owns:
//! - the [`FrameIndex`] bitfield (fragment/frame bookkeeping);
//! - decoding an inbound `netClones`/`netAcks` payload into records;
//! - packing an outbound record buffer into a wire packet.

use super::clone::{
    read_downlink, read_inbound, write_end, write_outbound_clone, write_outbound_remove,
    write_outbound_timestamp, DownlinkRecord, InboundRecord, OutboundClone,
};
use super::{lz4dict, MessageBuffer};

/// `HashRageString("netClones")` — inbound clone stream tag.
pub const NET_CLONES: u32 = crate::udp::hash_rage_string("netClones");
/// `HashRageString("netAcks")` — inbound ack stream tag.
pub const NET_ACKS: u32 = crate::udp::hash_rage_string("netAcks");
/// `HashRageString("msgPackedClones")` — outbound clone stream tag.
pub const MSG_PACKED_CLONES: u32 = crate::udp::hash_rage_string("msgPackedClones");
/// `HashRageString("msgPackedAcks")` — outbound ack stream tag.
pub const MSG_PACKED_ACKS: u32 = crate::udp::hash_rage_string("msgPackedAcks");

/// Decompressed inbound scratch size (the engine uses a 16 KiB stack buffer,
/// `ParseGameStatePacket`, SGS.cpp:3827).
pub const INBOUND_SCRATCH: usize = 16384;

/// Flush threshold: keep compressed packets under ~1100 bytes minus headers
/// (`MaybeFlushBuffer`, SGS.cpp:785).
pub const FLUSH_BUDGET_BYTES: usize = 1100 - 12 - 12;

/// Fragmented frame identifier: `lastFragment:1 | currentFragment:7 |
/// frameIndex:56`, LSB-first as the C++ union packs it on x86.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameIndex {
    pub last_fragment: bool,
    pub current_fragment: u8,
    pub frame_index: u64,
}

impl FrameIndex {
    pub fn to_u64(self) -> u64 {
        (self.last_fragment as u64)
            | ((self.current_fragment as u64 & 0x7F) << 1)
            | ((self.frame_index & ((1u64 << 56) - 1)) << 8)
    }

    pub fn from_u64(v: u64) -> Self {
        Self {
            last_fragment: (v & 1) != 0,
            current_fragment: ((v >> 1) & 0x7F) as u8,
            frame_index: (v >> 8) & ((1u64 << 56) - 1),
        }
    }
}

/// A decoded inbound packed packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingPacket {
    pub msg_type: u32,
    pub records: Vec<InboundRecord>,
}

/// Decode a raw inbound payload `[u32 type][lz4 body]` (the tail the client
/// uploads). Returns `None` if the type isn't a clone/ack stream or LZ4 fails,
/// matching the `length <= 0` bail in `ParseGameStatePacket`.
pub fn decode_incoming(payload: &[u8]) -> Option<IncomingPacket> {
    if payload.len() < 4 {
        return None;
    }
    let msg_type = u32::from_le_bytes(payload[..4].try_into().ok()?);
    if msg_type != NET_CLONES && msg_type != NET_ACKS {
        return None;
    }
    let body = lz4dict::decompress_using_dict(&payload[4..], INBOUND_SCRATCH)?;

    // NOTE: inbound decoding assumes NATIVE mode (13-bit object ids). BASTON
    // runs non-OneSync today, so the big-mode length hack (16-bit ids) is never
    // active on this path — unlike the ack path (`onesync.rs`), which enables it
    // symmetrically. If big-mode inbound is ever wired, the `length_hack` flag
    // MUST be threaded down to here (via `.with_length_hack(...)`) or object ids
    // desync by 3 bits and every record parses as garbage.
    let mut buf = MessageBuffer::from_bytes(body);
    let mut records = Vec::new();
    while !buf.is_at_end() {
        let ty = match buf.read_bits_single(3) {
            Some(t) => t,
            None => break,
        };
        match read_inbound(ty, &mut buf) {
            Some(r) => records.push(r),
            None => break, // END or malformed
        }
    }
    Some(IncomingPacket { msg_type, records })
}

/// A decoded server→client packed packet (the client's view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownlinkPacket {
    pub msg_type: u32,
    pub frame: FrameIndex,
    pub records: Vec<DownlinkRecord>,
}

/// Decode one outbound fragment `[u32 type][u64 frameIndex][lz4 body]` produced
/// by [`PackedWriter`]/`pack_frame` (plain LZ4, no dictionary). This is what a
/// simulated/headless client runs to observe server-parsed state.
pub fn decode_downlink(packet: &[u8]) -> Option<DownlinkPacket> {
    if packet.len() < 12 {
        return None;
    }
    let msg_type = u32::from_le_bytes(packet[..4].try_into().ok()?);
    let frame = FrameIndex::from_u64(u64::from_le_bytes(packet[4..12].try_into().ok()?));
    // Outbound is plain LZ4; decompress with an empty dictionary window.
    let body = lz4dict::decompress_plain(&packet[12..], INBOUND_SCRATCH)?;
    let mut buf = MessageBuffer::from_bytes(body);
    let mut records = Vec::new();
    while !buf.is_at_end() {
        let ty = match buf.read_bits_single(3) {
            Some(t) => t,
            None => break,
        };
        match read_downlink(ty, &mut buf) {
            Some(r) => records.push(r),
            None => break,
        }
    }
    Some(DownlinkPacket {
        msg_type,
        frame,
        records,
    })
}

/// Pack a finished record buffer into a wire packet: `[u32 msgType][u64
/// frameIndex][lz4 body]`. The caller has already written its records (and a
/// trailing `END`) into `body`; `used_bytes` is `MessageBuffer::data_length()`.
pub fn pack_frame(msg_type: u32, frame: FrameIndex, body: &[u8]) -> Vec<u8> {
    let compressed = lz4dict::compress_default(body);
    let mut out = Vec::with_capacity(12 + compressed.len());
    out.extend_from_slice(&msg_type.to_le_bytes());
    out.extend_from_slice(&frame.to_u64().to_le_bytes());
    out.extend_from_slice(&compressed);
    out
}

/// Working buffer size for one fragment before compression. The engine caps a
/// sync packet at ~2.4 KiB, but a single clone blob can be up to 4 KiB (12-bit
/// length); 16 KiB matches the inbound scratch and comfortably holds any one
/// record plus the running batch.
const WORK_BUF_BYTES: usize = 16384;

/// Stateful per-client outbound assembler for `msgPackedClones` /
/// `msgPackedAcks`. Records are appended and auto-fragmented: whenever the next
/// record would push the compressed size past [`FLUSH_BUDGET_BYTES`], the
/// current batch is flushed as one fragment (incrementing the fragment
/// counter) and a fresh batch starts. [`Self::finish`] appends the terminating
/// `END`, emits the final fragment with `last_fragment = true`, and returns all
/// wire packets. Mirrors `FlushBuffer`/`MaybeFlushBuffer` (SGS.cpp:737-789).
pub struct PackedWriter {
    msg_type: u32,
    frame_index: u64,
    buf: MessageBuffer,
    fragment: u8,
    packets: Vec<Vec<u8>>,
    had_time: bool,
    /// Length hack (OneSync big-id): widen 13-bit fields to 16 bits.
    length_hack: bool,
}

impl PackedWriter {
    pub fn new(msg_type: u32, frame_index: u64, length_hack: bool) -> Self {
        Self {
            msg_type,
            frame_index,
            buf: MessageBuffer::new(WORK_BUF_BYTES).with_length_hack(length_hack),
            fragment: 0,
            packets: Vec::new(),
            had_time: false,
            length_hack,
        }
    }

    /// Emit the current batch as a fragment (not the last one).
    fn flush_fragment(&mut self) {
        // Nothing meaningful buffered: skip (the engine deliberately still
        // sends blank frames to keep ACKs flowing, but that concerns the final
        // flush; intermediate flushes only happen when data is present).
        let used = self.buf.data_length();
        if used == 0 {
            return;
        }
        // Every fragment is self-terminated with END (FlushBuffer, SGS.cpp:744).
        write_end(&mut self.buf);
        let used = self.buf.data_length();
        let raw = self.buf.buffer()[..used].to_vec();
        self.fragment = self.fragment.wrapping_add(1);
        let frame = FrameIndex {
            last_fragment: false,
            current_fragment: self.fragment & 0x7F,
            frame_index: self.frame_index,
        };
        self.packets.push(pack_frame(self.msg_type, frame, &raw));
        self.buf = MessageBuffer::new(WORK_BUF_BYTES).with_length_hack(self.length_hack);
        self.had_time = false;
    }

    /// Flush if appending `next_bits` more bits would exceed the budget.
    fn maybe_flush(&mut self, next_bits: usize) {
        let projected = self.buf.data_length() + next_bits.div_ceil(8);
        // LZ4 worst case ~= n + n/255 + 16; compare against the byte budget.
        let bound = projected + projected / 255 + 16;
        if bound > FLUSH_BUDGET_BYTES {
            self.flush_fragment();
        }
    }

    /// Ensure a timestamp record precedes the first clone of a fragment
    /// (SGS.cpp:1869-1879). Idempotent within a fragment.
    fn ensure_timestamp(&mut self, timestamp: u64) {
        if !self.had_time {
            self.maybe_flush(3 + 32 + 32);
            write_outbound_timestamp(&mut self.buf, timestamp);
            self.had_time = true;
        }
    }

    /// Append a create/sync clone record, timestamped and auto-fragmented.
    pub fn write_clone(&mut self, timestamp: u64, clone: &OutboundClone<'_>) {
        self.ensure_timestamp(timestamp);
        let bits = 3 + 16 + 16 + 4 + 32 + 16 + 64 + 32 + 12 + clone.data.len() * 8;
        self.maybe_flush(bits);
        write_outbound_clone(&mut self.buf, clone);
    }

    /// Append a removal record.
    pub fn write_remove(&mut self, force_steal: bool, object_id: u16, uniqifier: u16) {
        self.maybe_flush(3 + 1 + 16 + 16);
        write_outbound_remove(&mut self.buf, force_steal, object_id, uniqifier);
    }

    /// Finalize: append `END`, emit the last fragment (always sent, even if
    /// blank, so the client keeps ACKing — SGS.cpp:740), return all packets.
    pub fn finish(mut self) -> Vec<Vec<u8>> {
        write_end(&mut self.buf);
        let used = self.buf.data_length();
        let raw = self.buf.buffer()[..used].to_vec();
        self.fragment = self.fragment.wrapping_add(1);
        let frame = FrameIndex {
            last_fragment: true,
            current_fragment: self.fragment & 0x7F,
            frame_index: self.frame_index,
        };
        self.packets.push(pack_frame(self.msg_type, frame, &raw));
        self.packets
    }
}

#[cfg(test)]
mod tests {
    use super::super::clone::{rec, write_end, NetObjEntityType, OutboundClone};
    use super::*;

    #[test]
    fn untrusted_parsers_never_panic_on_arbitrary_input() {
        // Deterministic pseudo-random byte sweep over every public parse entry
        // point an untrusted client can reach. Contract: return None/empty,
        // never panic. Cheap stand-in for a cargo-fuzz target on stable.
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        for _ in 0..4000 {
            let len = (next() % 600) as usize;
            let buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = decode_incoming(&buf);
            let _ = decode_downlink(&buf);
            let _ = super::super::reliability::parse_nack(&buf);
            let _ = super::super::reliability::parse_ack(&buf);
            let _ = super::super::object_ids::parse_object_ids(&buf);
            let _ = super::super::lz4dict::decompress_using_dict(&buf, 16 * 1024);
        }
    }

    #[test]
    fn frame_index_bitfield_roundtrip() {
        let f = FrameIndex {
            last_fragment: true,
            current_fragment: 0x5A & 0x7F,
            frame_index: 0x0011_2233_4455,
        };
        assert_eq!(FrameIndex::from_u64(f.to_u64()), f);
        // last_fragment lands in bit 0.
        assert_eq!(f.to_u64() & 1, 1);
    }

    #[test]
    fn distinct_stream_tags() {
        // Sanity: the four tags are distinct non-zero hashes.
        let tags = [NET_CLONES, NET_ACKS, MSG_PACKED_CLONES, MSG_PACKED_ACKS];
        for (i, a) in tags.iter().enumerate() {
            assert_ne!(*a, 0);
            for b in &tags[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn pack_then_manual_unpack_header() {
        let mut body = MessageBuffer::new(64);
        assert!(body.write_bits_single(rec::TIMESTAMP, 3));
        assert!(body.write_bits_single(0x1234, 32));
        assert!(body.write_bits_single(0, 32));
        assert!(write_end(&mut body));
        let used = body.data_length();
        let raw = body.into_inner();
        let frame = FrameIndex {
            last_fragment: true,
            current_fragment: 1,
            frame_index: 99,
        };
        let packet = pack_frame(MSG_PACKED_ACKS, frame, &raw[..used]);

        assert_eq!(
            u32::from_le_bytes(packet[..4].try_into().unwrap()),
            MSG_PACKED_ACKS
        );
        assert_eq!(
            FrameIndex::from_u64(u64::from_le_bytes(packet[4..12].try_into().unwrap())),
            frame
        );
    }

    #[test]
    fn incoming_rejects_non_clone_type() {
        let mut payload = 0xDEAD_BEEFu32.to_le_bytes().to_vec();
        payload.extend_from_slice(&[0u8; 8]);
        assert!(decode_incoming(&payload).is_none());
    }

    fn sample_clone(object_id: u16, blob: &[u8]) -> OutboundClone<'_> {
        OutboundClone {
            sync_type: rec::SYNC,
            object_id,
            owner_net_id: 1,
            entity_type: None,
            creation_token: 0,
            uniqifier: 0x2222,
            dependent_frame_index: 0,
            timestamp: 0x5555,
            first_frame_update: false,
            data: blob,
        }
    }

    #[test]
    fn writer_single_fragment_roundtrips_through_client_decoder() {
        let mut w = PackedWriter::new(MSG_PACKED_CLONES, 77, false);
        let create = OutboundClone {
            sync_type: rec::CREATE,
            object_id: 10,
            owner_net_id: 4,
            entity_type: Some(NetObjEntityType::Ped),
            creation_token: 0xABCD,
            uniqifier: 0x0F0F,
            dependent_frame_index: 0,
            timestamp: 0x1000,
            first_frame_update: false,
            data: &[0x11, 0x22],
        };
        w.write_clone(0xDEAD_BEEF, &create);
        w.write_clone(0xDEAD_BEEF, &sample_clone(11, &[0x33]));
        let packets = w.finish();

        assert_eq!(packets.len(), 1);
        let decoded = decode_downlink(&packets[0]).unwrap();
        assert_eq!(decoded.msg_type, MSG_PACKED_CLONES);
        assert!(decoded.frame.last_fragment);
        assert_eq!(decoded.frame.frame_index, 77);

        // Timestamp record first, then the two clones.
        assert_eq!(decoded.records[0], DownlinkRecord::Timestamp(0xDEAD_BEEF));
        assert!(matches!(
            &decoded.records[1],
            DownlinkRecord::Clone { is_create: true, object_id: 10, entity_type: Some(NetObjEntityType::Ped), data, .. } if data == &[0x11, 0x22]
        ));
        assert!(matches!(
            &decoded.records[2],
            DownlinkRecord::Clone { is_create: false, object_id: 11, data, .. } if data == &[0x33]
        ));
    }

    #[test]
    fn writer_fragments_when_over_budget() {
        // Many sizeable clones must spill into multiple fragments, the last
        // flagged last_fragment, all sharing the frame index with increasing
        // fragment counters.
        let mut w = PackedWriter::new(MSG_PACKED_CLONES, 5, false);
        let blob = vec![0xABu8; 400]; // ~400B each; budget is ~1076B compressed
        for id in 0..40u16 {
            w.write_clone(1234, &sample_clone(id, &blob));
        }
        let packets = w.finish();
        assert!(
            packets.len() >= 2,
            "expected fragmentation, got {}",
            packets.len()
        );

        let mut total_clones = 0;
        for (i, pkt) in packets.iter().enumerate() {
            let d = decode_downlink(pkt).unwrap();
            assert_eq!(d.frame.frame_index, 5);
            assert_eq!(d.frame.current_fragment as usize, i + 1);
            assert_eq!(d.frame.last_fragment, i == packets.len() - 1);
            total_clones += d
                .records
                .iter()
                .filter(|r| matches!(r, DownlinkRecord::Clone { .. }))
                .count();
        }
        assert_eq!(total_clones, 40);
    }

    #[test]
    fn writer_remove_record_roundtrips() {
        let mut w = PackedWriter::new(MSG_PACKED_CLONES, 1, false);
        w.write_remove(true, 99, 0x7777);
        let packets = w.finish();
        let d = decode_downlink(&packets[0]).unwrap();
        assert_eq!(
            d.records[0],
            DownlinkRecord::Remove {
                force_steal: true,
                object_id: 99,
                uniqifier: 0x7777
            }
        );
    }

    #[test]
    fn incoming_clone_stream_roundtrip() {
        // Build a netClones body (create + sync + end), compress it dictless so
        // the dict decompressor can still read it, and decode.
        use super::super::clone::NetObjEntityType;
        // We reuse the outbound writer to synthesize a record stream, but the
        // INBOUND parser expects the inbound field order — so hand-write instead.
        let mut body = MessageBuffer::new(256);
        // create: type, uniqifier, objectId, token, entityType, len, blob
        body.write_bits_single(rec::CREATE, 3);
        body.write_bits_single(0xAAAA, 16);
        body.write_bits_single(50, 13);
        body.write_bits_single(0x1111_2222, 32);
        body.write_bits_single(NetObjEntityType::Player as u32, 4);
        body.write_bits_single(2, 12);
        body.write_bits(&[0x77, 0x88], 16);
        // sync: type, uniqifier, objectId, len, blob
        body.write_bits_single(rec::SYNC, 3);
        body.write_bits_single(0xAAAA, 16);
        body.write_bits_single(50, 13);
        body.write_bits_single(0, 12);
        write_end(&mut body);
        let used = body.data_length();
        let raw = body.into_inner();

        let compressed = lz4dict::compress_default(&raw[..used]);
        let mut payload = NET_CLONES.to_le_bytes().to_vec();
        payload.extend_from_slice(&compressed);

        let decoded = decode_incoming(&payload).unwrap();
        assert_eq!(decoded.msg_type, NET_CLONES);
        assert_eq!(decoded.records.len(), 2);
        assert!(matches!(
            &decoded.records[0],
            InboundRecord::Clone {
                is_create: true,
                object_id: 50,
                uniqifier: 0xAAAA,
                ..
            }
        ));
        assert!(matches!(
            &decoded.records[1],
            InboundRecord::Clone { is_create: false, object_id: 50, data, .. } if data.is_empty()
        ));
    }
}
