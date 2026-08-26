//! The runtime LSP server registry: resolve a file path → language key, and a
//! key → a spawnable [`ServerSpec`], honoring the built-in defaults and the
//! user's `[[lsp.servers]]` overrides.
//!
//! The pure data (the six built-ins) and the config **validation** live in
//! `thegn_core::lsp_registry`; this is the layer that turns that data plus the
//! loaded config into concrete, launchable server specs and does the `PATH`
//! existence check that decides whether a built-in default is actually usable.
//! It stays pure w.r.t. process launching — resolution is total and testable
//! with an injected existence predicate — so the host can drive it off the loop.

use std::path::Path;

use thegn_core::config::LspServerConfig;
use thegn_core::lsp_registry::{BUILTIN_SERVERS, is_builtin_key, normalize_ext};

use super::ServerSpec;

/// One resolved registry entry: a language key, the extensions it serves, the
/// `didOpen` languageId, and the server command/args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub key: String,
    pub extensions: Vec<String>,
    pub language_id: String,
    /// The server executable. Empty ⇒ the language is disabled.
    pub command: String,
    pub args: Vec<String>,
    /// `true` when `command` is the built-in default (usable only when found on
    /// `PATH`); `false` when it is an explicit user command (trusted outright).
    pub default_command: bool,
    /// `true` when `key` is one of the six built-in languages.
    pub builtin: bool,
}

/// How a registry entry resolves for reporting (`thegn doctor`). Distinct from
/// [`Registry::resolve`] which yields a launchable spec — doctor wants to show a
/// missing/disabled command too, without spawning anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// `command = ""` — the language is turned off.
    Disabled,
    /// The command was found (`PATH` lookup or an explicit path).
    Ready(String),
    /// The command is named but not present on this host.
    Missing(String),
}

/// The resolved server registry: built-ins seeded first, then user entries
/// merged over them field-wise (or appended, for a new key).
#[derive(Debug, Clone, Default)]
pub struct Registry {
    entries: Vec<RegistryEntry>,
}

impl Registry {
    /// Build the registry from the loaded `[[lsp.servers]]` config.
    pub fn build(servers: &[LspServerConfig]) -> Registry {
        // Seed with the six built-ins in declaration order.
        let mut entries: Vec<RegistryEntry> = BUILTIN_SERVERS
            .iter()
            .map(|b| RegistryEntry {
                key: b.key.to_string(),
                extensions: b.extensions.iter().map(|e| (*e).to_string()).collect(),
                language_id: b.language_id.to_string(),
                command: b.command.to_string(),
                args: b.args.iter().map(|a| (*a).to_string()).collect(),
                default_command: true,
                builtin: true,
            })
            .collect();

        for s in servers {
            let key = s.lang.trim();
            if key.is_empty() {
                continue; // flagged by config validation; ignored at runtime
            }
            let ext_override: Vec<String> = s
                .extensions
                .iter()
                .map(|e| normalize_ext(e))
                .filter(|e| !e.is_empty())
                .collect();

            if let Some(slot) = entries.iter_mut().find(|e| e.key == key) {
                // Override a built-in (or an earlier duplicate) field-wise.
                // `command = ""` disables (today's semantics); a non-empty
                // command is trusted outright.
                slot.command = s.command.clone();
                slot.args = s.args.clone();
                slot.default_command = false;
                if !ext_override.is_empty() {
                    slot.extensions = ext_override;
                }
                if let Some(id) = s.language_id.as_ref().filter(|id| !id.trim().is_empty()) {
                    slot.language_id = id.trim().to_string();
                }
            } else {
                // A brand-new registry entry.
                let language_id = s
                    .language_id
                    .as_ref()
                    .map(|id| id.trim())
                    .filter(|id| !id.is_empty())
                    .unwrap_or(key)
                    .to_string();
                entries.push(RegistryEntry {
                    key: key.to_string(),
                    extensions: ext_override,
                    language_id,
                    command: s.command.clone(),
                    args: s.args.clone(),
                    default_command: false,
                    builtin: is_builtin_key(key),
                });
            }
        }

        Registry { entries }
    }

    /// Every entry (built-in and user), in declaration order — for `doctor`.
    pub fn entries(&self) -> &[RegistryEntry] {
        &self.entries
    }

    /// The registry entry for `key`.
    pub fn entry(&self, key: &str) -> Option<&RegistryEntry> {
        self.entries.iter().find(|e| e.key == key)
    }

    /// Resolve a file path to a registry key by its extension. First-declared
    /// entry wins on collision, so a bad (colliding) config degrades rather than
    /// failing. Pure.
    pub fn resolve_key(&self, path: &str) -> Option<String> {
        let ext = normalize_ext(path.rsplit('.').next()?);
        if ext.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .find(|e| e.extensions.contains(&ext))
            .map(|e| e.key.clone())
    }

    /// Resolve `key` to a launchable [`ServerSpec`], or `None` when the language
    /// is disabled or the (built-in default) command is not installed. An
    /// explicit user command is trusted outright; a built-in default is used
    /// only when found on `PATH`.
    pub fn resolve(&self, key: &str) -> Option<ServerSpec> {
        self.resolve_with(key, binary_on_path)
    }

    /// [`Registry::resolve`] with an injectable existence check (for tests).
    pub fn resolve_with(&self, key: &str, exists: impl Fn(&str) -> bool) -> Option<ServerSpec> {
        let e = self.entry(key)?;
        if e.command.is_empty() {
            return None; // disabled
        }
        if e.default_command && !exists(&e.command) {
            return None; // built-in default not installed
        }
        Some(ServerSpec {
            key: e.key.clone(),
            language_id: e.language_id.clone(),
            command: e.command.clone(),
            args: e.args.clone(),
        })
    }

    /// The reporting resolution for `key` (`thegn doctor`) — never spawns.
    pub fn describe(&self, key: &str) -> Option<Resolution> {
        self.describe_with(key, binary_on_path)
    }

    /// [`Registry::describe`] with an injectable existence check (for tests).
    pub fn describe_with(&self, key: &str, exists: impl Fn(&str) -> bool) -> Option<Resolution> {
        let e = self.entry(key)?;
        Some(if e.command.is_empty() {
            Resolution::Disabled
        } else if exists(&e.command) {
            Resolution::Ready(e.command.clone())
        } else {
            Resolution::Missing(e.command.clone())
        })
    }
}

/// Whether `cmd` resolves to a file (absolute/relative path, or a `PATH`
/// lookup). Shared with the doctor probe so both agree on "installed".
pub fn binary_on_path(cmd: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    if cmd.contains('/') {
        return Path::new(cmd).is_file();
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(
        lang: &str,
        command: &str,
        exts: &[&str],
        language_id: Option<&str>,
    ) -> LspServerConfig {
        LspServerConfig {
            lang: lang.to_string(),
            command: command.to_string(),
            args: vec![],
            extensions: exts.iter().map(|s| s.to_string()).collect(),
            language_id: language_id.map(str::to_string),
        }
    }

    #[test]
    fn builtins_present_by_default() {
        let reg = Registry::build(&[]);
        assert_eq!(reg.entries().len(), 6);
        // A built-in default resolves only when its binary exists.
        assert!(reg.resolve_with("rust", |c| c == "rust-analyzer").is_some());
        assert!(reg.resolve_with("rust", |_| false).is_none());
    }

    #[test]
    fn resolve_key_by_extension() {
        let reg = Registry::build(&[]);
        assert_eq!(reg.resolve_key("src/lib.rs").as_deref(), Some("rust"));
        assert_eq!(reg.resolve_key("a/b.tsx").as_deref(), Some("tsx"));
        assert_eq!(reg.resolve_key("x.MJS").as_deref(), Some("javascript"));
        assert_eq!(reg.resolve_key("README.md"), None);
        assert_eq!(reg.resolve_key("noext"), None);
    }

    #[test]
    fn non_builtin_entry_registers_and_resolves() {
        let reg = Registry::build(&[user("zig", "zls", &["zig", "zon"], None)]);
        assert_eq!(reg.resolve_key("main.zig").as_deref(), Some("zig"));
        assert_eq!(reg.resolve_key("build.zon").as_deref(), Some("zig"));
        // An explicit user command is trusted outright (no PATH gate).
        let spec = reg.resolve_with("zig", |_| false).expect("trusted command");
        assert_eq!(spec.command, "zls");
        assert_eq!(spec.language_id, "zig"); // defaults to the key
    }

    #[test]
    fn override_entry_keeps_builtin_extensions_and_language_id() {
        let reg = Registry::build(&[user("rust", "my-ra", &[], None)]);
        // Override command is trusted even when not on PATH.
        let spec = reg.resolve_with("rust", |_| false).expect("override");
        assert_eq!(spec.command, "my-ra");
        assert_eq!(spec.language_id, "rust");
        // Built-in extensions preserved.
        assert_eq!(reg.resolve_key("x.rs").as_deref(), Some("rust"));
    }

    #[test]
    fn empty_command_disables() {
        let reg = Registry::build(&[user("python", "", &[], None)]);
        assert!(reg.resolve_with("python", |_| true).is_none());
        assert_eq!(
            reg.describe_with("python", |_| true),
            Some(Resolution::Disabled)
        );
    }

    #[test]
    fn language_id_override_and_extension_override() {
        let reg = Registry::build(&[user(
            "tsx",
            "tsserver",
            &["tsx", "mtsx"],
            Some("tsx-custom"),
        )]);
        let spec = reg.resolve_with("tsx", |_| true).unwrap();
        assert_eq!(spec.language_id, "tsx-custom");
        assert_eq!(reg.resolve_key("a.mtsx").as_deref(), Some("tsx"));
    }

    #[test]
    fn first_declared_wins_on_collision() {
        // `foo` declared before `bar`, both claim `.x`.
        let reg = Registry::build(&[
            user("foo", "a", &["x"], None),
            user("bar", "b", &["x"], None),
        ]);
        assert_eq!(reg.resolve_key("t.x").as_deref(), Some("foo"));
    }

    #[test]
    fn describe_reports_ready_and_missing() {
        let reg = Registry::build(&[]);
        assert_eq!(
            reg.describe_with("go", |c| c == "gopls"),
            Some(Resolution::Ready("gopls".into()))
        );
        assert_eq!(
            reg.describe_with("go", |_| false),
            Some(Resolution::Missing("gopls".into()))
        );
        assert_eq!(reg.describe_with("nope", |_| true), None);
    }
}
