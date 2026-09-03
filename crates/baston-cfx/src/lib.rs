//! `baston-cfx` — CFX platform identity, without FXServer.
//!
//! BASTON performs the same three public exchanges FXServer performs:
//!
//! 1. **Key validation** — `portal-api.cfx.re`, returning the licence token
//!    the client reads plus the nucleus and listing credentials.
//! 2. **Nucleus registration** — `cfx.re/api/register`, returning the
//!    `users.cfx.re` hostname.
//! 3. **Server-list ingress** — `servers-frontend.fivem.net`, the heartbeat
//!    that keeps the server in the list.
//!
//! Steps 2 and 3 are transcribed from the public tree. Step 1 is the one
//! FXServer performs inside the closed `svadhesive` component, and it is an
//! ordinary HTTPS GET carrying the operator's own key in the path — no client
//! secret, no device binding, no challenge.
//!
//! # Two rules this crate is built around
//!
//! **BASTON identifies itself as BASTON.** [`user_agent`] never claims to be
//! FXServer. A refusal from CFX is an answer, and the caller falls back to
//! running without a CFX identity — see [`CfxError::ValidateRefused`].
//!
//! **A licence may lower a limit, never raise one.** The entitlements come
//! from the same endpoint the client checks, and are applied *before* any
//! listener opens. An unreadable policy grants nothing and stops the boot
//! rather than producing a guess. See [`policy`].
//!
//! Asset Escrow is out of scope permanently: it lives entirely inside
//! `svadhesive`, and reaching it means defeating a DRM mechanism rather than
//! speaking a protocol.

mod error;
mod identity;
mod listing;
mod policy;
mod secret;

pub use error::CfxError;
pub use identity::{authenticate, user_agent, CfxIdentity};
pub use listing::{Listing, PublicAddress, ServerSnapshot, HEARTBEAT_INTERVAL};
pub use policy::{decide_slots, PolicySet, SlotDecision};
