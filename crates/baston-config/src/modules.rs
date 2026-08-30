//! `[modules]` resolution — which capabilities this process actually runs.
//!
//! Three sources feed the final [`ModuleSet`], in increasing order of
//! precedence:
//!
//! 1. the per-module defaults from `baston-modules`;
//! 2. the legacy per-section flags (`[voice] enabled`, `[metrics] enabled`,
//!    `[debug] display_info`, `[dev] hot_reload`), which
//!    stay authoritative so existing `baston.toml` files keep their meaning;
//! 3. `[modules] enable` / `[modules] disable`, then `BASTON_MODULE_*`.
//!
//! Layers 2 and 3 are *not* allowed to silently disagree. An operator who
//! writes `[voice] enabled = true` and `[modules] disable = ["voice"]` has a
//! bug in their configuration, and resolving it by precedence would hide it —
//! so the load fails naming both sites.

use baston_modules::{ModuleId, ModuleSet};
use serde::Deserialize;

use crate::ConfigError;

/// `[modules]` section — the Tier 1/Tier 2 switchboard (ADR-002).
///
/// Deliberately additive rather than exhaustive: an operator states the
/// deltas they want from the defaults, so a new module shipping in a later
/// version does not require every existing config to be rewritten.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModulesConfig {
    /// Modules to turn on, by slug.
    #[serde(default)]
    pub enable: Vec<String>,
    /// Modules to turn off, by slug.
    #[serde(default)]
    pub disable: Vec<String>,
}

/// Whether a legacy per-section flag was actually written by the operator.
///
/// `serde` cannot tell a default from an explicit value once parsed, and the
/// difference decides whether a `[modules]` entry contradicts something or
/// merely restates it. So presence is read from the raw TOML instead of being
/// threaded through every section struct as an `Option<bool>`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LegacyToggles {
    pub voice: Option<bool>,
    pub metrics: Option<bool>,
    pub debug_overlay: Option<bool>,
    pub hot_reload: Option<bool>,
}

impl LegacyToggles {
    /// Read the legacy flags that the operator wrote explicitly.
    ///
    /// Parsing the document a second time is cheap (it happens once, at boot)
    /// and keeps presence detection in one place instead of spreading
    /// `Option<bool>` through the public config structs.
    pub fn from_toml(raw: &str) -> Self {
        let Ok(doc) = toml::from_str::<toml::Value>(raw) else {
            // A malformed document is reported by the real parse; this pass
            // must not race it to a worse error message.
            return Self::default();
        };
        let flag =
            |section: &str, key: &str| -> Option<bool> { doc.get(section)?.get(key)?.as_bool() };
        Self {
            voice: flag("voice", "enabled"),
            metrics: flag("metrics", "enabled"),
            // The overlay has no boolean: any mode other than "off" is on.
            debug_overlay: doc
                .get("debug")
                .and_then(|d| d.get("display_info"))
                .and_then(|v| v.as_str())
                .map(|mode| !mode.eq_ignore_ascii_case("off")),
            hot_reload: flag("dev", "hot_reload"),
        }
    }

    fn get(self, module: ModuleId) -> Option<bool> {
        match module {
            ModuleId::Voice => self.voice,
            ModuleId::Metrics => self.metrics,
            ModuleId::DebugOverlay => self.debug_overlay,
            ModuleId::HotReload => self.hot_reload,
            _ => None,
        }
    }

    /// The `baston.toml` site that set this flag, for error messages.
    fn origin(module: ModuleId) -> &'static str {
        match module {
            ModuleId::Voice => "[voice] enabled",
            ModuleId::Metrics => "[metrics] enabled",
            ModuleId::DebugOverlay => "[debug] display_info",
            ModuleId::HotReload => "[dev] hot_reload",
            _ => "its section",
        }
    }
}

impl ModulesConfig {
    /// Resolve the modules this process runs.
    ///
    /// Fails rather than guessing when two configuration sites disagree, or
    /// when the operator asks for a capability this bundle does not contain.
    pub fn resolve(&self, legacy: LegacyToggles) -> Result<ModuleSet, ConfigError> {
        let mut set = ModuleSet::defaults();

        // 1. Legacy per-section flags, where explicitly written.
        for &module in baston_modules::ALL {
            if let Some(enabled) = legacy.get(module) {
                set.set(module, enabled);
            }
        }

        // 2. `[modules]`, checked against the legacy flags rather than layered
        //    blindly over them.
        let enable = self.parse_slugs(&self.enable, "enable")?;
        let disable = self.parse_slugs(&self.disable, "disable")?;

        for &module in &enable {
            if disable.contains(&module) {
                return Err(ConfigError::Invalid {
                    section: "modules",
                    reason: format!(
                        "\"{module}\" is in both enable and disable\n  \
                         → remove it from one of the two lists"
                    ),
                });
            }
        }

        for (module, wanted) in enable
            .iter()
            .map(|&m| (m, true))
            .chain(disable.iter().map(|&m| (m, false)))
        {
            if let Some(legacy_value) = legacy.get(module) {
                if legacy_value != wanted {
                    return Err(ConfigError::ModuleConflict {
                        module: module.slug(),
                        legacy_site: LegacyToggles::origin(module),
                        legacy_value,
                        list: if wanted { "enable" } else { "disable" },
                    });
                }
            }
            set.set(module, wanted);
        }

        // 3. Environment overrides, last so a container can flip a module
        //    without rewriting the file it mounted.
        for &module in baston_modules::ALL {
            let var = module.env_var();
            if let Ok(value) = std::env::var(&var) {
                let enabled = parse_bool(&value).ok_or_else(|| ConfigError::ModuleEnvOverride {
                    var: var.clone(),
                    value: value.clone(),
                })?;
                set.set(module, enabled);
            }
        }

        // 4. A capability this build does not contain cannot be switched on,
        //    and pretending otherwise would defer the failure to first use.
        for &module in baston_modules::ALL {
            if set.is_enabled(module) && !module.is_compiled_in() {
                return Err(ConfigError::ModuleNotCompiledIn {
                    module: module.slug(),
                    bundle: module.provided_by().unwrap_or("a different bundle"),
                });
            }
        }

        Ok(set)
    }

    fn parse_slugs(
        &self,
        slugs: &[String],
        list: &'static str,
    ) -> Result<Vec<ModuleId>, ConfigError> {
        slugs
            .iter()
            .map(|slug| {
                ModuleId::parse(slug).ok_or_else(|| ConfigError::Invalid {
                    section: "modules",
                    reason: format!(
                        "unknown module \"{slug}\" in {list}\n  → known modules: {}",
                        baston_modules::ALL
                            .iter()
                            .map(|m| m.slug())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                })
            })
            .collect()
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Sections whose module is off, so the operator learns their edits are inert
/// instead of wondering why nothing happened.
///
/// A warning rather than an error: a `[voice]` block left in a config with
/// voice switched off is normal, and refusing to boot over it would be
/// hostile. The conflict cases that *are* mistakes are rejected in
/// [`ModulesConfig::resolve`].
pub fn inert_sections(set: ModuleSet, raw: &str) -> Vec<(&'static str, &'static str)> {
    let Ok(doc) = toml::from_str::<toml::Value>(raw) else {
        return Vec::new();
    };
    baston_modules::ALL
        .iter()
        .copied()
        .filter(|&module| !set.is_enabled(module))
        .filter_map(|module| {
            let section = module.config_section()?;
            doc.get(section)?;
            Some((section, module.slug()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(toml_src: &str) -> Result<ModuleSet, ConfigError> {
        let config: ModulesConfig = toml::from_str::<toml::Value>(toml_src)
            .ok()
            .and_then(|v| v.get("modules").cloned())
            .map(|v| v.try_into().expect("valid [modules]"))
            .unwrap_or_default();
        config.resolve(LegacyToggles::from_toml(toml_src))
    }

    #[test]
    fn empty_config_yields_the_documented_defaults() {
        let set = resolve("[server]\nport = 30120\n").unwrap();
        assert_eq!(set, ModuleSet::defaults());
        assert!(set.is_enabled(ModuleId::Voice));
        assert!(!set.is_enabled(ModuleId::AdminApi));
    }

    #[test]
    fn legacy_voice_flag_still_decides() {
        let off = resolve("[voice]\nenabled = false\n").unwrap();
        assert!(!off.is_enabled(ModuleId::Voice));
        let on = resolve("[voice]\nenabled = true\n").unwrap();
        assert!(on.is_enabled(ModuleId::Voice));
    }

    #[test]
    fn display_info_mode_maps_onto_the_overlay_module() {
        let off = resolve("[debug]\ndisplay_info = \"off\"\n").unwrap();
        assert!(!off.is_enabled(ModuleId::DebugOverlay));
        let on = resolve("[debug]\ndisplay_info = \"everyone\"\n").unwrap();
        assert!(on.is_enabled(ModuleId::DebugOverlay));
    }

    #[test]
    fn modules_section_enables_a_control_surface() {
        let set = resolve("[modules]\nenable = [\"admin-api\", \"profiler\"]\n").unwrap();
        assert!(set.is_enabled(ModuleId::AdminApi));
        assert!(set.is_enabled(ModuleId::Profiler));
    }

    #[test]
    fn contradicting_a_legacy_flag_is_rejected() {
        let err = resolve("[voice]\nenabled = true\n[modules]\ndisable = [\"voice\"]\n")
            .expect_err("the two sites disagree");
        let text = err.to_string();
        assert!(text.contains("[voice] enabled"), "{text}");
        assert!(text.contains("disable"), "{text}");
    }

    #[test]
    fn agreeing_with_a_legacy_flag_is_fine() {
        let set = resolve("[voice]\nenabled = false\n[modules]\ndisable = [\"voice\"]\n").unwrap();
        assert!(!set.is_enabled(ModuleId::Voice));
    }

    #[test]
    fn a_module_in_both_lists_is_rejected() {
        let err = resolve("[modules]\nenable = [\"profiler\"]\ndisable = [\"profiler\"]\n")
            .expect_err("contradictory lists");
        assert!(err.to_string().contains("both enable and disable"));
    }

    #[test]
    fn unknown_slugs_name_the_valid_ones() {
        let err = resolve("[modules]\nenable = [\"lua\"]\n").expect_err("no module named lua");
        let text = err.to_string();
        assert!(text.contains("unknown module \"lua\""), "{text}");
        assert!(text.contains("scripting-lua"), "{text}");
    }

    #[test]
    fn inert_sections_flag_configured_but_disabled_modules() {
        let set = resolve("[modules]\ndisable = [\"metrics\"]\n").unwrap();
        let inert = inert_sections(
            set,
            "[metrics]\nport = 9090\n[modules]\ndisable = [\"metrics\"]\n",
        );
        assert!(inert.iter().any(|(section, _)| *section == "metrics"));
    }

    #[test]
    fn inert_sections_ignore_enabled_modules() {
        let set = resolve("[voice]\nenabled = true\n").unwrap();
        let inert = inert_sections(set, "[voice]\nenabled = true\n");
        assert!(!inert.iter().any(|(section, _)| *section == "voice"));
    }

    #[test]
    fn absent_capabilities_cannot_be_enabled() {
        // baston-config never forwards the Tier 2 features, so from here the
        // scripting capabilities are always absent — which makes this the
        // canonical "wrong bundle" case.
        let err = resolve("[modules]\nenable = [\"scripting-lua\"]\n")
            .expect_err("lua is not compiled into this crate's build");
        let text = err.to_string();
        assert!(text.contains("scripting-lua"), "{text}");
        assert!(text.contains("bundle"), "{text}");
    }
}
