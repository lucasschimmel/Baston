use std::time::Duration;

use baston_core::license::{Entitlements, LicenseKeyToken};

use crate::CfxPlatformError;

const POLICY_ENDPOINT: &str = "https://policy-live.fivem.net/api/policy";
const BASE_MAX_SLOTS: u32 = 48;
const MAX_POLICY_RESPONSE_BYTES: usize = 64 * 1024;

/// Origin of the effective policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySource {
    /// Entitlements came from a successful official CFX policy response.
    Cfx,
    /// Policy lookup failed, so only the base slot tier is retained.
    ConservativeFallback,
}

/// Entitlements resolved for one authenticated CFX identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyResolution {
    /// Effective slot ceiling and opaque feature names.
    pub entitlements: Entitlements,
    /// Whether the decision is authoritative or conservative.
    pub source: PolicySource,
}

/// Client for CFX's public policy endpoint.
#[derive(Clone)]
pub struct PolicyClient {
    client: reqwest::Client,
    endpoint: reqwest::Url,
}

impl PolicyClient {
    /// Build a hardened client for the official CFX policy endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client or fixed endpoint cannot be built.
    pub fn new() -> Result<Self, CfxPlatformError> {
        Self::with_endpoint(POLICY_ENDPOINT)
    }

    fn with_endpoint(endpoint: &str) -> Result<Self, CfxPlatformError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CfxPlatformError::PolicyClientBuild)?;
        let endpoint =
            reqwest::Url::parse(endpoint).map_err(|_| CfxPlatformError::PolicyEndpoint)?;
        Ok(Self { client, endpoint })
    }

    /// Fetch the authoritative policy list.
    ///
    /// Errors are intentionally context-free because Reqwest errors may embed
    /// the request URL, whose final path segment is the secret token.
    ///
    /// # Errors
    ///
    /// Returns an error for transport failures, non-success statuses,
    /// oversized bodies, or malformed policy lists.
    pub async fn fetch(&self, token: &LicenseKeyToken) -> Result<Entitlements, CfxPlatformError> {
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| CfxPlatformError::PolicyEndpoint)?
            .push(token.as_str());

        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| CfxPlatformError::PolicyRequest)?;
        if !response.status().is_success() {
            return Err(CfxPlatformError::PolicyHttpStatus(
                response.status().as_u16(),
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| CfxPlatformError::PolicyRequest)?
        {
            if body.len().saturating_add(chunk.len()) > MAX_POLICY_RESPONSE_BYTES {
                return Err(CfxPlatformError::PolicyResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        let policies = serde_json::from_slice::<Vec<String>>(&body)
            .map_err(|_| CfxPlatformError::PolicyDecode)?;
        Ok(entitlements_from_policies(policies))
    }

    /// Resolve policies without ever granting paid capabilities on failure.
    ///
    /// This method does not return an error: failures produce a
    /// [`PolicySource::ConservativeFallback`] resolution capped to the base
    /// slot tier.
    pub async fn resolve(&self, token: &LicenseKeyToken) -> PolicyResolution {
        match self.fetch(token).await {
            Ok(entitlements) => PolicyResolution {
                entitlements,
                source: PolicySource::Cfx,
            },
            Err(error) => {
                tracing::warn!(
                    target: "cfx_platform",
                    error = %error,
                    "CFX policy lookup failed; applying the conservative base slot cap"
                );
                PolicyResolution {
                    entitlements: entitlements_from_policies(Vec::new()),
                    source: PolicySource::ConservativeFallback,
                }
            }
        }
    }
}

fn entitlements_from_policies(policies: Vec<String>) -> Entitlements {
    let grants = |name: &str| policies.iter().any(|policy| policy == name);
    let max_slots = if grants("onesync_big") {
        2_048
    } else if grants("onesync_plus") || grants("onesync_medium") {
        128
    } else if grants("onesync") {
        64
    } else {
        BASE_MAX_SLOTS
    };

    Entitlements {
        max_slots: Some(max_slots),
        features: policies,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn policies(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn serve_once(response: Vec<u8>) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            stream.write_all(&response).unwrap();
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        (format!("http://{address}/api/policy"), handle)
    }

    #[test]
    fn slot_tiers_match_cfx_client_policy_contract() {
        assert_eq!(
            entitlements_from_policies(policies(&[])).max_slots,
            Some(48)
        );
        assert_eq!(
            entitlements_from_policies(policies(&["onesync"])).max_slots,
            Some(64)
        );
        assert_eq!(
            entitlements_from_policies(policies(&["onesync_plus"])).max_slots,
            Some(128)
        );
        assert_eq!(
            entitlements_from_policies(policies(&["onesync_medium"])).max_slots,
            Some(128)
        );
        assert_eq!(
            entitlements_from_policies(policies(&["onesync_big"])).max_slots,
            Some(2_048)
        );
    }

    #[test]
    fn unknown_policies_are_preserved_without_unlocking_slots() {
        let entitlements = entitlements_from_policies(policies(&["custom_policy"]));
        assert_eq!(entitlements.max_slots, Some(48));
        assert_eq!(entitlements.features, vec!["custom_policy"]);
    }

    #[tokio::test]
    async fn network_failure_is_secret_safe_and_falls_back_to_base_slots() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let client = PolicyClient::with_endpoint(&format!("http://{address}/api/policy")).unwrap();
        let token = LicenseKeyToken::new("secret-policy-token").unwrap();

        let error = client.fetch(&token).await.unwrap_err();
        assert!(!error.to_string().contains(token.as_str()));

        let resolution = client.resolve(&token).await;
        assert_eq!(resolution.source, PolicySource::ConservativeFallback);
        assert_eq!(resolution.entitlements.max_slots, Some(48));
        assert!(!format!("{resolution:?}").contains(token.as_str()));
    }

    #[tokio::test]
    async fn fetch_decodes_success_and_percent_encodes_token_path_segment() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n[\"onesync\"]"
                .to_vec();
        let (endpoint, request) = serve_once(response);
        let client = PolicyClient::with_endpoint(&endpoint).unwrap();
        let token = LicenseKeyToken::new("a/b?c").unwrap();

        let entitlements = client.fetch(&token).await.unwrap();

        assert_eq!(entitlements.max_slots, Some(64));
        assert!(request
            .join()
            .unwrap()
            .starts_with("GET /api/policy/a%2Fb%3Fc HTTP/1.1"));
    }

    #[tokio::test]
    async fn redirect_is_not_followed() {
        let response =
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec();
        let (endpoint, request) = serve_once(response);
        let client = PolicyClient::with_endpoint(&endpoint).unwrap();
        let token = LicenseKeyToken::new("secret").unwrap();

        let error = client.fetch(&token).await.unwrap_err();

        assert!(matches!(error, CfxPlatformError::PolicyHttpStatus(302)));
        request.join().unwrap();
    }

    #[tokio::test]
    async fn oversized_response_is_rejected() {
        let body = vec![b'x'; MAX_POLICY_RESPONSE_BYTES + 1];
        let response = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes(),
            body,
        ]
        .concat();
        let (endpoint, request) = serve_once(response);
        let client = PolicyClient::with_endpoint(&endpoint).unwrap();
        let token = LicenseKeyToken::new("secret").unwrap();

        let error = client.fetch(&token).await.unwrap_err();

        assert!(matches!(error, CfxPlatformError::PolicyResponseTooLarge));
        request.join().unwrap();
    }
}
