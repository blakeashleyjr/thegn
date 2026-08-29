//! The LSP server **registry** model — pure, substrate-free data + validation.
//!
//! `[[lsp.servers]]` is a full registry, not an override table: an entry declares
//! a language `lang` key, the file `extensions` it serves, the `language_id` sent
//! in `didOpen`, and the server `command`/`args`. The six built-in servers are
//! the seed of the registry, expressed here as plain data ([`BUILTIN_SERVERS`]);
//! a user entry with a built-in key overrides that built-in field-wise, and any
//! other key registers an arbitrary server.
//!
//! This module owns two pure things only: the built-in table (so validation and
//! the `thegn-svc` resolver share one source of truth) and the config
//! **validation** for `thegn config validate`. The runtime resolver — extension
//! → key, key → a spawnable server, PATH existence — lives in `thegn_svc::lsp`
//! (it is the layer that actually launches processes); `thegn-core` stays
//! substrate-free.

use crate::config::LspServerConfig;

/// A built-in language server: its registry key, the extensions it serves, the
/// `didOpen` languageId, and the default command/args (used only when the
/// binary is found on `PATH`). The extension sets mirror
/// [`crate::semantic::Lang::from_path`] — the two are the tree-sitter tier and
/// the LSP tier of the same six languages and must agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinServer {
    pub key: &'static str,
    pub extensions: &'static [&'static str],
    pub language_id: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
}

/// The six built-in servers, in declaration order (which is also the
/// first-declared-wins resolution order for any shared extension).
pub const BUILTIN_SERVERS: &[BuiltinServer] = &[
    BuiltinServer {
        key: "rust",
        extensions: &["rs"],
        language_id: "rust",
        command: "rust-analyzer",
        args: &[],
    },
    BuiltinServer {
        key: "typescript",
        extensions: &["ts", "mts", "cts"],
        language_id: "typescript",
        command: "typescript-language-server",
        args: &["--stdio"],
    },
    BuiltinServer {
        key: "tsx",
        extensions: &["tsx"],
        language_id: "typescriptreact",
        command: "typescript-language-server",
        args: &["--stdio"],
    },
    BuiltinServer {
        key: "javascript",
        extensions: &["js", "mjs", "cjs", "jsx"],
        language_id: "javascript",
        command: "typescript-language-server",
        args: &["--stdio"],
    },
    BuiltinServer {
        key: "python",
        extensions: &["py", "pyi"],
        language_id: "python",
        command: "pyright-langserver",
        args: &["--stdio"],
    },
    BuiltinServer {
        key: "go",
        extensions: &["go"],
        language_id: "go",
        command: "gopls",
        args: &[],
    },
];

/// The built-in entry for `key`, if `key` names one of the six built-ins.
pub fn builtin(key: &str) -> Option<&'static BuiltinServer> {
    BUILTIN_SERVERS.iter().find(|b| b.key == key)
}

/// Whether `key` is one of the six built-in language keys.
pub fn is_builtin_key(key: &str) -> bool {
    builtin(key).is_some()
}

/// Normalize a raw extension for matching: drop a leading dot, ASCII-lowercase.
pub fn normalize_ext(ext: &str) -> String {
    ext.trim().trim_start_matches('.').to_ascii_lowercase()
}

/// Strictly validate `[[lsp.servers]]`, returning human-readable problems for
/// `thegn config validate` (empty = ok). Pure; no I/O.
///
/// Two rules, per the registry spec:
/// 1. a non-built-in `lang` key that declares no `extensions` is an error (it
///    could never resolve any file);
/// 2. an extension claimed by two different keys is flagged, naming both — at
///    runtime the first-declared entry still wins, so a bad config degrades
///    rather than failing.
pub fn validate_servers(servers: &[LspServerConfig]) -> Vec<String> {
    use std::collections::{BTreeMap, HashSet};

    let mut errs = Vec::new();

    // Collect the effective (extension → key) claims in declaration order. A
    // built-in key with its own `extensions` replaces the built-in set; a
    // built-in key without `extensions` keeps the built-in set; a non-built-in
    // key contributes exactly what it declares.
    let mut claims: Vec<(String, String)> = Vec::new(); // (ext, key), in order

    // Seed with built-ins that the config does not re-declare extensions for.
    let overridden: HashSet<&str> = servers
        .iter()
        .filter(|s| !s.extensions.is_empty())
        .map(|s| s.lang.trim())
        .collect();
    for b in BUILTIN_SERVERS {
        if !overridden.contains(b.key) {
            for e in b.extensions {
                claims.push(((*e).to_string(), b.key.to_string()));
            }
        }
    }

    for s in servers {
        let key = s.lang.trim();
        if key.is_empty() {
            errs.push("lsp.servers: an entry has an empty `lang` key".to_string());
            continue;
        }
        let declared: Vec<String> = s
            .extensions
            .iter()
            .map(|e| normalize_ext(e))
            .filter(|e| !e.is_empty())
            .collect();
        if !is_builtin_key(key) && declared.is_empty() {
            errs.push(format!(
                "lsp.servers: `{key}` is not a built-in language and declares no \
                 `extensions` (add e.g. `extensions = [\"{key}\"]`)"
            ));
        }
        for ext in declared {
            claims.push((ext, key.to_string()));
        }
    }

    // Rule 2: any extension claimed by two or more distinct keys.
    let mut by_ext: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (ext, key) in claims {
        let keys = by_ext.entry(ext).or_default();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    for (ext, keys) in by_ext {
        if keys.len() > 1 {
            errs.push(format!(
                "lsp.servers: extension `.{ext}` is claimed by more than one entry \
                 ({}); the first-declared wins at runtime",
                keys.join(", ")
            ));
        }
    }

    errs
}

/// The notice text for an ignored worktree-local LSP declaration.
fn overlay_notice_text(source: &str) -> String {
    format!(
        "{source}: `[lsp]` / `[[lsp.servers]]` in a worktree-local config is \
         ignored (a language-server command is untrusted until config-trust \
         resolution lands); declare servers in your user config instead"
    )
}

/// Whether a repo-overlay body (in `format` = `"toml"` | `"yaml"` | `"json"`)
/// declares an `lsp` table. Pure over the bytes — no I/O — so it is unit-testable
/// and safe anywhere. Malformed input ⇒ `false` (no panic).
pub fn overlay_declares_lsp(body: &str, format: &str) -> bool {
    match format {
        "toml" => body
            .parse::<toml::Value>()
            .ok()
            .is_some_and(|v| v.get("lsp").is_some()),
        "yaml" | "yml" => serde_yaml::from_str::<serde_yaml::Value>(body)
            .ok()
            .is_some_and(|v| v.get("lsp").is_some()),
        "json" => serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .is_some_and(|v| v.get("lsp").is_some()),
        _ => false,
    }
}

/// Scan a repo root's `.thegn.{toml,yaml,yml,json}` overlay for an `lsp`
/// declaration, returning a one-line notice when found. Registry commands are
/// subprocess argv that run on first use, so a worktree-local file must never
/// inject one; until config-trust resolution lands, such entries are ignored
/// outright (the untrusted overlay does not even parse `lsp`), and this helper
/// lets `doctor` tell the user why.
///
/// Reads the filesystem (first existing file wins, mirroring
/// `config::load_repo_overlay`) — a diagnostic/off-loop helper, never called on
/// the event loop.
pub fn repo_overlay_lsp_notice(repo_root: &std::path::Path) -> Option<String> {
    for ext in ["toml", "yaml", "yml", "json"] {
        let path = repo_root.join(format!(".thegn.{ext}"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        return overlay_declares_lsp(&text, ext)
            .then(|| overlay_notice_text(&format!(".thegn.{ext}")));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(lang: &str, command: &str, exts: &[&str]) -> LspServerConfig {
        LspServerConfig {
            lang: lang.to_string(),
            command: command.to_string(),
            args: vec![],
            extensions: exts.iter().map(|s| s.to_string()).collect(),
            language_id: None,
        }
    }

    #[test]
    fn builtins_cover_the_six_languages_and_agree_with_tree_sitter() {
        assert_eq!(BUILTIN_SERVERS.len(), 6);
        assert!(is_builtin_key("rust"));
        assert!(is_builtin_key("go"));
        assert!(!is_builtin_key("zig"));
        // Every built-in extension maps back to the tree-sitter Lang of the same
        // key — the two tiers must not drift.
        for b in BUILTIN_SERVERS {
            for e in b.extensions {
                let l = crate::semantic::Lang::from_path(&format!("x.{e}")).unwrap();
                assert_eq!(l.key(), b.key, "extension .{e}");
            }
        }
    }

    #[test]
    fn non_builtin_without_extensions_is_an_error() {
        let errs = validate_servers(&[cfg("zig", "zls", &[])]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("`zig`"), "{errs:?}");
        assert!(errs[0].contains("extensions"), "{errs:?}");
    }

    #[test]
    fn non_builtin_with_extensions_is_clean() {
        assert!(validate_servers(&[cfg("zig", "zls", &["zig", "zon"])]).is_empty());
    }

    #[test]
    fn legacy_override_only_entry_is_clean() {
        // A built-in key with only a command (no extensions) is the classic
        // override and must stay valid.
        assert!(validate_servers(&[cfg("rust", "my-ra", &[])]).is_empty());
    }

    #[test]
    fn collision_between_two_user_entries_is_flagged_naming_both() {
        let errs = validate_servers(&[cfg("foo", "a", &["x"]), cfg("bar", "b", &["x"])]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains(".x"), "{errs:?}");
        assert!(
            errs[0].contains("foo") && errs[0].contains("bar"),
            "{errs:?}"
        );
    }

    #[test]
    fn collision_with_a_builtin_extension_is_flagged() {
        // A new key claiming `.rs` collides with built-in rust.
        let errs = validate_servers(&[cfg("myrust", "x", &["rs"])]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains(".rs"), "{errs:?}");
        assert!(
            errs[0].contains("rust") && errs[0].contains("myrust"),
            "{errs:?}"
        );
    }

    #[test]
    fn builtin_extension_override_replaces_the_default_set_without_self_collision() {
        // Overriding rust's extensions must not read as rust colliding with
        // itself, and the replaced-away default (`.rs` still declared) is fine.
        assert!(validate_servers(&[cfg("rust", "rust-analyzer", &["rs", "rlib"])]).is_empty());
        // Now `.rlib` resolves to rust; a second entry claiming it collides.
        let errs = validate_servers(&[
            cfg("rust", "rust-analyzer", &["rs", "rlib"]),
            cfg("other", "x", &["rlib"]),
        ]);
        assert!(errs.iter().any(|e| e.contains(".rlib")), "{errs:?}");
    }

    #[test]
    fn dotted_and_mixed_case_extensions_normalize() {
        let errs = validate_servers(&[cfg("foo", "a", &[".X"]), cfg("bar", "b", &["x"])]);
        assert!(errs.iter().any(|e| e.contains(".x")), "{errs:?}");
    }

    #[test]
    fn overlay_declares_lsp_across_formats() {
        assert!(!overlay_declares_lsp("[sandbox]\nenabled = true\n", "toml"));
        assert!(overlay_declares_lsp(
            "[[lsp.servers]]\nlang = \"evil\"\nextensions = [\"x\"]\ncommand = \"pwn\"\n",
            "toml"
        ));
        assert!(overlay_declares_lsp("lsp:\n  enabled: true\n", "yaml"));
        assert!(overlay_declares_lsp(
            "{\"lsp\": {\"enabled\": true}}",
            "json"
        ));
        assert!(!overlay_declares_lsp("{\"sandbox\": {}}", "json"));
        // Malformed input and unknown format ⇒ false, no panic.
        assert!(!overlay_declares_lsp("!!! not toml", "toml"));
        assert!(!overlay_declares_lsp("anything", "ini"));
    }

    #[test]
    fn repo_overlay_notice_reads_the_thegn_file() {
        let dir = std::env::temp_dir().join(format!("thegn-lsp-trust-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No overlay ⇒ no notice.
        assert!(repo_overlay_lsp_notice(&dir).is_none());
        // An overlay declaring lsp ⇒ a notice naming the file.
        std::fs::write(
            dir.join(".thegn.toml"),
            "[[lsp.servers]]\nlang = \"evil\"\nextensions = [\"x\"]\ncommand = \"pwn\"\n",
        )
        .unwrap();
        let n = repo_overlay_lsp_notice(&dir).expect("notice");
        assert!(n.contains(".thegn.toml") && n.contains("ignored"), "{n}");
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
    }
}
