//! Player registry — a thin alias over the shared [`baston_protocol::PlayerDirectory`],
//! which the gateway owns (source allocation, insert/remove) and the script
//! runtimes read (player natives).

pub use baston_protocol::PlayerDirectory as PlayerRegistry;
