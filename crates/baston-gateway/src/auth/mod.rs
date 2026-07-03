//! CFX authentication service (jalon B1).
//!
//! Reproduces FXServer's `initConnect` ticket authorization
//! (`components/citizen-server-impl/src/InitConnectMethod.cpp`): the
//! `cfxTicket2` field is verified *offline* against an RSA public key fetched
//! once from the CNL endpoint (`https://lambda.fivem.net/api/ticket/pubkey`),
//! with expiry, GUID and anti-reuse checks. There is no per-connection call
//! to CFX servers.

mod ticket;

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use baston_config::AuthConfig;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::RsaPublicKey;

pub use ticket::TicketData;

/// Ticket validation / public key retrieval failures. Messages mirror
/// FXServer's `GetVerifyTicketErrorString` where a client-visible reason exists.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("No authentication ticket was specified.")]
    MissingTicket,
    #[error("Ticket authorization failed: {0}")]
    MalformedTicket(&'static str),
    #[error("Ticket expired. Please check your server's system time.")]
    TicketExpired,
    #[error("Mismatching GUID.")]
    GuidMismatch,
    #[error("Reused ticket.")]
    TicketReused,
    #[error("Invalid ticket signature.")]
    InvalidSignature,
    #[error("public key request failed: {0}")]
    PubkeyFetch(String),
    #[error("public key parse failed: {0}")]
    PubkeyParse(String),
}

/// Player identity extracted from a verified ticket.
#[derive(Debug, Clone)]
pub struct ValidatedPlayer {
    /// `license:<hex entitlement hash>` — the primary CFX identifier.
    pub license: String,
    /// `steam:<id>` when present among the ticket's `tk` identifiers.
    pub steam: Option<String>,
    pub ip: String,
    /// Every identifier (`license:`, `steam:`, `discord:`, `ip:`, ...).
    pub identifiers: Vec<String>,
}

/// How long a fetched public key is trusted before a failed verification may
/// force a refresh (FXServer uses the same 5-minute window).
const PUBKEY_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// Anti-reuse set GC interval, matching FXServer's 30 minutes.
const TICKET_GC_INTERVAL: Duration = Duration::from_secs(30 * 60);

struct CachedPubkey {
    key: RsaPublicKey,
    fetched_at: Instant,
}

pub struct AuthService {
    http: reqwest::Client,
    pubkey_url: String,
    pubkey: tokio::sync::Mutex<Option<CachedPubkey>>,
    /// (expiry, guid) pairs already seen — replay protection.
    used_tickets: Mutex<UsedTickets>,
}

struct UsedTickets {
    seen: HashSet<(u64, u64)>,
    next_gc: Instant,
}

impl AuthService {
    pub fn new(config: &AuthConfig) -> Result<Self, AuthError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.http_timeout_secs))
            .build()
            .map_err(|e| AuthError::PubkeyFetch(e.to_string()))?;
        Ok(Self {
            http,
            pubkey_url: config.pubkey_url.clone(),
            pubkey: tokio::sync::Mutex::new(None),
            used_tickets: Mutex::new(UsedTickets {
                seen: HashSet::new(),
                next_gc: Instant::now() + TICKET_GC_INTERVAL,
            }),
        })
    }

    /// Validate an `initConnect` ticket and derive the player's identifiers.
    ///
    /// `guid` is the decimal string from the `guid` POST field; `ip` is the
    /// peer address used for the `ip:` identifier.
    pub async fn validate_ticket(
        &self,
        guid: &str,
        ticket_b64: &str,
        ip: &str,
    ) -> Result<ValidatedPlayer, AuthError> {
        // C++: strtoull(guid) — a non-numeric guid parses as 0 and will only
        // match a ticket whose embedded guid is also 0.
        let guid: u64 = guid.parse().unwrap_or(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let ticket = self.verify_with_key_refresh(ticket_b64, guid, now).await?;
        self.check_reuse(&ticket)?;
        Ok(build_player(&ticket, ip))
    }

    /// Verify the ticket; on a signature failure with a stale cached key,
    /// refresh the key once and retry (key-rotation handling, as FXServer).
    async fn verify_with_key_refresh(
        &self,
        ticket_b64: &str,
        guid: u64,
        now: u64,
    ) -> Result<TicketData, AuthError> {
        let key = self.get_pubkey(false).await?;
        match ticket::parse_and_verify(ticket_b64, guid, &key, now) {
            Err(AuthError::InvalidSignature) => {
                let key = self.get_pubkey(true).await?;
                ticket::parse_and_verify(ticket_b64, guid, &key, now)
            }
            other => other,
        }
    }

    fn check_reuse(&self, ticket: &TicketData) -> Result<(), AuthError> {
        let mut used = self
            .used_tickets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        if now > used.next_gc {
            used.seen.clear();
            used.next_gc = now + TICKET_GC_INTERVAL;
        }
        if !used.seen.insert((ticket.expiry_unix_secs, ticket.guid)) {
            return Err(AuthError::TicketReused);
        }
        Ok(())
    }

    /// Return the cached CFX public key, fetching it when absent. With
    /// `force_refresh`, discard a key older than 5 minutes first.
    async fn get_pubkey(&self, force_refresh: bool) -> Result<RsaPublicKey, AuthError> {
        let mut cached = self.pubkey.lock().await;
        if let Some(entry) = cached.as_ref() {
            let expired = force_refresh && entry.fetched_at.elapsed() > PUBKEY_MAX_AGE;
            if !expired {
                return Ok(entry.key.clone());
            }
        }

        let key = self.fetch_pubkey().await?;
        *cached = Some(CachedPubkey {
            key: key.clone(),
            fetched_at: Instant::now(),
        });
        Ok(key)
    }

    async fn fetch_pubkey(&self) -> Result<RsaPublicKey, AuthError> {
        #[derive(serde::Deserialize)]
        struct PubkeyResponse {
            key: String,
        }

        let response: PubkeyResponse = self
            .http
            .get(&self.pubkey_url)
            .send()
            .await
            .map_err(|e| AuthError::PubkeyFetch(e.to_string()))?
            .error_for_status()
            .map_err(|e| AuthError::PubkeyFetch(e.to_string()))?
            .json()
            .await
            .map_err(|e| AuthError::PubkeyFetch(e.to_string()))?;

        parse_pubkey(&response.key)
    }
}

/// The endpoint returns base64 of a DER `SEQUENCE { n INTEGER, e INTEGER }`,
/// which is exactly a PKCS#1 `RSAPublicKey`.
fn parse_pubkey(key_b64: &str) -> Result<RsaPublicKey, AuthError> {
    use base64::Engine;
    let der = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .map_err(|e| AuthError::PubkeyParse(format!("base64: {e}")))?;
    RsaPublicKey::from_pkcs1_der(&der).map_err(|e| AuthError::PubkeyParse(e.to_string()))
}

fn build_player(ticket: &TicketData, ip: &str) -> ValidatedPlayer {
    let mut identifiers = Vec::new();

    // LicenseIdentityProvider.cpp: license = hex(entitlementHash)
    let license = ticket
        .entitlement_hash
        .map(|h| format!("license:{}", hex::encode(h)))
        .unwrap_or_default();
    if !license.is_empty() {
        identifiers.push(license.clone());
    }

    // extraJson "tk" array carries additional identifiers (steam:, discord:, ...)
    if let Some(json) = ticket
        .extra_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
    {
        if let Some(tk) = json.get("tk").and_then(|v| v.as_array()) {
            for entry in tk.iter().filter_map(|v| v.as_str()) {
                identifiers.push(entry.to_owned());
            }
        }
    }

    identifiers.push(format!("ip:{ip}"));

    let steam = identifiers
        .iter()
        .find(|id| id.starts_with("steam:"))
        .cloned();

    ValidatedPlayer {
        license,
        steam,
        ip: ip.to_owned(),
        identifiers,
    }
}

#[cfg(test)]
mod tests {
    use super::ticket::test_support::*;
    use super::*;

    fn service() -> AuthService {
        AuthService::new(&AuthConfig::default()).expect("service")
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    async fn service_with_key(key: RsaPublicKey) -> AuthService {
        let svc = service();
        *svc.pubkey.lock().await = Some(CachedPubkey {
            key,
            fetched_at: Instant::now(),
        });
        svc
    }

    #[tokio::test]
    async fn validates_ticket_and_builds_identifiers() {
        let (private, public) = generate_keypair();
        let svc = service_with_key(public).await;
        let json = r#"{"gn":"gta5","tk":["steam:76561198000000000","discord:1234"]}"#;
        let ticket = build_ticket(&private, 42, now() + 3600, [0xCD; 20], json);

        let player = svc
            .validate_ticket("42", &ticket, "127.0.0.1")
            .await
            .expect("valid");
        assert_eq!(player.license, format!("license:{}", "cd".repeat(20)));
        assert_eq!(player.steam.as_deref(), Some("steam:76561198000000000"));
        assert!(player.identifiers.contains(&"discord:1234".to_owned()));
        assert!(player.identifiers.contains(&"ip:127.0.0.1".to_owned()));
    }

    #[tokio::test]
    async fn replayed_ticket_rejected() {
        let (private, public) = generate_keypair();
        let svc = service_with_key(public).await;
        let ticket = build_ticket(&private, 7, now() + 3600, [0x11; 20], "{}");

        svc.validate_ticket("7", &ticket, "127.0.0.1")
            .await
            .expect("first use ok");
        assert!(matches!(
            svc.validate_ticket("7", &ticket, "127.0.0.1").await,
            Err(AuthError::TicketReused)
        ));
    }

    #[test]
    fn parses_pkcs1_der_pubkey() {
        use base64::Engine;
        use rsa::pkcs1::EncodeRsaPublicKey;
        let (_, public) = generate_keypair();
        let der = public.to_pkcs1_der().unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_bytes());
        let parsed = parse_pubkey(&b64).expect("parse");
        assert_eq!(parsed, public);
    }
}
