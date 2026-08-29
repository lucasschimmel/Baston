//! Resources that ship inside the server binary.
//!
//! BASTON is the server; the FiveM client draws only what a client script asks
//! it to. An overlay the server owns therefore still needs code running on the
//! client — but nothing says that code has to be a resource an operator
//! installs, can edit, or can stop.
//!
//! A builtin exists only in the binary: it is never on disk, never discovered
//! by [`ResourceManager`](baston_zone::ResourceManager), never listed among the
//! operator's resources, and cannot be started or stopped. It is advertised
//! straight into `getConfiguration` and served from memory, so what runs on the
//! client is exactly what the running server shipped, version-locked to it.
//!
//! The packfile is built once on first request and kept: the contents are
//! `'static`, so unlike the disk-backed cache there is nothing to invalidate.

use std::sync::{Arc, OnceLock};

use baston_config::BastonConfig;
use baston_protocol::ResourceManifest;
use baston_zone::packfile::{build_rpf, generate_fxmanifest, PackfileInput};
use sha1::{Digest, Sha1};

use super::packfile_cache::CachedPackfile;

/// The `displayinfo` overlay. The name is deliberately prefixed: a resource
/// directory sharing it is shadowed and reported, rather than silently
/// replacing server-owned code.
pub const DISPLAYINFO: &str = "baston_displayinfo";

const DISPLAYINFO_CLIENT: &str = include_str!("../../assets/displayinfo/client.js");

/// One embedded resource: its manifest and its client files.
struct Builtin {
    name: &'static str,
    client_scripts: &'static [&'static str],
    files: &'static [(&'static str, &'static str)],
    packfile: OnceLock<Arc<CachedPackfile>>,
}

impl Builtin {
    fn manifest(&self) -> ResourceManifest {
        ResourceManifest {
            name: self.name.to_owned(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            dependencies: Vec::new(),
            server_scripts: Vec::new(),
            client_scripts: self
                .client_scripts
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            files: Vec::new(),
        }
    }

    fn packfile(&self) -> Arc<CachedPackfile> {
        Arc::clone(self.packfile.get_or_init(|| {
            let manifest = self.manifest();
            let mut inputs = vec![PackfileInput {
                path: "fxmanifest.lua".to_owned(),
                data: generate_fxmanifest(&manifest).into_bytes(),
            }];
            for (path, contents) in self.files {
                inputs.push(PackfileInput {
                    path: (*path).to_owned(),
                    data: contents.as_bytes().to_vec(),
                });
            }
            let bytes = build_rpf(inputs);
            let sha1_hex = hex::encode(Sha1::digest(&bytes));
            tracing::info!(
                target: "gateway",
                resource = self.name,
                size = bytes.len(),
                sha1 = %sha1_hex,
                "builtin resource packed"
            );
            Arc::new(CachedPackfile {
                bytes: Arc::new(bytes),
                sha1_hex,
            })
        }))
    }
}

fn displayinfo() -> &'static Builtin {
    static DISPLAYINFO_BUILTIN: OnceLock<Builtin> = OnceLock::new();
    DISPLAYINFO_BUILTIN.get_or_init(|| Builtin {
        name: DISPLAYINFO,
        client_scripts: &["client.js"],
        files: &[("client.js", DISPLAYINFO_CLIENT)],
        packfile: OnceLock::new(),
    })
}

/// The builtins this server is serving, decided once at startup.
///
/// Membership is a config decision, not a runtime one: a server with the
/// overlay disabled does not advertise it at all, so the client never
/// downloads code it will not be allowed to use.
#[derive(Default)]
pub struct BuiltinResources {
    enabled: Vec<&'static Builtin>,
}

impl BuiltinResources {
    pub fn from_config(config: &BastonConfig) -> Self {
        let mut enabled: Vec<&'static Builtin> = Vec::new();
        if config.debug.display_info.is_enabled() {
            enabled.push(displayinfo());
            tracing::info!(
                target: "gateway",
                access = ?config.debug.display_info,
                refresh_hz = config.debug.refresh_hz,
                "displayinfo overlay enabled"
            );
        }
        Self { enabled }
    }

    /// Names to advertise in `getConfiguration`.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.enabled.iter().map(|builtin| builtin.name)
    }

    pub fn contains(&self, resource: &str) -> bool {
        self.enabled.iter().any(|builtin| builtin.name == resource)
    }

    /// The packfile for a builtin, or `None` if this server does not serve it.
    pub fn packfile(&self, resource: &str) -> Option<Arc<CachedPackfile>> {
        self.enabled
            .iter()
            .find(|builtin| builtin.name == resource)
            .map(|builtin| builtin.packfile())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baston_config::{DebugConfig, DisplayInfoAccess};

    fn config(access: DisplayInfoAccess) -> BastonConfig {
        let mut config: BastonConfig =
            toml::from_str("[server]\nport = 30120\n").expect("minimal config parses");
        config.debug = DebugConfig {
            display_info: access,
            allow: vec!["license:admin".to_owned()],
            refresh_hz: 5,
        };
        config
    }

    #[test]
    fn a_disabled_overlay_is_not_advertised_at_all() {
        let builtins = BuiltinResources::from_config(&config(DisplayInfoAccess::Off));
        assert_eq!(builtins.names().count(), 0);
        assert!(!builtins.contains(DISPLAYINFO));
        assert!(builtins.packfile(DISPLAYINFO).is_none());
    }

    #[test]
    fn an_enabled_overlay_packs_its_client_script() {
        let builtins = BuiltinResources::from_config(&config(DisplayInfoAccess::Allowlist));
        assert_eq!(builtins.names().collect::<Vec<_>>(), vec![DISPLAYINFO]);
        let pack = builtins.packfile(DISPLAYINFO).expect("builtin is served");
        assert!(!pack.bytes.is_empty());
        assert_eq!(pack.sha1_hex.len(), 40);
        // RPF2 magic, so the client actually mounts it.
        assert_eq!(&pack.bytes[..4], b"RPF2");
    }

    #[test]
    fn the_packfile_is_built_once_and_reused() {
        let builtins = BuiltinResources::from_config(&config(DisplayInfoAccess::Everyone));
        let first = builtins.packfile(DISPLAYINFO).expect("served");
        let second = builtins.packfile(DISPLAYINFO).expect("served");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn an_unknown_name_is_not_a_builtin() {
        let builtins = BuiltinResources::from_config(&config(DisplayInfoAccess::Everyone));
        assert!(!builtins.contains("some-operator-resource"));
        assert!(builtins.packfile("some-operator-resource").is_none());
    }

    #[test]
    fn the_generated_manifest_declares_the_client_script() {
        let manifest = displayinfo().manifest();
        assert_eq!(manifest.client_scripts, vec!["client.js".to_owned()]);
        assert!(
            manifest.server_scripts.is_empty(),
            "the overlay must run no server code: the snapshot comes from the server itself"
        );
        let fxmanifest = generate_fxmanifest(&manifest);
        assert!(fxmanifest.contains("client.js"), "{fxmanifest}");
    }
}
