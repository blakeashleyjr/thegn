//! Comment/format-preserving config authoring — the *write* half of the config
//! system (the read/resolve half is `config.rs` + `config_resolve.rs`).
//!
//! `superzej env create`/`edit`, `config set`, and the TUI setup wizard all funnel
//! through here so they edit the user's `config.toml` **in place** — via
//! `toml_edit` — without clobbering comments or reformatting the file. Two rules
//! mirror the trust model in `config_resolve.rs`:
//!
//! - `[env.<name>]` tables are authored in the **global** config only (a repo
//!   `.superzej.toml` may *select* an env with a top-level `env = "…"` but never
//!   *define* one — see [`select_env_in_repo`]).
//! - Tokens are never written here: [`EnvSpec::api_key_env`] holds a *SecretRef*
//!   (`keyring:` / `file:` / `env:`), produced by the host's secret backend.

use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, value};

/// A typed environment to author into the global config. Optional fields are
/// only written when `Some`, so `edit` upserts without dropping untouched keys.
#[derive(Debug, Clone, Default)]
pub struct EnvSpec {
    pub name: String,
    /// `"local"` | `"ssh"` | `"provider"`.
    pub placement: String,
    /// e.g. `"in_env"` — where files live (optional).
    pub data: Option<String>,
    // provider placement
    pub provider: Option<String>,
    pub api_key_env: Option<String>,
    pub region: Option<String>,
    pub size: Option<String>,
    pub template: Option<String>,
    pub max_instances: Option<i64>,
    pub max_lifetime_secs: Option<i64>,
    pub auto_provision: Option<bool>,
    // ssh placement
    pub ssh_host: Option<String>,
    // local placement
    pub sandbox_backend: Option<String>,
}

fn read_doc(path: &Path) -> Result<DocumentMut> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.parse::<DocumentMut>()
        .with_context(|| format!("parse {}", path.display()))
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).with_context(|| format!("write {}", path.display()))
}

/// Get-or-create a child table named `key` under `parent`, marking it implicit
/// (so an empty table isn't emitted as a bare `[header]`).
fn subtable<'a>(parent: &'a mut Table, key: &str) -> &'a mut Table {
    let entry = parent
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()));
    if !entry.is_table() {
        *entry = Item::Table(Table::new());
    }
    entry.as_table_mut().expect("just ensured table")
}

/// Create or update `[env.<name>]` (+ its `.provider`/`.ssh`/`.sandbox` subtable)
/// in the global config at `config_path`, preserving all other content + comments.
pub fn upsert_env(config_path: &Path, spec: &EnvSpec) -> Result<()> {
    anyhow::ensure!(!spec.name.trim().is_empty(), "env name is empty");
    let mut doc = read_doc(config_path)?;
    let root = doc.as_table_mut();
    let env = subtable(root, "env");
    env.set_implicit(true);
    let et = subtable(env, &spec.name);
    et.insert("placement", value(&spec.placement));
    if let Some(d) = &spec.data {
        et.insert("data", value(d));
    }
    match spec.placement.as_str() {
        "provider" => {
            let p = subtable(et, "provider");
            put_str(p, "provider", &spec.provider);
            put_str(p, "api_key_env", &spec.api_key_env);
            put_str(p, "region", &spec.region);
            put_str(p, "size", &spec.size);
            put_str(p, "template", &spec.template);
            if let Some(n) = spec.max_instances {
                p.insert("max_instances", value(n));
            }
            if let Some(n) = spec.max_lifetime_secs {
                p.insert("max_lifetime_secs", value(n));
            }
            if let Some(b) = spec.auto_provision {
                p.insert("auto_provision", value(b));
            }
        }
        "ssh" => {
            let s = subtable(et, "ssh");
            put_str(s, "host", &spec.ssh_host);
        }
        "local" => {
            if spec.sandbox_backend.is_some() {
                let sb = subtable(et, "sandbox");
                put_str(sb, "backend", &spec.sandbox_backend);
            }
        }
        other => anyhow::bail!("unknown placement {other:?} (expected local|ssh|provider)"),
    }
    write_doc(config_path, &doc)
}

fn put_str(tbl: &mut Table, key: &str, v: &Option<String>) {
    if let Some(s) = v.as_ref().filter(|s| !s.trim().is_empty()) {
        tbl.insert(key, value(s.as_str()));
    }
}

/// Remove `[env.<name>]` from the global config. Ok if it wasn't there.
pub fn remove_env(config_path: &Path, name: &str) -> Result<()> {
    let mut doc = read_doc(config_path)?;
    if let Some(env) = doc.get_mut("env").and_then(Item::as_table_mut) {
        env.remove(name);
    }
    write_doc(config_path, &doc)
}

/// Set one dotted key (`a.b.c = "value"`) as a string, creating intermediate
/// tables. The general `config set` counterpart to `config get`.
pub fn set_key(config_path: &Path, dotted: &str, val: &str) -> Result<()> {
    let parts: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
    anyhow::ensure!(!parts.is_empty(), "empty key");
    let mut doc = read_doc(config_path)?;
    let mut tbl = doc.as_table_mut();
    for seg in &parts[..parts.len() - 1] {
        tbl = subtable(tbl, seg);
    }
    tbl.insert(parts[parts.len() - 1], value(val));
    write_doc(config_path, &doc)
}

/// In a **repo** `.superzej.toml`, select an env with a top-level `env = "<name>"`
/// (repos select, never define — this refuses to write any `[env.*]` table).
pub fn select_env_in_repo(repo_toml: &Path, name: &str) -> Result<()> {
    let mut doc = read_doc(repo_toml)?;
    doc.as_table_mut().insert("env", value(name));
    write_doc(repo_toml, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("sz-cfgwrite-{}-{name}", std::process::id()))
    }

    #[test]
    fn upsert_provider_env_round_trips_and_preserves_comments() {
        let p = tmp("provider.toml");
        std::fs::write(&p, "# my superzej config\n[sandbox]\nbackend = \"bwrap\"\n").unwrap();
        let spec = EnvSpec {
            name: "fly-dev".into(),
            placement: "provider".into(),
            data: Some("in_env".into()),
            provider: Some("fly".into()),
            api_key_env: Some("keyring:fly-dev".into()),
            region: Some("iad".into()),
            size: Some("shared-cpu-2x".into()),
            template: Some("image:ghcr.io/x:v1".into()),
            max_instances: Some(5),
            auto_provision: Some(true),
            ..Default::default()
        };
        upsert_env(&p, &spec).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# my superzej config"), "comment preserved");
        assert!(
            out.contains("backend = \"bwrap\""),
            "existing table preserved"
        );
        // Re-parse and verify the env landed.
        let doc = out.parse::<DocumentMut>().unwrap();
        let prov = &doc["env"]["fly-dev"]["provider"];
        assert_eq!(
            doc["env"]["fly-dev"]["placement"].as_str(),
            Some("provider")
        );
        assert_eq!(prov["provider"].as_str(), Some("fly"));
        assert_eq!(prov["api_key_env"].as_str(), Some("keyring:fly-dev"));
        assert_eq!(prov["region"].as_str(), Some("iad"));
        assert_eq!(prov["max_instances"].as_integer(), Some(5));
        assert_eq!(prov["auto_provision"].as_bool(), Some(true));

        // edit: change size only, other fields stay.
        let edit = EnvSpec {
            name: "fly-dev".into(),
            placement: "provider".into(),
            size: Some("performance-1x".into()),
            ..Default::default()
        };
        upsert_env(&p, &edit).unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["env"]["fly-dev"]["provider"]["size"].as_str(),
            Some("performance-1x")
        );
        assert_eq!(
            doc["env"]["fly-dev"]["provider"]["region"].as_str(),
            Some("iad"),
            "untouched key kept"
        );

        remove_env(&p, "fly-dev").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(
            doc.get("env").and_then(|e| e.get("fly-dev")).is_none(),
            "env removed"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn repo_select_writes_only_a_string() {
        let p = tmp("repo.toml");
        let _ = std::fs::remove_file(&p);
        select_env_in_repo(&p, "fly-dev").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert_eq!(out.trim(), "env = \"fly-dev\"");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn set_key_creates_nested_tables() {
        let p = tmp("set.toml");
        let _ = std::fs::remove_file(&p);
        set_key(&p, "sandbox.backend", "docker").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(doc["sandbox"]["backend"].as_str(), Some("docker"));
        let _ = std::fs::remove_file(&p);
    }
}
