//! Per-key bearer authentication for the `/api/v1` surface.
//!
//! Keys come from `[[api.keys]]` in `baston.toml`; the legacy
//! `meshing.admin_token` is folded in as an implicit full-permission key so
//! existing setups keep working. Fail-closed: no keys → every request denied.

use axum::http::HeaderMap;
use baston_config::{ApiConfig, ApiPermission};
use subtle::ConstantTimeEq;

const ALL_PERMISSIONS: [ApiPermission; 6] = [
    ApiPermission::MonitorRead,
    ApiPermission::ResourceControl,
    ApiPermission::PlayerKick,
    ApiPermission::ZoneDrain,
    ApiPermission::ProfilerControl,
    ApiPermission::ProfilerRead,
];

/// Why a request was rejected. Distinguished so handlers can answer 401
/// (unknown token) vs 403 (known key, missing permission) — and so the audit
/// log records denied attempts by key name.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// No/unknown bearer token.
    Unknown,
    /// Valid key, but it lacks the required permission.
    Forbidden { key_name: String },
}

struct KeyEntry {
    name: String,
    token: String,
    permissions: Vec<ApiPermission>,
}

/// All configured API keys. Built once at startup.
pub struct KeyRing {
    keys: Vec<KeyEntry>,
}

impl KeyRing {
    /// `admin_token`, when non-empty, becomes the implicit `"admin"` key with
    /// every permission (legacy back-compat).
    pub fn from_config(api: &ApiConfig, admin_token: &str) -> Self {
        let mut keys: Vec<KeyEntry> = api
            .keys
            .iter()
            .map(|k| KeyEntry {
                name: k.name.clone(),
                token: k.token.trim().to_owned(),
                permissions: k.permissions.clone(),
            })
            .collect();
        if !admin_token.is_empty() {
            keys.push(KeyEntry {
                name: "admin".to_owned(),
                token: admin_token.to_owned(),
                permissions: ALL_PERMISSIONS.to_vec(),
            });
        }
        Self { keys }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Authorize a request for one permission. Returns the key name (for the
    /// audit log). Token comparison is constant-time per key so token bytes
    /// can't be recovered via response timing; every key is compared even
    /// after a match so timing doesn't reveal *which* key matched either.
    pub fn authorize(
        &self,
        headers: &HeaderMap,
        permission: ApiPermission,
    ) -> Result<String, AuthError> {
        let provided = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if provided.is_empty() {
            return Err(AuthError::Unknown);
        }

        let mut matched: Option<usize> = None;
        for (i, key) in self.keys.iter().enumerate() {
            if bool::from(provided.as_bytes().ct_eq(key.token.as_bytes())) {
                matched.get_or_insert(i);
            }
        }
        let Some(i) = matched else {
            return Err(AuthError::Unknown);
        };
        let key = &self.keys[i];
        if key.permissions.contains(&permission) {
            Ok(key.name.clone())
        } else {
            Err(AuthError::Forbidden {
                key_name: key.name.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_config::ApiKey;

    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    fn ring() -> KeyRing {
        KeyRing::from_config(
            &ApiConfig {
                keys: vec![ApiKey {
                    name: "monitor-bot".into(),
                    token: "monitor-token-0123456789abcdef01".into(),
                    permissions: vec![ApiPermission::MonitorRead],
                }],
                ..Default::default()
            },
            "legacy-admin-token",
        )
    }

    #[test]
    fn key_permission_granted_and_denied() {
        let ring = ring();
        assert_eq!(
            ring.authorize(
                &headers("monitor-token-0123456789abcdef01"),
                ApiPermission::MonitorRead
            ),
            Ok("monitor-bot".to_owned())
        );
        assert_eq!(
            ring.authorize(
                &headers("monitor-token-0123456789abcdef01"),
                ApiPermission::PlayerKick
            ),
            Err(AuthError::Forbidden {
                key_name: "monitor-bot".to_owned()
            })
        );
    }

    #[test]
    fn legacy_admin_token_has_all_permissions() {
        let ring = ring();
        for p in ALL_PERMISSIONS {
            assert_eq!(
                ring.authorize(&headers("legacy-admin-token"), p),
                Ok("admin".to_owned())
            );
        }
    }

    #[test]
    fn unknown_and_missing_tokens_are_rejected() {
        let ring = ring();
        assert_eq!(
            ring.authorize(&headers("wrong"), ApiPermission::MonitorRead),
            Err(AuthError::Unknown)
        );
        assert_eq!(
            ring.authorize(&HeaderMap::new(), ApiPermission::MonitorRead),
            Err(AuthError::Unknown)
        );
    }

    #[test]
    fn empty_ring_denies_everything() {
        let ring = KeyRing::from_config(&ApiConfig::default(), "");
        assert!(ring.is_empty());
        assert_eq!(
            ring.authorize(&headers("anything"), ApiPermission::MonitorRead),
            Err(AuthError::Unknown)
        );
    }
}
