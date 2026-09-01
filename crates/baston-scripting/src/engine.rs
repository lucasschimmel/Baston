//! Which scripting runtime owns a resource (ADR-002, Tier 2).
//!
//! A resource declares its language by the extension of its server scripts, so
//! selection needs no new manifest key and existing FiveM resources keep
//! working unchanged. The choice is made once, when the resource loads.
//!
//! One engine per resource is a BASTON limitation, not a rule of the platform.
//! FiveM runs each script in the runtime its own extension picks, and
//! `cfx-server-data` ships a resource that relies on it: `runcode` has Lua
//! server scripts and a shared `runcode.js`. Supporting that here means two
//! runtimes inside one resource — two threads, and an answer for how events,
//! exports and state bags behave across them — so the refusal below names the
//! limitation rather than blaming the resource for it.

use crate::error::ScriptError;

/// The scripting engine a resource runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// JavaScript, on deno_core / V8.
    Js,
    /// Lua, on mlua.
    Lua,
}

impl Engine {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Js => "js",
            Self::Lua => "lua",
        }
    }

    /// Whether this build contains the engine.
    pub const fn is_compiled_in(self) -> bool {
        match self {
            Self::Js => cfg!(feature = "js"),
            Self::Lua => cfg!(feature = "lua"),
        }
    }

    /// The bundle that ships this engine, for an error that would otherwise
    /// dead-end on "not compiled in".
    const fn bundle(self) -> &'static str {
        match self {
            Self::Js => "js (or full)",
            Self::Lua => "lua (or full)",
        }
    }

    /// The engine that owns `path`, or `None` for a file no engine claims.
    fn of_path(path: &str) -> Option<Self> {
        let extension = path.rsplit('.').next()?.to_ascii_lowercase();
        match extension.as_str() {
            "js" | "mjs" | "cjs" => Some(Self::Js),
            "lua" => Some(Self::Lua),
            _ => None,
        }
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Pick the engine for a resource from its server script paths.
///
/// Refuses rather than guessing in every ambiguous case: an unrecognised
/// extension, a mix of languages, or an engine this bundle does not contain.
/// Each refusal names the fix, because the operator hitting it is usually one
/// bundle away from a working server.
pub fn select(resource: &str, script_paths: &[String]) -> Result<Engine, ScriptError> {
    let mut chosen: Option<Engine> = None;
    for path in script_paths {
        let Some(engine) = Engine::of_path(path) else {
            return Err(ScriptError::RuntimeInit {
                resource: resource.to_owned(),
                message: format!(
                    "server script \"{path}\" has no runtime\n  \
                     → BASTON runs .js/.mjs/.cjs and .lua server scripts"
                ),
            });
        };
        match chosen {
            None => chosen = Some(engine),
            Some(previous) if previous != engine => {
                return Err(ScriptError::RuntimeInit {
                    resource: resource.to_owned(),
                    message: format!(
                        "server_scripts mixes {previous} and {engine}, which BASTON \
                         cannot run in one resource yet\n  \
                         → FiveM does allow it; this is a BASTON limitation\n  \
                         → split the resource in two, one per language, for now"
                    ),
                });
            }
            Some(_) => {}
        }
    }

    let engine = chosen.ok_or_else(|| ScriptError::RuntimeInit {
        resource: resource.to_owned(),
        message: "no server_scripts to run".to_owned(),
    })?;

    if !engine.is_compiled_in() {
        return Err(ScriptError::RuntimeInit {
            resource: resource.to_owned(),
            message: format!(
                "this build has no {engine} runtime\n  \
                 → it ships in bundle {}\n  \
                 → run `baston-gateway --modules` to see what this binary contains",
                engine.bundle()
            ),
        });
    }
    Ok(engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn extensions_map_to_their_engine() {
        assert_eq!(Engine::of_path("server/main.js"), Some(Engine::Js));
        assert_eq!(Engine::of_path("dist/index.mjs"), Some(Engine::Js));
        assert_eq!(Engine::of_path("sv_main.lua"), Some(Engine::Lua));
        // Case follows the filesystem's habits, not ours.
        assert_eq!(Engine::of_path("SV_MAIN.LUA"), Some(Engine::Lua));
        assert_eq!(Engine::of_path("readme.md"), None);
        assert_eq!(Engine::of_path("noextension"), None);
    }

    #[test]
    #[cfg(feature = "js")]
    fn a_javascript_resource_selects_the_js_engine() {
        let engine = select("test", &paths(&["a.js", "b.js"])).unwrap();
        assert_eq!(engine, Engine::Js);
    }

    #[test]
    #[cfg(feature = "lua")]
    fn a_lua_resource_selects_the_lua_engine() {
        let engine = select("test", &paths(&["sv.lua"])).unwrap();
        assert_eq!(engine, Engine::Lua);
    }

    /// The refusal has to name both languages *and* own the limitation.
    /// `cfx-server-data`'s `runcode` mixes them and FiveM runs it, so telling
    /// the operator their resource is wrong sends them looking for a fault
    /// that is on our side.
    #[test]
    fn mixing_engines_is_refused_as_our_limitation_not_the_resources_fault() {
        let err = select("test", &paths(&["a.js", "b.lua"])).expect_err("mixed");
        let text = err.to_string();
        assert!(text.contains("mixes"), "{text}");
        assert!(text.contains("js") && text.contains("lua"), "{text}");
        assert!(text.contains("BASTON limitation"), "{text}");
        assert!(text.contains("FiveM does allow it"), "{text}");
    }

    #[test]
    fn an_unknown_extension_names_what_is_supported() {
        let err = select("test", &paths(&["main.py"])).expect_err("unsupported");
        assert!(err.to_string().contains(".lua"), "{err}");
    }

    #[test]
    fn a_resource_without_server_scripts_is_refused() {
        assert!(select("test", &[]).is_err());
    }

    #[test]
    #[cfg(not(feature = "lua"))]
    fn a_lua_resource_on_a_js_build_names_the_bundle() {
        // The single most likely support question for a bundled binary.
        let err = select("test", &paths(&["sv.lua"])).expect_err("no lua here");
        let text = err.to_string();
        assert!(text.contains("bundle lua"), "{text}");
        assert!(text.contains("--modules"), "{text}");
    }
}
