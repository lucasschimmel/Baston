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
fn license_unknown_mode_is_rejected_at_parse() {
    // Enum-typed `mode`: serde rejects unknown values at parse time.
    let parsed: Result<BastonConfig, _> = toml::from_str("[license]\nmode = \"trust-me-bro\"\n");
    assert!(parsed.is_err(), "unknown licence mode must fail to parse");
}

#[test]
fn the_removed_verified_mode_fails_loudly_instead_of_silently_downgrading() {
    // `verified` ran an FXServer sidecar that no longer exists. A config
    // carrying it must stop the operator rather than boot unauthenticated
    // while they believe CFX validated their key.
    let parsed: Result<BastonConfig, _> = toml::from_str("[license]\nmode = \"verified\"\n");
    assert!(parsed.is_err(), "verified mode must no longer parse");
}

#[test]
fn cfx_mode_needs_a_key_of_the_same_shape_gate_does() {
    let missing = LicenseConfig {
        mode: LicenseMode::Cfx,
        sv_license_key: String::new(),
    };
    assert!(matches!(
        missing.validate(),
        Err(ConfigError::LicenseMissingKey(ref m)) if m == "cfx"
    ));

    let placeholder = LicenseConfig {
        mode: LicenseMode::Cfx,
        sv_license_key: "cfxk_REPLACE_ME_please".into(),
    };
    assert!(matches!(
        placeholder.validate(),
        Err(ConfigError::LicenseMalformedKey)
    ));
}

#[test]
fn only_cfx_mode_claims_to_authenticate() {
    assert!(LicenseMode::Cfx.authenticates());
    assert!(!LicenseMode::Gate.authenticates());
    assert!(!LicenseMode::Off.authenticates());
}

#[test]
fn listing_defaults_off_and_needs_nothing() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert!(!config.listing.enabled);
    assert!(config.listing.ip_override.is_none());
    config
        .validate()
        .expect("a server that does not list is always valid");
}

#[test]
fn listing_without_an_authenticated_identity_is_rejected() {
    // The bargain: no token published means no client ever checks the slot
    // count, so a listing without `cfx` would be discoverable and unchecked.
    let config: BastonConfig = toml::from_str(
        "[server]\nport = 30120\n\
         [license]\nmode = \"gate\"\nsv_license_key = \"cfxk_1a2b3c4d5e6f7g8h9i0j_key\"\n\
         [listing]\nenabled = true\nip_override = \"203.0.113.10\"\n",
    )
    .unwrap();
    assert!(matches!(
        config.validate(),
        Err(ConfigError::Invalid {
            section: "listing",
            ..
        })
    ));
}

#[test]
fn a_fully_configured_listing_validates() {
    let config: BastonConfig = toml::from_str(
        "[server]\nport = 30120\n\
         [license]\nmode = \"cfx\"\nsv_license_key = \"cfxk_1a2b3c4d5e6f7g8h9i0j_key\"\n\
         [listing]\nenabled = true\nip_override = \"203.0.113.10\"\n",
    )
    .unwrap();
    config
        .validate()
        .expect("a complete listing config is valid");
}

#[test]
fn a_config_still_carrying_the_escrow_section_is_ignored_not_rejected() {
    // Escrow support went with the sidecar. An operator's old `[escrow]`
    // block is dead weight, not a reason to refuse boot — serde ignores
    // unknown sections, so their server still starts.
    let config: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[escrow]\nenabled = true\nbackend = \"sidecar\"\n")
            .expect("a stale [escrow] section must not break the load");
    assert_eq!(config.server.port, 30120);
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

#[test]
fn display_info_is_off_until_asked_for() {
    let debug = DebugConfig::default();
    assert_eq!(debug.display_info, DisplayInfoAccess::Off);
    assert!(
        !debug.allows(&["license:abc".to_owned()]),
        "the overlay must not appear on a server that never configured it"
    );
}

#[test]
fn allowlist_matches_identifiers_case_insensitively() {
    let debug = DebugConfig {
        display_info: DisplayInfoAccess::Allowlist,
        allow: vec!["License:ABC".to_owned()],
        ..DebugConfig::default()
    };
    assert!(debug.allows(&["steam:110000".to_owned(), "license:abc".to_owned()]));
    assert!(!debug.allows(&["license:other".to_owned()]));
}

#[test]
fn everyone_needs_no_identifier_at_all() {
    let debug = DebugConfig {
        display_info: DisplayInfoAccess::Everyone,
        ..DebugConfig::default()
    };
    assert!(debug.allows(&[]));
}

#[test]
fn an_empty_allowlist_is_rejected_rather_than_silently_denying() {
    let debug = DebugConfig {
        display_info: DisplayInfoAccess::Allowlist,
        ..DebugConfig::default()
    };
    assert!(matches!(
        debug.validate(),
        Err(ConfigError::Invalid {
            section: "debug",
            ..
        })
    ));
}

#[test]
fn refresh_hz_is_bounded() {
    for hz in [0, 31] {
        let debug = DebugConfig {
            display_info: DisplayInfoAccess::Everyone,
            refresh_hz: hz,
            ..DebugConfig::default()
        };
        assert!(
            debug.validate().is_err(),
            "refresh_hz = {hz} must be rejected"
        );
    }
}

#[test]
fn game_build_defaults_to_the_build_the_decoder_uses() {
    let config: BastonConfig = toml::from_str("[server]\nport = 30120\n").unwrap();
    assert_eq!(
        config.server.enforce_game_build,
        DEFAULT_GAME_BUILD.to_string(),
        "a config that states no build must still enforce one, or the server decodes a \
         build it never asked its clients to run"
    );
    assert_eq!(
        config.server.game_build().unwrap(),
        Some(DEFAULT_GAME_BUILD)
    );
    config.validate().expect("the default is valid");
}

#[test]
fn game_build_accepts_a_build_it_has_never_heard_of() {
    // The bound is a typo catcher, not an allowlist: next year's build has to
    // work without a code change.
    let config: BastonConfig = toml::from_str("[server]\nenforce_game_build = \"4210\"\n").unwrap();
    assert_eq!(config.server.game_build().unwrap(), Some(4210));
    config
        .validate()
        .expect("an unknown but plausible build is valid");
}

#[test]
fn empty_game_build_means_no_enforcement_and_is_valid() {
    let config: BastonConfig = toml::from_str("[server]\nenforce_game_build = \"\"\n").unwrap();
    assert_eq!(config.server.game_build().unwrap(), None);
    config
        .validate()
        .expect("enforcing nothing is a choice, not an error");
}

#[test]
fn a_game_build_that_is_not_a_build_stops_the_boot() {
    // Every one of these used to reach `/info.json` verbatim and fail later, in
    // the client, as a build switch that never happened.
    for raw in ["latest", "32258", "1603", "3258_1", "+3258", "3258 ", "0"] {
        let config: BastonConfig =
            toml::from_str(&format!("[server]\nenforce_game_build = \"{raw}\"\n")).unwrap();
        assert!(
            matches!(
                config.validate(),
                Err(ConfigError::Invalid {
                    section: "server",
                    ..
                })
            ),
            "enforce_game_build = {raw:?} should be refused at load"
        );
    }
}

#[test]
fn the_game_build_error_names_the_value_and_a_way_out() {
    let config: BastonConfig =
        toml::from_str("[server]\nenforce_game_build = \"latest\"\n").unwrap();
    let message = config.validate().unwrap_err().to_string();
    assert!(message.contains("latest"), "{message}");
    assert!(
        message.contains(&DEFAULT_GAME_BUILD.to_string()),
        "{message}"
    );
}

#[test]
fn a_map_file_resolves_against_the_config_it_was_named_in() {
    // Relative to the config file, not the working directory: a mounted
    // `config/` has to work without knowing where the process was launched.
    let mut config: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[meshing]\nmap_file = \"map.toml\"\n").unwrap();
    config.config_dir = Some(PathBuf::from("/srv/baston/config"));
    assert_eq!(
        config.map_file_path(),
        Some(PathBuf::from("/srv/baston/config").join("map.toml"))
    );
}

#[test]
fn an_absolute_map_file_is_left_alone() {
    let mut config: BastonConfig = toml::from_str(
        "[server]\nport = 30120\n[meshing]\nmap_file = \"/opt/baston/world.toml\"\n",
    )
    .unwrap();
    config.config_dir = Some(PathBuf::from("/srv/baston/config"));
    assert_eq!(
        config.map_file_path(),
        Some(PathBuf::from("/opt/baston/world.toml"))
    );
}

#[test]
fn no_map_file_means_zones_keep_declaring_their_own_bounds() {
    let config: BastonConfig =
        toml::from_str("[server]\nport = 30120\n[meshing]\nenabled = true\n").unwrap();
    assert_eq!(config.map_file_path(), None);
}

#[test]
fn listing_without_an_ip_override_is_the_normal_case() {
    // FXServer's sv_listingIpOverride defaults to empty for the same reason:
    // with no override CFX reaches the server through the hostname the nucleus
    // assigns, where CFX terminates TLS. Requiring one forced the direct-IP
    // path, where the list queries the game port over HTTPS and fails.
    let config: BastonConfig = toml::from_str(
        "[server]\nport = 30120\n\
         [license]\nmode = \"cfx\"\nsv_license_key = \"cfxk_1a2b3c4d5e6f7g8h9i0j_key\"\n\
         [listing]\nenabled = true\n",
    )
    .unwrap();
    config
        .validate()
        .expect("a listing with no override must be valid");
    assert!(config.listing.ip_override.is_none());
}

#[test]
fn an_override_that_is_given_still_has_to_be_reachable() {
    let base = "[server]\nport = 30120\n\
                [license]\nmode = \"cfx\"\nsv_license_key = \"cfxk_1a2b3c4d5e6f7g8h9i0j_key\"\n";
    for bad in ["0.0.0.0", "127.0.0.1", "224.0.0.1"] {
        let config: BastonConfig = toml::from_str(&format!(
            "{base}[listing]\nenabled = true\nip_override = \"{bad}\"\n"
        ))
        .unwrap();
        let message = config.validate().unwrap_err().to_string();
        assert!(message.contains(bad), "{message}");
        assert!(
            message.contains("leave it unset"),
            "the error should name the way out: {message}"
        );
    }
}
