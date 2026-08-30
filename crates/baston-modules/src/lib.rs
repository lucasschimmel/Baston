//! The BASTON module registry — see `docs/adr/002-module-tiers.md`.
//!
//! A capability belongs to exactly one tier, and the tier decides the
//! mechanism:
//!
//! - **Tier 0 (core)** is not represented here. It is never optional.
//! - **Tier 1 (module)** is compiled in unconditionally and toggled at
//!   runtime. Cost when off must be indistinguishable from absence.
//! - **Tier 2 (capability)** is selected at build time with a Cargo feature,
//!   because enabling it changes the dependency graph. Operators receive it as
//!   a prebuilt [`Bundle`], never as a source build.
//! - **Tier 3 (addon)** lives out of process and is not represented here
//!   either — it needs no gate.
//!
//! This crate describes capabilities. It never implements them, and it holds
//! no dependency beyond `serde`, so anything in the workspace can depend on it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Which mechanism gates a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Tier 1 — compiled in, toggled at runtime.
    Module,
    /// Tier 2 — selected at build time via a Cargo feature.
    Capability,
}

impl Tier {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Capability => "capability",
        }
    }

    /// Tier number as used in ADR-002, for reports meant to be read next to it.
    pub const fn number(self) -> u8 {
        match self {
            Self::Module => 1,
            Self::Capability => 2,
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Every gated capability BASTON ships.
///
/// The discriminants are stable: they index [`ModuleSet`]'s bitmask, which is
/// serialised into diagnostics. Append new modules at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ModuleId {
    // --- Tier 1 ---
    /// Embedded Mumble-compatible voice server (`baston-voice`).
    Voice = 0,
    /// Prometheus exporter. The `metrics` instrumentation itself is core; only
    /// the recorder and its HTTP listener are gated.
    Metrics = 1,
    /// Monitoring/control HTTP API and the legacy `/admin/*` routes.
    AdminApi = 2,
    /// The in-game `displayinfo` debug overlay.
    DebugOverlay = 3,
    /// Script profiler capture and its API routes.
    Profiler = 4,
    /// Filesystem watcher that restarts a resource when its scripts change.
    HotReload = 5,
    // --- Tier 2 ---
    /// JavaScript scripting runtime (`deno_core` / V8).
    ScriptingJs = 6,
    /// Lua scripting runtime (`mlua` / Lua 5.4).
    ScriptingLua = 7,
    /// CFX Asset Escrow support (Windows, operator-supplied FXServer).
    Escrow = 8,
}

/// Every module, in declaration order. Reports iterate this so their output
/// order is stable across runs.
pub const ALL: &[ModuleId] = &[
    ModuleId::Voice,
    ModuleId::Metrics,
    ModuleId::AdminApi,
    ModuleId::DebugOverlay,
    ModuleId::Profiler,
    ModuleId::HotReload,
    ModuleId::ScriptingJs,
    ModuleId::ScriptingLua,
    ModuleId::Escrow,
];

impl ModuleId {
    /// Stable identifier used in `[modules]`, on the CLI, and in the banner.
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Metrics => "metrics",
            Self::AdminApi => "admin-api",
            Self::DebugOverlay => "debug-overlay",
            Self::Profiler => "profiler",
            Self::HotReload => "hot-reload",
            Self::ScriptingJs => "scripting-js",
            Self::ScriptingLua => "scripting-lua",
            Self::Escrow => "escrow",
        }
    }

    pub const fn tier(self) -> Tier {
        match self {
            Self::Voice
            | Self::Metrics
            | Self::AdminApi
            | Self::DebugOverlay
            | Self::Profiler
            | Self::HotReload => Tier::Module,
            Self::ScriptingJs | Self::ScriptingLua | Self::Escrow => Tier::Capability,
        }
    }

    /// One line, shown by `--modules`.
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Voice => "Mumble-compatible voice server (TLS control + UDP voice)",
            Self::Metrics => "Prometheus exporter on the metrics port",
            Self::AdminApi => "monitoring/control HTTP API and legacy /admin routes",
            Self::DebugOverlay => "in-game displayinfo overlay",
            Self::Profiler => "script profiler capture and its API routes",
            Self::HotReload => "restart resources when their scripts change on disk",
            Self::ScriptingJs => "JavaScript resources (deno_core / V8)",
            Self::ScriptingLua => "Lua resources (mlua / Lua 5.4)",
            Self::Escrow => "CFX Asset Escrow decryption (Windows)",
        }
    }

    /// The `baston.toml` section that configures this module, when it has one.
    ///
    /// Used to tell an operator that the section they just edited belongs to a
    /// module that is off — the single likeliest way to misconfigure BASTON.
    pub const fn config_section(self) -> Option<&'static str> {
        match self {
            Self::Voice => Some("voice"),
            Self::Metrics => Some("metrics"),
            Self::AdminApi => Some("api"),
            Self::DebugOverlay => Some("debug"),
            Self::Escrow => Some("escrow"),
            // Configured through `[dev]`, which also carries core settings, so
            // the section cannot be attributed to the module alone.
            Self::HotReload => None,
            // No section of their own: the profiler is driven over the API, and
            // the scripting capabilities are selected by the bundle.
            Self::Profiler | Self::ScriptingJs | Self::ScriptingLua => None,
        }
    }

    /// Whether the module is on when nothing says otherwise.
    ///
    /// Off is the default for anything that opens a listener or widens the
    /// control surface. `voice` is the deliberate exception: it is a headline
    /// capability, and one an operator has to discover in the documentation is
    /// one that effectively does not exist. `scripting-*` defaults follow the
    /// bundle — a capability that was compiled in was, by definition, asked
    /// for.
    pub const fn default_enabled(self) -> bool {
        match self {
            Self::Voice => true,
            Self::Metrics => true,
            Self::HotReload => true,
            Self::ScriptingJs | Self::ScriptingLua => true,
            Self::AdminApi | Self::DebugOverlay | Self::Profiler | Self::Escrow => false,
        }
    }

    /// Whether this build contains the code at all.
    ///
    /// Always true for Tier 1. For Tier 2 it reflects the Cargo feature, which
    /// the compiling binary must forward to this crate.
    // The arms are `cfg!` values, so in a bundle where every capability is
    // absent they all fold to `false` and clippy sees a `matches!`. Rewriting
    // it that way would only be correct for that one bundle.
    #[allow(clippy::match_like_matches_macro)]
    pub const fn is_compiled_in(self) -> bool {
        match self {
            Self::ScriptingJs => cfg!(feature = "scripting-js"),
            Self::ScriptingLua => cfg!(feature = "scripting-lua"),
            Self::Escrow => cfg!(feature = "escrow"),
            // Tier 1 is compiled in unconditionally (ADR-002).
            _ => true,
        }
    }

    /// The bundle an operator needs in order to get this capability, for error
    /// messages that would otherwise dead-end on "not compiled in".
    pub const fn provided_by(self) -> Option<&'static str> {
        match self {
            Self::ScriptingJs => Some("js (or full)"),
            Self::ScriptingLua => Some("lua (or full)"),
            Self::Escrow => Some("full, on Windows"),
            _ => None,
        }
    }

    pub fn parse(slug: &str) -> Option<Self> {
        ALL.iter().copied().find(|m| m.slug() == slug)
    }

    /// The environment variable that overrides this module, e.g.
    /// `BASTON_MODULE_ADMIN_API`.
    pub fn env_var(self) -> String {
        format!(
            "BASTON_MODULE_{}",
            self.slug().replace('-', "_").to_uppercase()
        )
    }

    const fn bit(self) -> u32 {
        1u32 << (self as u8)
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// The set of modules enabled for one process.
///
/// A bitmask so it is `Copy` and free to consult: module checks sit on boot
/// paths, but also in a few per-request paths (the API keyring, the overlay
/// feed), and a check must never be a reason to cache a bool elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModuleSet(u32);

impl ModuleSet {
    /// Nothing enabled. Used as the base that configuration resolves onto.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Every module at its documented default, minus anything this build does
    /// not contain.
    pub fn defaults() -> Self {
        let mut set = Self::empty();
        for &module in ALL {
            if module.default_enabled() && module.is_compiled_in() {
                set.enable(module);
            }
        }
        set
    }

    pub const fn is_enabled(self, module: ModuleId) -> bool {
        self.0 & module.bit() != 0
    }

    pub fn enable(&mut self, module: ModuleId) {
        self.0 |= module.bit();
    }

    pub fn disable(&mut self, module: ModuleId) {
        self.0 &= !module.bit();
    }

    pub fn set(&mut self, module: ModuleId, enabled: bool) {
        if enabled {
            self.enable(module);
        } else {
            self.disable(module);
        }
    }

    pub fn enabled(self) -> impl Iterator<Item = ModuleId> {
        ALL.iter().copied().filter(move |&m| self.is_enabled(m))
    }

    /// Compiled into this build but switched off — distinct from absent, and
    /// the distinction is what an operator needs when a feature "does nothing".
    pub fn disabled(self) -> impl Iterator<Item = ModuleId> {
        ALL.iter()
            .copied()
            .filter(move |&m| !self.is_enabled(m) && m.is_compiled_in())
    }

    /// Not in this build at all. Fixing these needs a different bundle.
    pub fn absent(self) -> impl Iterator<Item = ModuleId> {
        ALL.iter().copied().filter(|m| !m.is_compiled_in())
    }

    /// Slugs of the enabled modules, for the banner and structured logs.
    pub fn slugs(self) -> Vec<&'static str> {
        self.enabled().map(ModuleId::slug).collect()
    }

    /// A scripting capability is enabled and compiled in.
    pub fn has_scripting(self) -> bool {
        self.is_enabled(ModuleId::ScriptingJs) || self.is_enabled(ModuleId::ScriptingLua)
    }
}

/// A supported build of BASTON: which Tier 2 capabilities it was compiled with.
///
/// Bundles exist to bound the test matrix. Arbitrary feature combinations build
/// from source but are not supported, and report as [`Bundle::Custom`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bundle {
    /// No scripting runtime: zone worker, relay, benchmarking.
    Lite,
    /// JavaScript only. The default bundle.
    Js,
    /// Lua only.
    Lua,
    /// Everything, including escrow.
    Full,
    /// A supported combination was not what this binary was built with.
    Custom,
}

impl Bundle {
    /// What this binary actually contains, derived from the compiled features
    /// rather than declared anywhere — a build cannot misreport itself.
    pub const fn current() -> Self {
        let js = ModuleId::ScriptingJs.is_compiled_in();
        let lua = ModuleId::ScriptingLua.is_compiled_in();
        let escrow = ModuleId::Escrow.is_compiled_in();
        match (js, lua, escrow) {
            (false, false, false) => Self::Lite,
            (true, false, false) => Self::Js,
            (false, true, false) => Self::Lua,
            (true, true, true) => Self::Full,
            _ => Self::Custom,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Lite => "lite",
            Self::Js => "js",
            Self::Lua => "lua",
            Self::Full => "full",
            Self::Custom => "custom",
        }
    }

    /// Whether CI builds and tests this combination.
    pub const fn is_supported(self) -> bool {
        !matches!(self, Self::Custom)
    }
}

impl fmt::Display for Bundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Human-readable module report, printed by `--modules`.
///
/// Written for a support channel: an operator pastes it, and the reader learns
/// the bundle, what is running, what is merely off, and what would need another
/// build — three states that a plain on/off list conflates.
pub fn report(set: ModuleSet) -> String {
    use fmt::Write as _;

    let bundle = Bundle::current();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "BASTON {} · bundle: {bundle}",
        env!("CARGO_PKG_VERSION")
    );
    if !bundle.is_supported() {
        let _ = writeln!(
            out,
            "  note: this feature combination is buildable but not covered by CI"
        );
    }
    let _ = writeln!(out);

    for &module in ALL {
        let state = if !module.is_compiled_in() {
            "absent "
        } else if set.is_enabled(module) {
            "on     "
        } else {
            "off    "
        };
        let _ = writeln!(
            out,
            "  {state} {:<14} tier {}  {}",
            module.slug(),
            module.tier().number(),
            module.summary()
        );
    }

    let absent: Vec<ModuleId> = set.absent().collect();
    if !absent.is_empty() {
        let _ = writeln!(out, "\n  absent capabilities need a different bundle:");
        for module in absent {
            if let Some(bundle) = module.provided_by() {
                let _ = writeln!(out, "    {:<14} → bundle {bundle}", module.slug());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_unique_and_parse_round_trips() {
        let mut seen = Vec::new();
        for &module in ALL {
            assert!(
                !seen.contains(&module.slug()),
                "duplicate slug {}",
                module.slug()
            );
            seen.push(module.slug());
            assert_eq!(ModuleId::parse(module.slug()), Some(module));
        }
    }

    #[test]
    fn all_covers_every_discriminant() {
        // A module added to the enum but not to ALL would silently vanish from
        // every report and from `[modules]` resolution.
        assert_eq!(ALL.len(), ModuleId::Escrow as usize + 1);
    }

    #[test]
    fn bits_do_not_collide() {
        let mut mask = 0u32;
        for &module in ALL {
            assert_eq!(mask & module.bit(), 0, "bit collision on {module}");
            mask |= module.bit();
        }
    }

    #[test]
    fn tier_one_is_always_compiled_in() {
        for &module in ALL {
            if module.tier() == Tier::Module {
                assert!(module.is_compiled_in(), "{module} must always be present");
            }
        }
    }

    #[test]
    fn defaults_never_enable_an_absent_capability() {
        let set = ModuleSet::defaults();
        for module in set.enabled() {
            assert!(module.is_compiled_in(), "{module} enabled but not compiled");
        }
    }

    #[test]
    fn set_operations_are_independent() {
        let mut set = ModuleSet::empty();
        set.enable(ModuleId::Voice);
        set.enable(ModuleId::Profiler);
        assert!(set.is_enabled(ModuleId::Voice));
        assert!(set.is_enabled(ModuleId::Profiler));
        assert!(!set.is_enabled(ModuleId::Metrics));
        set.disable(ModuleId::Voice);
        assert!(!set.is_enabled(ModuleId::Voice));
        assert!(set.is_enabled(ModuleId::Profiler));
    }

    #[test]
    fn control_surfaces_are_off_by_default() {
        // ADR-002: anything that widens the control surface stays off until an
        // operator asks for it.
        for module in [
            ModuleId::AdminApi,
            ModuleId::DebugOverlay,
            ModuleId::Profiler,
        ] {
            assert!(!module.default_enabled(), "{module} must default to off");
        }
    }

    #[test]
    fn env_var_names_are_shouty_snake_case() {
        assert_eq!(ModuleId::AdminApi.env_var(), "BASTON_MODULE_ADMIN_API");
        assert_eq!(ModuleId::Voice.env_var(), "BASTON_MODULE_VOICE");
    }

    #[test]
    fn report_mentions_every_module() {
        let text = report(ModuleSet::defaults());
        for &module in ALL {
            assert!(text.contains(module.slug()), "report omits {module}");
        }
    }
}
