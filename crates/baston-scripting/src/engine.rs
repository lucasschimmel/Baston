//! Which scripting runtime owns a resource (ADR-002, Tier 2).
//!
//! A resource declares its language by the extension of its server scripts, so
//! selection needs no new manifest key and existing FiveM resources keep
//! working unchanged. The choice is made once, when the resource loads.
//!
//! A resource may use both. FiveM runs each script in the runtime its own
//! extension picks, and `cfx-server-data` ships a resource that relies on it:
//! `runcode` has Lua server scripts and a shared `runcode.js`. So the scripts
//! are grouped by engine and the resource gets one runtime per group.
//!
//! The two halves share what the host owns — events, state bags, KVP, the
//! player directory — because those live outside the isolates. They do not
//! share `exports`, which are registered inside a runtime; that matches the
//! existing limit, since exports do not cross resources either.

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

/// Group a resource's server scripts by the engine that runs each one.
///
/// Order is preserved within a group, because `server_scripts` order is what
/// a resource relies on; between groups it does not exist, since the groups
/// run in separate runtimes.
///
/// Refuses rather than guessing: an unrecognised extension, or an engine this
/// bundle does not contain. Each refusal names the fix, because the operator
/// hitting it is usually one bundle away from a working server.
pub fn group_by_engine(
    resource: &str,
    script_paths: &[String],
) -> Result<Vec<(Engine, Vec<usize>)>, ScriptError> {
    let mut groups: Vec<(Engine, Vec<usize>)> = Vec::new();
    for (index, path) in script_paths.iter().enumerate() {
        let Some(engine) = Engine::of_path(path) else {
            return Err(ScriptError::RuntimeInit {
                resource: resource.to_owned(),
                message: format!(
                    "server script \"{path}\" has no runtime\n  \
                     → BASTON runs .js/.mjs/.cjs and .lua server scripts"
                ),
            });
        };
        match groups.iter_mut().find(|(known, _)| *known == engine) {
            Some((_, indices)) => indices.push(index),
            None => groups.push((engine, vec![index])),
        }
    }

    if groups.is_empty() {
        return Err(ScriptError::RuntimeInit {
            resource: resource.to_owned(),
            message: "no server_scripts to run".to_owned(),
        });
    }

    // A missing runtime is fatal for the whole resource rather than for its
    // half: running only the Lua side of a resource that also has JS would be
    // a resource behaving in a way its author never wrote.
    if let Some((engine, _)) = groups.iter().find(|(e, _)| !e.is_compiled_in()) {
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
    Ok(groups)
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

    /// Groups, not a single choice: the count and the order inside each are
    /// what a resource's `server_scripts` promised.
    #[test]
    #[cfg(feature = "js")]
    fn scripts_of_one_language_form_one_group_in_order() {
        let groups = group_by_engine("test", &paths(&["a.js", "b.js"])).unwrap();
        assert_eq!(groups, vec![(Engine::Js, vec![0, 1])]);
    }

    #[test]
    #[cfg(feature = "lua")]
    fn a_lua_resource_groups_onto_the_lua_engine() {
        let groups = group_by_engine("test", &paths(&["sv.lua"])).unwrap();
        assert_eq!(groups, vec![(Engine::Lua, vec![0])]);
    }

    /// `cfx-server-data`'s runcode: Lua server scripts and a shared
    /// `runcode.js`. FiveM runs it, and refusing it was our limitation.
    #[test]
    #[cfg(all(feature = "js", feature = "lua"))]
    fn a_resource_using_both_languages_gets_a_group_for_each() {
        let groups = group_by_engine(
            "runcode",
            &paths(&["runcode_sv.lua", "runcode.js", "runcode_web.lua"]),
        )
        .unwrap();
        assert_eq!(
            groups,
            vec![(Engine::Lua, vec![0, 2]), (Engine::Js, vec![1])],
            "each language keeps its own scripts, in the order they were declared"
        );
    }

    /// A group appears in the order its language was first seen, so the log
    /// and the error messages read the way the manifest does.
    #[test]
    #[cfg(all(feature = "js", feature = "lua"))]
    fn groups_follow_the_order_the_languages_first_appear() {
        let groups = group_by_engine("test", &paths(&["a.js", "b.lua"])).unwrap();
        assert_eq!(groups[0].0, Engine::Js);
        assert_eq!(groups[1].0, Engine::Lua);
    }

    #[test]
    fn an_unknown_extension_names_what_is_supported() {
        let err = group_by_engine("test", &paths(&["main.py"])).expect_err("unsupported");
        assert!(err.to_string().contains(".lua"), "{err}");
    }

    #[test]
    fn a_resource_without_server_scripts_is_refused() {
        assert!(group_by_engine("test", &[]).is_err());
    }

    #[test]
    #[cfg(not(feature = "lua"))]
    fn a_lua_resource_on_a_js_build_names_the_bundle() {
        // The single most likely support question for a bundled binary.
        let err = group_by_engine("test", &paths(&["sv.lua"])).expect_err("no lua here");
        let text = err.to_string();
        assert!(text.contains("bundle lua"), "{text}");
        assert!(text.contains("--modules"), "{text}");
    }
}
