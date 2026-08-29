use super::*;

#[test]
fn parses_minimal_config() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert_eq!(config.server.port, 30120);
    assert!(!config.dev.auth_bypass);
    assert!(config.auth.pubkey_url.contains("lambda.fivem.net"));
    assert_eq!(config.connection.deferral_timeout_secs, 10);
    assert_eq!(config.resources.path, PathBuf::from("resources"));
}

#[test]
fn voice_defaults_off_on_game_port_plus_one() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert!(!config.voice.enabled);
    assert_eq!(config.voice.port, 30121);
    config.validate().expect("disabled voice is always valid");
}

#[test]
fn voice_section_parses_and_rejects_game_port_collision() {
    let config: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[voice]\nenabled = true\nport = 64738\n").unwrap();
    assert!(config.voice.enabled);
    assert_eq!(config.voice.port, 64738);
    config.validate().expect("distinct port is valid");

    let clash: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[voice]\nenabled = true\nport = 30120\n").unwrap();
    assert!(matches!(
        clash.validate(),
        Err(ConfigError::Invalid {
            section: "voice",
            ..
        })
    ));
}

#[test]
fn escrow_defaults_off_and_validates_trivially() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert!(!config.escrow.enabled);
    assert_eq!(config.escrow.backend, EscrowBackend::Sidecar);
    config
        .escrow
        .validate()
        .expect("disabled escrow is always valid");
}

#[test]
fn escrow_enabled_without_license_is_rejected() {
    let escrow = EscrowConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(matches!(
        escrow.validate(),
        Err(ConfigError::EscrowMissingLicense)
    ));
}

#[test]
fn escrow_sidecar_missing_fxserver_path_is_rejected() {
    let escrow = EscrowConfig {
        enabled: true,
        backend: EscrowBackend::Sidecar,
        server_license: "license:abc".into(),
        ..Default::default()
    };
    assert!(matches!(
        escrow.validate(),
        Err(ConfigError::EscrowMissingFxserverPath)
    ));
}

#[test]
fn escrow_unknown_backend_is_rejected_at_parse() {
    // With `backend` typed as an enum, serde rejects unknown values when the
    // TOML is parsed — no separate validation error variant needed.
    let parsed: Result<BastonConfig, _> =
        toml::from_str("[escrow]\nenabled = true\nbackend = \"carrier-pigeon\"\n");
    assert!(parsed.is_err(), "unknown backend must fail to parse");
}

#[test]
fn license_defaults_off_and_validates_trivially() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert_eq!(config.license.mode, LicenseMode::Off);
    config.license.validate().expect("off is always valid");
}

#[test]
fn license_gate_requires_a_key() {
    let lic = LicenseConfig {
        mode: LicenseMode::Gate,
        ..Default::default()
    };
    assert!(matches!(
        lic.validate(),
        Err(ConfigError::LicenseMissingKey(_))
    ));
}

#[test]
fn license_gate_rejects_placeholder_key() {
    let lic = LicenseConfig {
        mode: LicenseMode::Gate,
        sv_license_key: "cfxk_REPLACE_ME_please".into(),
        ..Default::default()
    };
    assert!(matches!(
        lic.validate(),
        Err(ConfigError::LicenseMalformedKey)
    ));
}

#[test]
fn license_gate_accepts_well_formed_key() {
    let lic = LicenseConfig {
        mode: LicenseMode::Gate,
        sv_license_key: "cfxk_1a2b3c4d5e6f7g8h9i0j_realkey".into(),
        ..Default::default()
    };
    lic.validate().expect("well-formed key passes the gate");
}

#[test]
fn license_config_debug_redacts_server_key() {
    let lic = LicenseConfig {
        sv_license_key: "cfxk_super_secret_server_key".into(),
        ..Default::default()
    };
    let debug = format!("{lic:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("cfxk_super_secret_server_key"));
}

#[test]
fn public_listing_defaults_off_on_any_interface() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert!(!config.license.public_listing);
    assert!(config.license.listing_ip_override.is_none());
    assert!(config.server.bind_address.is_unspecified());
}

#[test]
fn public_listing_requires_verified_mode() {
    let config: BastonConfig = toml::from_str(
        "[server]\nbind_address = \"192.0.2.10\"\n\
         [license]\npublic_listing = true\nlisting_ip_override = \"203.0.113.10\"\n",
    )
    .unwrap();
    assert!(matches!(
        config.validate(),
        Err(ConfigError::Invalid {
            section: "license",
            ..
        })
    ));
}

#[test]
fn public_listing_rejects_unspecified_gateway_bind() {
    let mut config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    config.license.mode = LicenseMode::Verified;
    config.license.sv_license_key = "cfxk_1a2b3c4d5e6f7g8h9i0j_realkey".into();
    config.license.fxserver_path = Some(std::env::current_exe().unwrap());
    config.license.public_listing = true;
    config.license.listing_ip_override = Some("203.0.113.10".parse().unwrap());

    assert!(matches!(
        config.validate(),
        Err(ConfigError::Invalid {
            section: "server",
            ..
        })
    ));
}

#[test]
fn public_listing_requires_udp_on_public_server_port() {
    let mut config: BastonConfig =
        toml::from_str("[server]\nport = 30120\nbind_address = \"192.0.2.10\"\n").unwrap();
    config.license.mode = LicenseMode::Verified;
    config.license.sv_license_key = "cfxk_1a2b3c4d5e6f7g8h9i0j_realkey".into();
    config.license.fxserver_path = Some(std::env::current_exe().unwrap());
    config.license.public_listing = true;
    config.license.listing_ip_override = Some("203.0.113.10".parse().unwrap());
    config.udp.port = Some(30121);

    assert!(matches!(
        config.validate(),
        Err(ConfigError::Invalid { section: "udp", .. })
    ));
}

#[test]
fn license_verified_requires_fxserver_path() {
    let lic = LicenseConfig {
        mode: LicenseMode::Verified,
        sv_license_key: "cfxk_1a2b3c4d5e6f7g8h9i0j_realkey".into(),
        fxserver_path: None,
        ..Default::default()
    };
    assert!(matches!(
        lic.validate(),
        Err(ConfigError::LicenseMissingFxserverPath)
    ));
}

#[test]
fn license_unknown_mode_is_rejected_at_parse() {
    // Enum-typed `mode`: serde rejects unknown values at parse time.
    let parsed: Result<BastonConfig, _> = toml::from_str("[license]\nmode = \"trust-me-bro\"\n");
    assert!(parsed.is_err(), "unknown licence mode must fail to parse");
}

const STRONG_TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn api_key(name: &str, token: &str) -> ApiKey {
    ApiKey {
        name: name.into(),
        token: token.into(),
        permissions: vec![ApiPermission::MonitorRead],
    }
}

#[test]
fn api_defaults_to_no_keys_and_validates() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert!(config.api.keys.is_empty());
    assert_eq!(config.api.audit_log, PathBuf::from("baston-audit.jsonl"));
    config.api.validate().expect("empty key list is valid");
}

#[test]
fn api_keys_parse_from_toml_with_dotted_permissions() {
    let config: BastonConfig = toml::from_str(
        "[server]\nport = 30120\n\
         [[api.keys]]\n\
         name = \"discord-bot\"\n\
         token = \"0123456789abcdef0123456789abcdef\"\n\
         permissions = [\"monitor.read\", \"player.kick\", \"console.execute\"]\n",
    )
    .unwrap();
    assert_eq!(config.api.keys.len(), 1);
    assert_eq!(
        config.api.keys[0].permissions,
        vec![
            ApiPermission::MonitorRead,
            ApiPermission::PlayerKick,
            ApiPermission::ConsoleExecute
        ]
    );
    config.api.validate().expect("well-formed key");
}

#[test]
fn api_unknown_permission_is_rejected_at_parse() {
    let parsed: Result<BastonConfig, _> = toml::from_str(
        "[[api.keys]]\nname = \"x\"\ntoken = \"0123456789abcdef0123456789abcdef\"\n\
         permissions = [\"root.everything\"]\n",
    );
    assert!(parsed.is_err(), "unknown permission must fail to parse");
}

#[test]
fn api_weak_token_is_rejected() {
    let api = ApiConfig {
        keys: vec![api_key("bot", "short")],
        ..Default::default()
    };
    assert!(matches!(
        api.validate(),
        Err(ConfigError::ApiKeyWeakToken(name)) if name == "bot"
    ));
}

#[test]
fn api_duplicate_names_and_tokens_are_rejected() {
    let api = ApiConfig {
        keys: vec![api_key("bot", STRONG_TOKEN), api_key("bot", STRONG_TOKEN)],
        ..Default::default()
    };
    assert!(matches!(
        api.validate(),
        Err(ConfigError::ApiKeyDuplicateName(_))
    ));

    let api = ApiConfig {
        keys: vec![api_key("bot", STRONG_TOKEN), api_key("panel", STRONG_TOKEN)],
        ..Default::default()
    };
    assert!(matches!(
        api.validate(),
        Err(ConfigError::ApiKeyDuplicateToken(name)) if name == "panel"
    ));
}

#[test]
fn api_key_without_permissions_is_rejected() {
    let mut key = api_key("bot", STRONG_TOKEN);
    key.permissions.clear();
    let api = ApiConfig {
        keys: vec![key],
        ..Default::default()
    };
    assert!(matches!(
        api.validate(),
        Err(ConfigError::ApiKeyNoPermissions(name)) if name == "bot"
    ));
}

#[test]
fn sync_and_download_defaults_are_backward_compatible() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert_eq!(config.state_sync.tick_min_hz, 20);
    assert_eq!(config.state_sync.tick_default_hz, 60);
    assert_eq!(config.state_sync.tick_max_hz, 120);
    assert_eq!(config.state_sync.interest_budget_bytes, 24 * 1024);
    assert_eq!(config.resources.file_download_timeout_secs, 30);
    assert_eq!(config.resources.file_download_chunk_bytes, 64 * 1024);
    assert_eq!(config.resources.file_download_concurrency, 64);
    config.validate().unwrap();
}

#[test]
fn sync_tick_bounds_and_thresholds_are_validated() {
    let mut sync = StateSyncConfig {
        tick_max_hz: 121,
        ..Default::default()
    };
    assert!(matches!(
        sync.validate(),
        Err(ConfigError::Invalid {
            section: "state_sync",
            ..
        })
    ));

    sync.tick_max_hz = 120;
    sync.tick_low_utilization = 0.9;
    sync.tick_high_utilization = 0.8;
    assert!(sync.validate().is_err());
}

#[test]
fn download_policy_rejects_zero_and_unsafe_chunk_sizes() {
    let resources = ResourcesConfig {
        file_download_concurrency: 0,
        ..Default::default()
    };
    assert!(matches!(
        resources.validate(),
        Err(ConfigError::Invalid {
            section: "resources",
            ..
        })
    ));
}
