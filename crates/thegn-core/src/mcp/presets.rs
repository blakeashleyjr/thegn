//! Curated `[mcp_servers.<name>]` presets shipped as data (`thegn mcp preset`).
//!
//! THE-49 (a thegn-wide agent memory system) is answered here as a *decision*,
//! not a feature: every credible memory engine already speaks MCP, so memory is
//! a curated preset riding the proxy — thegn contributes the two things only it
//! can, per-workspace/worktree **partitioning** and **credential custody**, and
//! builds/bundles no memory engine of its own. Presets are references, printed
//! for the user to vet before `--write` appends them; thegn never hard-depends
//! on any preset's software.
//!
//! Each preset is a vetted block with a pinned `source`, least-privilege
//! `grants`, a default `proxy` exposure, and a note on external requirements.
//! At least one memory preset MUST be fully local (no API key, offline at
//! runtime) — enforced by [`tests::a_local_memory_preset_exists`].

/// One curated preset.
pub struct Preset {
    /// Preset id (also the `[mcp_servers.<id>]` key in its block).
    pub name: &'static str,
    /// Grouping (`memory`, `dev`, …).
    pub category: &'static str,
    /// One-line description.
    pub description: &'static str,
    /// External requirements at runtime — API keys, a container runtime, etc.
    /// **Empty ⇒ fully local, no API key, works offline once installed.**
    pub requires: &'static [&'static str],
    /// The `[mcp_servers.<name>]` TOML block, verbatim.
    pub toml: &'static str,
}

impl Preset {
    /// Whether this preset needs no external credential/runtime (local-first).
    pub fn is_local(&self) -> bool {
        self.requires.is_empty()
    }
}

/// The curated set. Memory presets first (THE-49), at least one fully local.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "memory-graph",
        category: "memory",
        description: "Local knowledge-graph memory (@modelcontextprotocol/server-memory), \
                      stored per workspace under the repo — no API key, offline once installed.",
        requires: &[],
        toml: r#"# Local knowledge-graph memory, partitioned per workspace.
# Fully local: the graph lives in a JSON file under the repo; no API key.
[mcp_servers.memory-graph]
command = ["npx", "-y", "@modelcontextprotocol/server-memory"]
env = { MEMORY_FILE_PATH = "{repo_root}/.thegn/memory/{workspace}.json" }

[mcp_servers.memory-graph.proxy]
tools = ["*"]
scope = "workspace"      # one memory per workspace ({workspace} in the path)

[[mcp_servers.memory-graph.grants]]
kind  = "npm:install"
scope = "@modelcontextprotocol/*"
"#,
    },
    Preset {
        name: "memory-mem0",
        category: "memory",
        description: "mem0 self-hosted memory (embeddings + vector store). Benchmarks well; \
                      needs an embedding/LLM API key — store it with `thegn mcp secret set`.",
        requires: &["OPENAI_API_KEY (embeddings/LLM) — or configure mem0 for a local model"],
        toml: r#"# mem0 memory server. Needs an embedding/LLM API key — keep it in the
# keyring (`thegn mcp secret set mem0-openai <key>`), NOT in this file.
[mcp_servers.memory-mem0]
command = ["uvx", "mem0-mcp"]
env = { OPENAI_API_KEY = "keyring:mem0-openai", MEM0_DIR = "{repo_root}/.thegn/mem0/{workspace}" }

[mcp_servers.memory-mem0.proxy]
tools = ["*"]
scope = "workspace"

[[mcp_servers.memory-mem0.grants]]
kind  = "process:exec"
scope = "uvx"
"#,
    },
    Preset {
        name: "filesystem",
        category: "dev",
        description: "Sandboxed filesystem access rooted at the worktree \
                      (@modelcontextprotocol/server-filesystem). Local, no API key.",
        requires: &[],
        toml: r#"# Filesystem tools rooted at the current worktree. Local; no API key.
# Default-deny keeps writes off unless you list them — start read-only.
[mcp_servers.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "{worktree}"]

[mcp_servers.filesystem.proxy]
tools = ["read_file", "read_multiple_files", "list_directory", "search_files", "directory_tree"]
scope = "worktree"

[[mcp_servers.filesystem.grants]]
kind  = "npm:install"
scope = "@modelcontextprotocol/*"
"#,
    },
];

/// Look a preset up by name.
pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::McpServerConfig;
    use std::collections::BTreeMap;

    #[derive(serde::Deserialize)]
    struct Doc {
        mcp_servers: BTreeMap<String, McpServerConfig>,
    }

    fn parse(p: &Preset) -> BTreeMap<String, McpServerConfig> {
        toml::from_str::<Doc>(p.toml)
            .unwrap_or_else(|e| panic!("preset `{}` toml invalid: {e}", p.name))
            .mcp_servers
    }

    #[test]
    fn every_preset_parses_with_grants_and_proxy() {
        assert!(!PRESETS.is_empty());
        for p in PRESETS {
            let servers = parse(p);
            // The block declares exactly the server named after the preset.
            let srv = servers.get(p.name).unwrap_or_else(|| {
                panic!(
                    "preset `{}` block must define [mcp_servers.{}]",
                    p.name, p.name
                )
            });
            assert!(
                !srv.grants.is_empty(),
                "preset `{}` must ship least-privilege grants",
                p.name
            );
            // Every preset opts into the proxy (else it is inert data).
            assert!(
                srv.is_proxy_exposed(),
                "preset `{}` must declare a proxy exposure",
                p.name
            );
        }
    }

    #[test]
    fn no_preset_hardcodes_a_secret_value() {
        // Secrets in presets must be refs (keyring:/env:/file:), never literals.
        for p in PRESETS {
            for (name, srv) in parse(p) {
                for (k, v) in &srv.env {
                    if k.to_ascii_uppercase().contains("KEY")
                        || k.to_ascii_uppercase().contains("TOKEN")
                        || k.to_ascii_uppercase().contains("SECRET")
                    {
                        assert!(
                            v.starts_with("keyring:")
                                || v.starts_with("env:")
                                || v.starts_with("file:"),
                            "preset `{}` server `{name}` env `{k}` looks like a literal secret",
                            p.name
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_local_memory_preset_exists() {
        // THE-49 mandate: at least one memory preset is fully local (no API key).
        let local_memory = PRESETS
            .iter()
            .filter(|p| p.category == "memory")
            .any(Preset::is_local);
        assert!(
            local_memory,
            "at least one memory preset must be fully local (no API key)"
        );
    }

    #[test]
    fn find_resolves_and_misses() {
        assert!(find("memory-graph").is_some());
        assert!(find("nope").is_none());
    }
}
