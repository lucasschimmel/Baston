//! `baston-core` — cross-crate abstractions shared by the zone runtime and its
//! optional plugins.
//!
//! Currently this is the [`script_decryptor`] boundary: the zone reads raw
//! script bytes and hands them to a [`script_decryptor::ScriptDecryptor`]
//! before feeding them to the script runtime. The only implementation today is
//! the no-op [`script_decryptor::PlainDecryptor`], which passes plaintext
//! through and refuses a CFX-encrypted (escrow) file with an actionable error
//! rather than handing ciphertext to V8.
//!
//! The trait is deliberately kept without a second implementation: it is the
//! documented seam where escrow support returns, and the one place that knows
//! an `.fxap` payload is not loadable. See
//! `docs/adr/003-remove-the-fxserver-sidecar.md`.

pub mod script_decryptor;
