//! Errors from the CFX platform exchanges.
//!
//! Every variant names what an operator should do about it, because all of
//! them surface at boot and stop the server.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CfxError {
    #[error("could not build the CFX HTTP client: {0}")]
    ClientBuild(String),

    #[error(
        "the CFX key-validation request failed: {0}\n  \
         → check outbound HTTPS to portal-api.cfx.re\n  \
         → to run without CFX identity: set [license] mode = \"off\""
    )]
    ValidateRequest(String),

    /// CFX answered, and the answer was a refusal.
    ///
    /// Kept distinct from a transport failure because the two mean opposite
    /// things: this one is CFX's decision and must be honoured, not retried.
    #[error(
        "CFX refused the key-validation request (HTTP {status})\n  \
         → BASTON identifies itself as `{user_agent}` and does not impersonate FXServer\n  \
         → a 401/403 here is CFX declining this client; that decision stands\n  \
         → set [license] mode = \"off\" to run without CFX identity"
    )]
    ValidateRefused { status: u16, user_agent: String },

    #[error("the CFX key-validation response was not valid JSON: {0}")]
    ValidateDecode(String),

    #[error(
        "CFX reported the licence key as not valid\n  \
         → check the key at https://portal.cfx.re\n  \
         → a revoked or mistyped key fails here, exactly as it would on FXServer"
    )]
    KeyRejected,

    #[error(
        "CFX validated the key but returned no {0}\n  \
         → BASTON cannot act as this server without it; this is a platform-side \
         response change, please report it"
    )]
    MissingCredential(&'static str),

    #[error(
        "the CFX policy lookup failed: {0}\n  \
         → BASTON refuses to boot rather than guess an entitlement it could not read"
    )]
    PolicyUnavailable(String),

    #[error("the CFX policy response was not a list of strings")]
    PolicyDecode,

    #[error("the CFX policy response exceeded {0} bytes")]
    PolicyTooLarge(usize),

    #[error(
        "server-list registration requires [listing] ip_override\n  \
         → set it to the public address players connect to, e.g. \"203.0.113.10\""
    )]
    ListingAddressMissing,
}
