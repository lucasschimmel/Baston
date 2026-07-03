//! CFX `cfxTicket2` parsing and offline RSA verification.
//!
//! Byte layout and verification scheme extracted from FXServer
//! `components/citizen-server-impl/src/InitConnectMethod.cpp`
//! (`VerifyTicket` / `VerifyTicketEx`):
//!
//! ```text
//! offset        size  field
//! 0             4     u32 inner length (must be 16)
//! 4             8     u64 guid (LE)
//! 12            8     u64 expiry, unix seconds (LE)
//! 20            4     u32 signature #1 length (must be 128)
//! 24            128   RSA sig #1 over bytes[4..20]
//! 152           4     u32 extra-data length N
//! 156           N     extra data: [0..20) entitlement hash (SHA1),
//!                     [20..24) u32 json length M, [24..24+M) JSON
//! 156+N         4     u32 signature #2 length (must be 128)
//! 160+N         128   RSA sig #2 over bytes[4..156+N)
//! ```
//!
//! Signatures are RSA PKCS#1 v1.5. Botan verifies with
//! `EMSA_PKCS1(SHA-1)` over the *message* `0x02 || SHA1(payload)`, i.e. the
//! digest actually signed is `SHA1(0x02 || SHA1(payload))`.

use base64::Engine;
use rsa::{Pkcs1v15Sign, RsaPublicKey};
use sha1::{Digest, Sha1};

use super::AuthError;

const INNER_LENGTH: usize = 16;
const SIG_LENGTH: usize = 128;
/// header (4) + inner (16) + sig-len (4) + sig (128)
const BASE_LENGTH: usize = 4 + INNER_LENGTH + 4 + SIG_LENGTH;

/// Extra payload carried by the ticket (entitlement hash → `license:`,
/// JSON → `tk`/`hw`/`gn`/`si` fields).
#[derive(Debug)]
pub struct TicketData {
    pub guid: u64,
    pub expiry_unix_secs: u64,
    /// 20-byte entitlement hash; hex-encoded it becomes the `license:` identifier.
    pub entitlement_hash: Option<[u8; 20]>,
    pub extra_json: Option<String>,
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    // callers bound-check before slicing
    u32::from_le_bytes(data[offset..offset + 4].try_into().expect("4-byte slice"))
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().expect("8-byte slice"))
}

/// The digest FXServer's Botan `EMSA_PKCS1(SHA-1)` verifier ends up checking:
/// `SHA1(0x02 || SHA1(payload))`.
pub(super) fn signed_digest(payload: &[u8]) -> [u8; 20] {
    let inner = Sha1::digest(payload);
    let mut msg = Vec::with_capacity(21);
    msg.push(2u8);
    msg.extend_from_slice(&inner);
    Sha1::digest(&msg).into()
}

fn verify_sig(pubkey: &RsaPublicKey, payload: &[u8], sig: &[u8]) -> Result<(), AuthError> {
    pubkey
        .verify(Pkcs1v15Sign::new::<Sha1>(), &signed_digest(payload), sig)
        .map_err(|_| AuthError::InvalidSignature)
}

/// Parse and fully verify a base64 `cfxTicket2` against the CFX public key.
/// Mirrors `VerifyTicket` + `VerifyTicketEx`; `now_unix_secs` is injected for
/// testability.
pub fn parse_and_verify(
    ticket_b64: &str,
    guid: u64,
    pubkey: &RsaPublicKey,
    now_unix_secs: u64,
) -> Result<TicketData, AuthError> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(ticket_b64.trim())
        .map_err(|_| AuthError::MalformedTicket("invalid base64"))?;

    if data.len() < BASE_LENGTH + 4 {
        return Err(AuthError::MalformedTicket("ticket too short"));
    }
    if read_u32(&data, 0) as usize != INNER_LENGTH {
        return Err(AuthError::MalformedTicket("invalid inner length"));
    }

    let ticket_guid = read_u64(&data, 4);
    let expiry = read_u64(&data, 12);

    if expiry < now_unix_secs {
        return Err(AuthError::TicketExpired);
    }
    if ticket_guid != guid {
        return Err(AuthError::GuidMismatch);
    }

    // signature #1: over the 16-byte inner block (guid + expiry)
    if read_u32(&data, 20) as usize != SIG_LENGTH {
        return Err(AuthError::MalformedTicket("invalid signature #1 length"));
    }
    verify_sig(pubkey, &data[4..20], &data[24..24 + SIG_LENGTH])?;

    // extended block (VerifyTicketEx)
    let extra_len = read_u32(&data, BASE_LENGTH) as usize;
    let extra_start = BASE_LENGTH + 4;
    let sig2_len_off = extra_start + extra_len;
    if data.len() < sig2_len_off + 4 + SIG_LENGTH {
        return Err(AuthError::MalformedTicket("truncated extra block"));
    }
    if read_u32(&data, sig2_len_off) as usize != SIG_LENGTH {
        return Err(AuthError::MalformedTicket("invalid signature #2 length"));
    }
    // signature #2 covers everything from the inner block through the extra data
    verify_sig(
        pubkey,
        &data[4..sig2_len_off],
        &data[sig2_len_off + 4..sig2_len_off + 4 + SIG_LENGTH],
    )?;

    let extra = &data[extra_start..sig2_len_off];
    let entitlement_hash = if extra.len() >= 20 {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&extra[..20]);
        Some(hash)
    } else {
        None
    };

    let extra_json = if extra.len() >= 24 {
        let json_len = read_u32(extra, 20) as usize;
        if extra.len() >= 24 + json_len {
            Some(String::from_utf8_lossy(&extra[24..24 + json_len]).into_owned())
        } else {
            None
        }
    } else {
        None
    };

    Ok(TicketData {
        guid: ticket_guid,
        expiry_unix_secs: expiry,
        entitlement_hash,
        extra_json,
    })
}

#[cfg(test)]
pub(super) mod test_support {
    //! Builds validly-signed tickets with a locally generated 1024-bit key
    //! (sig length 128, matching the CFX key size).

    use rsa::{RsaPrivateKey, RsaPublicKey};

    use super::*;

    pub fn generate_keypair() -> (RsaPrivateKey, RsaPublicKey) {
        let mut rng = rsa::rand_core::OsRng;
        let private = RsaPrivateKey::new(&mut rng, 1024).expect("keygen");
        let public = RsaPublicKey::from(&private);
        (private, public)
    }

    fn sign(private: &RsaPrivateKey, payload: &[u8]) -> Vec<u8> {
        private
            .sign(Pkcs1v15Sign::new::<Sha1>(), &signed_digest(payload))
            .expect("sign")
    }

    pub fn build_ticket(
        private: &RsaPrivateKey,
        guid: u64,
        expiry: u64,
        entitlement_hash: [u8; 20],
        extra_json: &str,
    ) -> String {
        let mut extra = Vec::new();
        extra.extend_from_slice(&entitlement_hash);
        extra.extend_from_slice(&(extra_json.len() as u32).to_le_bytes());
        extra.extend_from_slice(extra_json.as_bytes());

        let mut data = Vec::new();
        data.extend_from_slice(&(INNER_LENGTH as u32).to_le_bytes());
        data.extend_from_slice(&guid.to_le_bytes());
        data.extend_from_slice(&expiry.to_le_bytes());
        let sig1 = sign(private, &data[4..20]);
        data.extend_from_slice(&(sig1.len() as u32).to_le_bytes());
        data.extend_from_slice(&sig1);
        data.extend_from_slice(&(extra.len() as u32).to_le_bytes());
        data.extend_from_slice(&extra);
        let sig2 = sign(private, &data[4..]);
        data.extend_from_slice(&(sig2.len() as u32).to_le_bytes());
        data.extend_from_slice(&sig2);

        base64::engine::general_purpose::STANDARD.encode(data)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const HASH: [u8; 20] = [0xAB; 20];

    #[test]
    fn valid_ticket_roundtrip() {
        let (private, public) = generate_keypair();
        let json = r#"{"gn":"gta5","tk":["steam:76561198000000000"]}"#;
        let ticket = build_ticket(&private, 42, NOW + 3600, HASH, json);

        let parsed = parse_and_verify(&ticket, 42, &public, NOW).expect("valid ticket");
        assert_eq!(parsed.guid, 42);
        assert_eq!(parsed.entitlement_hash, Some(HASH));
        assert_eq!(parsed.extra_json.as_deref(), Some(json));
    }

    #[test]
    fn expired_ticket_rejected() {
        let (private, public) = generate_keypair();
        let ticket = build_ticket(&private, 42, NOW - 1, HASH, "{}");
        assert!(matches!(
            parse_and_verify(&ticket, 42, &public, NOW),
            Err(AuthError::TicketExpired)
        ));
    }

    #[test]
    fn guid_mismatch_rejected() {
        let (private, public) = generate_keypair();
        let ticket = build_ticket(&private, 42, NOW + 3600, HASH, "{}");
        assert!(matches!(
            parse_and_verify(&ticket, 43, &public, NOW),
            Err(AuthError::GuidMismatch)
        ));
    }

    #[test]
    fn wrong_key_rejected() {
        let (private, _) = generate_keypair();
        let (_, other_public) = generate_keypair();
        let ticket = build_ticket(&private, 42, NOW + 3600, HASH, "{}");
        assert!(matches!(
            parse_and_verify(&ticket, 42, &other_public, NOW),
            Err(AuthError::InvalidSignature)
        ));
    }

    #[test]
    fn garbage_rejected_cleanly() {
        let (_, public) = generate_keypair();
        assert!(parse_and_verify("not-base64!!!", 1, &public, NOW).is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 8]);
        assert!(matches!(
            parse_and_verify(&short, 1, &public, NOW),
            Err(AuthError::MalformedTicket(_))
        ));
    }
}
