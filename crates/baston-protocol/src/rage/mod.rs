//! RAGE network serialization primitives (OneSync wire format).
//!
//! `buffer` is the `datBitBuffer`-compatible bit packer everything else builds
//! on: clone records, sync-tree nodes, world-grid state.

mod buffer;
pub mod clone;
pub mod lz4dict;
pub mod object_ids;
pub mod packet;
pub mod reliability;
pub mod session;
pub mod sync_trees;

pub use buffer::MessageBuffer;
pub use clone::{AckRecord, InboundRecord, NetObjEntityType, OutboundClone, ENTITY_TYPE_BITS};
pub use packet::{FrameIndex, IncomingPacket};
