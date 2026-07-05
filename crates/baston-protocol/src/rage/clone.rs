//! OneSync clone stream — record layouts for both directions.
//!
//! The 3-bit record type prefixes each record; the packet reader/writer loops
//! consume it and dispatch here. The two directions have **different** field
//! orders, faithfully reproduced from the engine:
//! - inbound (client→server): `ProcessClonePacket` (SGS.cpp:3336) and the
//!   remove/takeover handlers (SGS.cpp:3102-3188);
//! - outbound (server→client): the `SyncCommandState` writer (SGS.cpp:1598-1943)
//!   as read back by `msgClone::Read` (CloneManager.cpp:806).

use super::MessageBuffer;

/// Record type tags (3-bit), shared by both directions.
pub mod rec {
    pub const CREATE: u32 = 1;
    pub const SYNC: u32 = 2;
    pub const REMOVE: u32 = 3;
    pub const TAKEOVER: u32 = 4;
    pub const TIMESTAMP: u32 = 5;
    pub const INDEX: u32 = 6;
    pub const END: u32 = 7;
}

/// GTA V network object types (`NetObjEntityType.h`, `STATE_FIVE`). The wire
/// field is [`ENTITY_TYPE_BITS`] wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum NetObjEntityType {
    Automobile = 0,
    Bike = 1,
    Boat = 2,
    Door = 3,
    Heli = 4,
    Object = 5,
    Ped = 6,
    Pickup = 7,
    PickupPlacement = 8,
    Plane = 9,
    Submarine = 10,
    Player = 11,
    Trailer = 12,
    Train = 13,
}

/// Bit width of the entity-type field for GTA V (`kNetObjectTypeBitLength`).
pub const ENTITY_TYPE_BITS: usize = 4;

impl NetObjEntityType {
    /// Every GTA5 variant in wire order (excludes the `Max = 14` sentinel).
    pub const ALL: [NetObjEntityType; 14] = [
        NetObjEntityType::Automobile,
        NetObjEntityType::Bike,
        NetObjEntityType::Boat,
        NetObjEntityType::Door,
        NetObjEntityType::Heli,
        NetObjEntityType::Object,
        NetObjEntityType::Ped,
        NetObjEntityType::Pickup,
        NetObjEntityType::PickupPlacement,
        NetObjEntityType::Plane,
        NetObjEntityType::Submarine,
        NetObjEntityType::Player,
        NetObjEntityType::Trailer,
        NetObjEntityType::Train,
    ];

    pub fn from_u8(v: u8) -> Option<Self> {
        use NetObjEntityType::*;
        Some(match v {
            0 => Automobile,
            1 => Bike,
            2 => Boat,
            3 => Door,
            4 => Heli,
            5 => Object,
            6 => Ped,
            7 => Pickup,
            8 => PickupPlacement,
            9 => Plane,
            10 => Submarine,
            11 => Player,
            12 => Trailer,
            13 => Train,
            _ => return None,
        })
    }

    pub fn is_vehicle(self) -> bool {
        use NetObjEntityType::*;
        matches!(
            self,
            Automobile | Bike | Boat | Heli | Plane | Submarine | Trailer | Train
        )
    }
}

/// A parsed inbound clone record (client→server `netClones` stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundRecord {
    /// Type 1/2: create carries `entity_type` + `creation_token`; sync doesn't.
    Clone {
        is_create: bool,
        object_id: u16,
        uniqifier: u16,
        entity_type: Option<NetObjEntityType>,
        creation_token: u32,
        /// Raw, still-bit-packed sync-tree blob (the server stores it opaquely
        /// and only semantically parses a subset of nodes elsewhere).
        data: Vec<u8>,
    },
    /// Type 3: the owner asks to remove one of its entities.
    Remove { object_id: u16, uniqifier: u16 },
    /// Type 4: hand an entity to `target_client` (0 = the sender).
    Takeover { target_client: u16, object_id: u16 },
    /// Type 5: the client's timestamp for subsequent acks.
    Timestamp(u32),
    /// Type 6: the client's frame index.
    Index(u32),
}

/// Parse the body of an inbound record whose 3-bit `ty` was already read.
/// Returns `None` on `END`/unknown (the caller stops), matching the C++ loop.
pub fn read_inbound(ty: u32, buf: &mut MessageBuffer) -> Option<InboundRecord> {
    match ty {
        rec::CREATE | rec::SYNC => {
            let is_create = ty == rec::CREATE;
            // NB: uniqifier BEFORE objectId here (ProcessClonePacket, SGS.cpp:3339).
            let uniqifier = buf.read_bits_single(16)? as u16;
            let object_id = buf.read_bits_single(13)? as u16;
            let (entity_type, creation_token) = if is_create {
                let token = buf.read_bits_single(32)?;
                let ty_raw = buf.read_bits_single(ENTITY_TYPE_BITS)? as u8;
                (NetObjEntityType::from_u8(ty_raw), token)
            } else {
                (None, 0)
            };
            let length = buf.read_bits_single(12)? as usize;
            let mut data = vec![0u8; length];
            if length > 0 && !buf.read_bits(&mut data, length * 8) {
                return None;
            }
            Some(InboundRecord::Clone {
                is_create,
                object_id,
                uniqifier,
                entity_type,
                creation_token,
                data,
            })
        }
        rec::REMOVE => {
            // Remove reads objectId BEFORE uniqifier (ProcessCloneRemove, SGS.cpp:3149).
            let object_id = buf.read_bits_single(13)? as u16;
            let uniqifier = buf.read_bits_single(16)? as u16;
            Some(InboundRecord::Remove {
                object_id,
                uniqifier,
            })
        }
        rec::TAKEOVER => {
            let target_client = buf.read_bits_single(16)? as u16;
            let object_id = buf.read_bits_single(13)? as u16;
            Some(InboundRecord::Takeover {
                target_client,
                object_id,
            })
        }
        rec::TIMESTAMP => Some(InboundRecord::Timestamp(buf.read_bits_single(32)?)),
        rec::INDEX => Some(InboundRecord::Index(buf.read_bits_single(32)?)),
        _ => None, // END or garbage
    }
}

/// One outbound clone record to serialize into a `msgPackedClones` buffer.
#[derive(Debug, Clone)]
pub struct OutboundClone<'a> {
    /// `SYNC` for updates, `CREATE` for first sight.
    pub sync_type: u32,
    pub object_id: u16,
    pub owner_net_id: u16,
    /// Present iff `sync_type == CREATE`.
    pub entity_type: Option<NetObjEntityType>,
    pub creation_token: u32,
    pub uniqifier: u16,
    /// The dependent (base) frame index this delta was cut against.
    pub dependent_frame_index: u64,
    pub timestamp: u32,
    /// First-frame update: uniqifier is bit-inverted on the wire.
    pub first_frame_update: bool,
    /// Raw bit-packed node blob.
    pub data: &'a [u8],
}

/// Write a create/sync clone record (3-bit type + body) as the server does
/// (SGS.cpp:1909-1943), read back by `msgClone::Read`.
pub fn write_outbound_clone(buf: &mut MessageBuffer, c: &OutboundClone<'_>) -> bool {
    let ok = buf.write_bits_single(c.sync_type, 3)
        && buf.write_bits_single(u32::from(c.object_id), 13)
        && buf.write_bits_single(u32::from(c.owner_net_id), 16);
    if !ok {
        return false;
    }
    if c.sync_type == rec::CREATE {
        let ety = c.entity_type.map(|e| e as u8).unwrap_or(0);
        if !(buf.write_bits_single(u32::from(ety), ENTITY_TYPE_BITS)
            && buf.write_bits_single(c.creation_token, 32))
        {
            return false;
        }
    }
    let uniq = if c.first_frame_update {
        !c.uniqifier
    } else {
        c.uniqifier
    };
    // dependent frame index: HIGH word first, then LOW (SGS.cpp:1928-1929).
    let mut ok = buf.write_bits_single(u32::from(uniq), 16)
        && buf.write_bits_single((c.dependent_frame_index >> 32) as u32, 32)
        && buf.write_bits_single(c.dependent_frame_index as u32, 32)
        && buf.write_bits_single(c.timestamp, 32);
    // length (12 bits) + blob.
    ok = ok && buf.write_bits_single(c.data.len() as u32, 12);
    if ok && !c.data.is_empty() {
        ok = buf.write_bits(c.data, c.data.len() * 8);
    }
    ok
}

/// A record decoded from a server→client packed stream (the client's view,
/// `msgPackedClones::Read` + `msgClone::Read`). Used by headless/simulated
/// clients and by writer round-trip tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownlinkRecord {
    Clone {
        is_create: bool,
        object_id: u16,
        owner_net_id: u16,
        entity_type: Option<NetObjEntityType>,
        creation_token: u32,
        /// As transmitted (bit-inverted on first-frame updates).
        uniqifier: u16,
        dependent_frame_index: u64,
        timestamp: u32,
        data: Vec<u8>,
    },
    Remove {
        force_steal: bool,
        object_id: u16,
        uniqifier: u16,
    },
    Timestamp(u64),
}

/// Read one downlink record whose 3-bit `ty` was already consumed. `None` on
/// `END`/unknown. Field order matches [`write_outbound_clone`] /
/// [`write_outbound_remove`] / [`write_outbound_timestamp`].
pub fn read_downlink(ty: u32, buf: &mut MessageBuffer) -> Option<DownlinkRecord> {
    match ty {
        rec::CREATE | rec::SYNC => {
            let is_create = ty == rec::CREATE;
            let object_id = buf.read_bits_single(13)? as u16;
            let owner_net_id = buf.read_bits_single(16)? as u16;
            let (entity_type, creation_token) = if is_create {
                let ety = buf.read_bits_single(ENTITY_TYPE_BITS)? as u8;
                let token = buf.read_bits_single(32)?;
                (NetObjEntityType::from_u8(ety), token)
            } else {
                (None, 0)
            };
            let uniqifier = buf.read_bits_single(16)? as u16;
            let hi = buf.read_bits_single(32)? as u64;
            let lo = buf.read_bits_single(32)? as u64;
            let dependent_frame_index = (hi << 32) | lo;
            let timestamp = buf.read_bits_single(32)?;
            let length = buf.read_bits_single(12)? as usize;
            let mut data = vec![0u8; length];
            if length > 0 && !buf.read_bits(&mut data, length * 8) {
                return None;
            }
            Some(DownlinkRecord::Clone {
                is_create,
                object_id,
                owner_net_id,
                entity_type,
                creation_token,
                uniqifier,
                dependent_frame_index,
                timestamp,
                data,
            })
        }
        rec::REMOVE => {
            let force_steal = buf.read_bit() != 0;
            let object_id = buf.read_bits_single(13)? as u16;
            let uniqifier = buf.read_bits_single(16)? as u16;
            Some(DownlinkRecord::Remove {
                force_steal,
                object_id,
                uniqifier,
            })
        }
        rec::TIMESTAMP => {
            let lo = buf.read_bits_single(32)? as u64;
            let hi = buf.read_bits_single(32)? as u64;
            Some(DownlinkRecord::Timestamp((hi << 32) | lo))
        }
        _ => None,
    }
}

/// Write a type-3 removal record (SGS.cpp:1601-1605).
pub fn write_outbound_remove(
    buf: &mut MessageBuffer,
    force_steal: bool,
    object_id: u16,
    uniqifier: u16,
) -> bool {
    buf.write_bits_single(rec::REMOVE, 3)
        && buf.write_bit(force_steal)
        && buf.write_bits_single(u32::from(object_id), 13)
        && buf.write_bits_single(u32::from(uniqifier), 16)
}

/// Write a type-5 timestamp record: LOW word first, then HIGH (SGS.cpp:1874-1876).
pub fn write_outbound_timestamp(buf: &mut MessageBuffer, timestamp: u64) -> bool {
    buf.write_bits_single(rec::TIMESTAMP, 3)
        && buf.write_bits_single(timestamp as u32, 32)
        && buf.write_bits_single((timestamp >> 32) as u32, 32)
}

/// Write the terminating type-7 record.
pub fn write_end(buf: &mut MessageBuffer) -> bool {
    buf.write_bits_single(rec::END, 3)
}

/// Write one ack record into a `msgPackedAcks` buffer: `3-bit type | 13-bit
/// objectId | 16-bit uniqifier` (the compact ack form the server emits per
/// processed clone, SGS.cpp:3080-3097). `ty` is `CREATE`, `SYNC`, or `REMOVE`.
pub fn write_ack_record(buf: &mut MessageBuffer, ty: u32, object_id: u16, uniqifier: u16) -> bool {
    buf.write_bits_single(ty, 3)
        && buf.write_bits_single(u32::from(object_id), 13)
        && buf.write_bits_single(u32::from(uniqifier), 16)
}

/// One ack record decoded from a server→client `msgPackedAcks` stream (client
/// view, `ParseAckPacket`, SGS.cpp:3859-3905). Only create/remove carry
/// meaning to the client; sync acks confirm receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckRecord {
    pub ty: u32,
    pub object_id: u16,
    pub uniqifier: u16,
}

/// Read one ack record whose 3-bit `ty` was already consumed. `None` on END.
pub fn read_ack_record(ty: u32, buf: &mut MessageBuffer) -> Option<AckRecord> {
    match ty {
        rec::CREATE | rec::SYNC | rec::REMOVE => {
            let object_id = buf.read_bits_single(13)? as u16;
            let uniqifier = buf.read_bits_single(16)? as u16;
            Some(AckRecord {
                ty,
                object_id,
                uniqifier,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_create_roundtrip_against_hand_written_stream() {
        // Build a create record exactly as ProcessClonePacket expects, then parse.
        let mut w = MessageBuffer::new(64);
        assert!(w.write_bits_single(rec::CREATE, 3));
        assert!(w.write_bits_single(0xBEEF, 16)); // uniqifier
        assert!(w.write_bits_single(0x1234 & 0x1FFF, 13)); // objectId (13-bit)
        assert!(w.write_bits_single(0xCAFE_1234, 32)); // creationToken
        assert!(w.write_bits_single(NetObjEntityType::Ped as u32, ENTITY_TYPE_BITS));
        assert!(w.write_bits_single(3, 12)); // length
        assert!(w.write_bits(&[0xAA, 0xBB, 0xCC], 24));

        let mut r = MessageBuffer::from_bytes(w.into_inner());
        let ty = r.read_bits_single(3).unwrap();
        let record = read_inbound(ty, &mut r).unwrap();
        assert_eq!(
            record,
            InboundRecord::Clone {
                is_create: true,
                object_id: 0x1234 & 0x1FFF,
                uniqifier: 0xBEEF,
                entity_type: Some(NetObjEntityType::Ped),
                creation_token: 0xCAFE_1234,
                data: vec![0xAA, 0xBB, 0xCC],
            }
        );
    }

    #[test]
    fn inbound_remove_uses_objectid_first() {
        let mut w = MessageBuffer::new(16);
        assert!(w.write_bits_single(rec::REMOVE, 3));
        assert!(w.write_bits_single(42, 13)); // objectId
        assert!(w.write_bits_single(0x9999, 16)); // uniqifier
        let mut r = MessageBuffer::from_bytes(w.into_inner());
        let ty = r.read_bits_single(3).unwrap();
        assert_eq!(
            read_inbound(ty, &mut r).unwrap(),
            InboundRecord::Remove {
                object_id: 42,
                uniqifier: 0x9999
            }
        );
    }

    #[test]
    fn outbound_sync_read_back_matches_msgclone_layout() {
        // Write with our server writer, then read with the client-side field
        // order from msgClone::Read to prove wire compatibility.
        let blob = [0xDE, 0xAD, 0xBE, 0xEF];
        let clone = OutboundClone {
            sync_type: rec::SYNC,
            object_id: 100,
            owner_net_id: 7,
            entity_type: None,
            creation_token: 0,
            uniqifier: 0x1357,
            dependent_frame_index: 0x0000_00AA_BBBB_CCCC,
            timestamp: 0x1122_3344,
            first_frame_update: false,
            data: &blob,
        };
        let mut w = MessageBuffer::new(128);
        assert!(write_outbound_clone(&mut w, &clone));

        let mut r = MessageBuffer::from_bytes(w.into_inner());
        assert_eq!(r.read_bits_single(3), Some(rec::SYNC));
        assert_eq!(r.read_bits_single(13), Some(100)); // objectId
        assert_eq!(r.read_bits_single(16), Some(7)); // clientId
        assert_eq!(r.read_bits_single(16), Some(0x1357)); // uniqifier
        let hi = r.read_bits_single(32).unwrap() as u64;
        let lo = r.read_bits_single(32).unwrap() as u64;
        assert_eq!((hi << 32) | lo, 0x0000_00AA_BBBB_CCCC);
        assert_eq!(r.read_bits_single(32), Some(0x1122_3344)); // timestamp
        assert_eq!(r.read_bits_single(12), Some(4)); // length
        let mut out = [0u8; 4];
        assert!(r.read_bits(&mut out, 32));
        assert_eq!(out, blob);
    }

    #[test]
    fn outbound_create_includes_type_and_token() {
        let clone = OutboundClone {
            sync_type: rec::CREATE,
            object_id: 5,
            owner_net_id: 3,
            entity_type: Some(NetObjEntityType::Automobile),
            creation_token: 0xABCD_1234,
            uniqifier: 1,
            dependent_frame_index: 0,
            timestamp: 9,
            first_frame_update: false,
            data: &[],
        };
        let mut w = MessageBuffer::new(64);
        assert!(write_outbound_clone(&mut w, &clone));
        let mut r = MessageBuffer::from_bytes(w.into_inner());
        assert_eq!(r.read_bits_single(3), Some(rec::CREATE));
        assert_eq!(r.read_bits_single(13), Some(5));
        assert_eq!(r.read_bits_single(16), Some(3));
        assert_eq!(r.read_bits_single(ENTITY_TYPE_BITS), Some(0)); // Automobile
        assert_eq!(r.read_bits_single(32), Some(0xABCD_1234)); // token
        assert_eq!(r.read_bits_single(16), Some(1)); // uniqifier
    }

    #[test]
    fn first_frame_update_inverts_uniqifier() {
        let clone = OutboundClone {
            sync_type: rec::SYNC,
            object_id: 1,
            owner_net_id: 1,
            entity_type: None,
            creation_token: 0,
            uniqifier: 0x00FF,
            dependent_frame_index: 0,
            timestamp: 0,
            first_frame_update: true,
            data: &[],
        };
        let mut w = MessageBuffer::new(64);
        assert!(write_outbound_clone(&mut w, &clone));
        let mut r = MessageBuffer::from_bytes(w.into_inner());
        r.read_bits_single(3);
        r.read_bits_single(13);
        r.read_bits_single(16);
        assert_eq!(r.read_bits_single(16), Some(u32::from(!0x00FFu16))); // ~uniqifier
    }
}
