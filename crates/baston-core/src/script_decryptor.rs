//! Script decryption abstraction.
//!
//! The zone's resource loader reads the raw bytes of every server script and
//! passes them through a [`ScriptDecryptor`] before compiling them. Plain
//! (unencrypted) resources take the [`PlainDecryptor`] fast path unchanged.
//!
//! CFX Asset Escrow resources are **not supported**. The FXServer sidecar that
//! used to decrypt them was removed (ADR-003); this trait is the seam where
//! support would return, and until then an encrypted file is refused here with
//! an error the operator can act on.

use thiserror::Error;

/// Decrypts server scripts at load time.
///
/// Contract:
/// - Plain bytes → return them unchanged (`Ok`).
/// - Encrypted bytes + decryption succeeds → return the plaintext.
/// - Encrypted bytes + no support / failure → return `Err`.
///
/// Implementations must be cheap to share (`Arc<dyn ScriptDecryptor>`) and
/// safe to call concurrently.
pub trait ScriptDecryptor: Send + Sync + 'static {
    /// Attempt to decrypt `bytes` for `resource_name`/`file_path`.
    fn decrypt(
        &self,
        resource_name: &str,
        file_path: &str,
        bytes: &[u8],
        entitlement: Option<&EntitlementContext>,
    ) -> Result<Vec<u8>, DecryptError>;

    /// Whether this decryptor can handle CFX-encrypted files. `false` for
    /// [`PlainDecryptor`], which is the only implementation today; the loader
    /// uses it only for diagnostics.
    fn supports_encrypted(&self) -> bool {
        false
    }
}

/// Entitlement material available when a resource is loaded.
///
/// CFX escrow is tied to the **server** licence (the owner bought the
/// resource), not to connected players. The `entitlement_hash` here identifies
/// the rights of the server that generated the CFX ticket.
#[derive(Debug, Clone)]
pub struct EntitlementContext {
    /// SHA-1 hex (40 chars) extracted from the CFX ticket.
    pub entitlement_hash: String,
    /// Extra ticket JSON (`tk`, `hw` fields), if present.
    pub entitlement_json: Option<serde_json::Value>,
}

/// Errors surfaced by a [`ScriptDecryptor`].
#[derive(Debug, Error)]
pub enum DecryptError {
    #[error(
        "file is CFX-encrypted (escrow) and BASTON cannot decrypt it — escrow is not \
         supported; use an unescrowed build of the resource"
    )]
    NoDecryptorAvailable,
    #[error("decryption failed for {resource}/{file}: {reason}")]
    DecryptionFailed {
        resource: String,
        file: String,
        reason: String,
    },
    #[error("entitlement missing or insufficient for {resource}")]
    EntitlementDenied { resource: String },
}

/// CFX Asset Escrow magic bytes.
///
/// Escrowed assets are "FX Asset Packs" (`.fxap`); encrypted scripts begin with
/// the ASCII marker `FXAP`. Derived from the official `.fxap` naming and
/// community reverse-engineering of the escrow format (support.cfx.re Asset
/// Escrow FAQ). To be re-validated against a real encrypted sample.
const CFX_MAGIC: &[u8] = b"FXAP";

/// Whether `bytes` look like a CFX-encrypted (escrow) file.
pub fn is_cfx_encrypted(bytes: &[u8]) -> bool {
    bytes.len() > CFX_MAGIC.len() && bytes.starts_with(CFX_MAGIC)
}

/// The only decryptor: passes plaintext through, refuses encrypted files.
///
/// This is the nominal path for plain resources and never panics or blocks.
pub struct PlainDecryptor;

impl ScriptDecryptor for PlainDecryptor {
    fn decrypt(
        &self,
        resource_name: &str,
        file_path: &str,
        bytes: &[u8],
        _entitlement: Option<&EntitlementContext>,
    ) -> Result<Vec<u8>, DecryptError> {
        if is_cfx_encrypted(bytes) {
            tracing::warn!(
                resource = resource_name,
                file = file_path,
                "file appears CFX-encrypted (CFX Asset Escrow) — BASTON has no escrow \
                 support, so this resource cannot run; ask its author for an \
                 unescrowed build or use a plain resource"
            );
            return Err(DecryptError::NoDecryptorAvailable);
        }
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cfx_magic() {
        let mut encrypted = Vec::from(CFX_MAGIC);
        encrypted.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        assert!(is_cfx_encrypted(&encrypted));
    }

    #[test]
    fn plain_javascript_is_not_encrypted() {
        assert!(!is_cfx_encrypted(b"console.log('hi')"));
    }

    #[test]
    fn magic_prefix_alone_is_not_treated_as_encrypted() {
        // Needs at least one payload byte beyond the marker.
        assert!(!is_cfx_encrypted(CFX_MAGIC));
    }

    #[test]
    fn plain_decryptor_passes_plaintext_through() {
        let bytes = b"console.log('axiom')";
        let out = PlainDecryptor
            .decrypt("axiom-core", "dist/server/index.js", bytes, None)
            .expect("plaintext must pass through");
        assert_eq!(out, bytes);
    }

    #[test]
    fn plain_decryptor_rejects_encrypted() {
        let mut encrypted = Vec::from(CFX_MAGIC);
        encrypted.extend_from_slice(b"ciphertext");
        let err = PlainDecryptor
            .decrypt("escrow-test", "dist/server/index.js", &encrypted, None)
            .expect_err("encrypted must be rejected");
        assert!(matches!(err, DecryptError::NoDecryptorAvailable));
    }

    #[test]
    fn plain_decryptor_does_not_support_encrypted() {
        assert!(!PlainDecryptor.supports_encrypted());
    }
}
