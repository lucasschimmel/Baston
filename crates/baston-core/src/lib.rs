//! `baston-core` — cross-crate abstractions shared by the zone runtime and its
//! optional plugins.
//!
//! Currently this is the [`script_decryptor`] boundary: the zone reads raw
//! script bytes and hands them to a [`script_decryptor::ScriptDecryptor`]
//! before feeding them to the V8 isolate. The default implementation is a
//! no-op ([`script_decryptor::PlainDecryptor`]); the optional
//! `baston-escrow-plugin` supplies a decrypting one for CFX Asset Escrow
//! resources. The core never depends on the plugin.

pub mod script_decryptor;
