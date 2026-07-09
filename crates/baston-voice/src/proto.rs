//! Generated Mumble protobuf types (`MumbleProto` package).
//!
//! The wire format is the stock Mumble control protocol — the FiveM client
//! embeds an unmodified Mumble client, so these messages are a hard contract.
//! We regenerate them from `proto/Mumble.proto` (copied verbatim from the
//! Mumble project) rather than hand-rolling structs.
#![allow(clippy::all, missing_docs)]

// Include the prost-generated module directly (no tonic runtime dependency —
// this crate carries no gRPC service, only messages).
include!(concat!(env!("OUT_DIR"), "/mumble_proto.rs"));
