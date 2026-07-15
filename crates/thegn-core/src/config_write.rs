//! Comment/format-preserving config authoring — the *write* half of the config
//! system (the read/resolve half is `config.rs` + `config_resolve.rs`).
//!
//! `thegn env create`/`edit`, `config set`, and the TUI setup wizard all funnel
//! through here so they edit the user's `config.toml` **in place** — via
//! `toml_edit` — without clobbering comments or reformatting the file. Two rules
//! mirror the trust model in `config_resolve.rs`:
//!
//! - `[env.<name>]` tables are authored in the **global** config only (a repo
//!   `.thegn.toml` may *select* an env with a top-level `env = "…"` but never
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

/// Set one dotted key to a string **array** (`repo_roots = ["a", "b"]`),
/// creating intermediate tables. The array-valued sibling of [`set_key`].
pub fn set_string_array(config_path: &Path, dotted: &str, items: &[String]) -> Result<()> {
    let parts: Vec<&str> = dotted.split('.').filter(|s| !s.is_empty()).collect();
    anyhow::ensure!(!parts.is_empty(), "empty key");
    let mut doc = read_doc(config_path)?;
    let mut tbl = doc.as_table_mut();
    for seg in &parts[..parts.len() - 1] {
        tbl = subtable(tbl, seg);
    }
    let mut arr = toml_edit::Array::new();
    for it in items {
        arr.push(it.as_str());
    }
    tbl.insert(parts[parts.len() - 1], value(arr));
    write_doc(config_path, &doc)
}

/// Create or update a `[host.<name>]` ssh host (+ its `.ssh` subtable) in the
/// global config, preserving comments. `ssh` is the `user@box[:port]` form the
/// env/terminal wizards use; untouched sibling keys are kept on re-upsert.
pub fn upsert_host(config_path: &Path, name: &str, ssh: &str) -> Result<()> {
    anyhow::ensure!(!name.trim().is_empty(), "host name is empty");
    anyhow::ensure!(!ssh.trim().is_empty(), "ssh target is empty");
    let mut doc = read_doc(config_path)?;
    let root = doc.as_table_mut();
    let hosts = subtable(root, "host");
    hosts.set_implicit(true);
    let ht = subtable(hosts, name.trim());
    ht.insert("reach", value("ssh"));
    let st = subtable(ht, "ssh");
    st.insert("host", value(ssh.trim()));
    write_doc(config_path, &doc)
}

/// Insert `key = "<v>"` only when `v` is non-blank (trimmed).
fn put_str_ref(tbl: &mut Table, key: &str, v: &str) {
    if !v.trim().is_empty() {
        tbl.insert(key, value(v.trim()));
    }
}

/// Upsert one entry into an array-of-tables at the dotted `array` path (e.g.
/// `"forges"` → `[[forges]]`, or `"issues.issue_accounts"` →
/// `[[issues.issue_accounts]]`), keyed by its `name` field — updates the
/// matching entry in place or appends a new one, preserving comments and
/// sibling entries. `fill` sets the entry's fields (`name` is written for you).
fn upsert_named_array_entry(
    config_path: &Path,
    array: &str,
    name: &str,
    fill: impl FnOnce(&mut Table),
) -> Result<()> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "{array} entry name is empty");
    let parts: Vec<&str> = array.split('.').filter(|s| !s.is_empty()).collect();
    anyhow::ensure!(!parts.is_empty(), "empty array path");
    let mut doc = read_doc(config_path)?;
    let mut parent = doc.as_table_mut();
    for seg in &parts[..parts.len() - 1] {
        parent.set_implicit(true);
        parent = subtable(parent, seg);
    }
    let entry = parent
        .entry(parts[parts.len() - 1])
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    if !entry.is_array_of_tables() {
        *entry = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    let aot = entry.as_array_of_tables_mut().expect("just ensured aot");
    let idx = aot
        .iter()
        .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name));
    let tbl = match idx {
        Some(i) => aot.get_mut(i).expect("index in range"),
        None => {
            aot.push(Table::new());
            aot.iter_mut().last().expect("just pushed")
        }
    };
    tbl.insert("name", value(name));
    fill(tbl);
    write_doc(config_path, &doc)
}

/// Remove the entry named `name` from the array-of-tables at the dotted `array`
/// path. Ok if absent.
pub fn remove_array_entry(config_path: &Path, array: &str, name: &str) -> Result<()> {
    let parts: Vec<&str> = array.split('.').filter(|s| !s.is_empty()).collect();
    anyhow::ensure!(!parts.is_empty(), "empty array path");
    let mut doc = read_doc(config_path)?;
    let mut node: &mut Table = doc.as_table_mut();
    for seg in &parts[..parts.len() - 1] {
        match node.get_mut(seg).and_then(Item::as_table_mut) {
            Some(t) => node = t,
            None => return write_doc(config_path, &doc), // path absent → nothing to remove
        }
    }
    if let Some(aot) = node
        .get_mut(parts[parts.len() - 1])
        .and_then(Item::as_array_of_tables_mut)
    {
        let idx = aot
            .iter()
            .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name.trim()));
        if let Some(i) = idx {
            aot.remove(i);
        }
    }
    write_doc(config_path, &doc)
}

/// Create or update a `[[issue_accounts]]` entry (keyed by `name`), preserving
/// comments + sibling entries. `token_ref` must be a SecretRef / `env:` ref
/// (never a raw token — see the module trust model); empty fields are omitted.
#[allow(clippy::too_many_arguments)] // one call site; a struct would cost more than it saves
pub fn upsert_issue_account(
    config_path: &Path,
    name: &str,
    provider: &str,
    token_ref: &str,
    team_id: &str,
    workspace_slug: &str,
    base_url: &str,
    email: &str,
    project_key: &str,
) -> Result<()> {
    upsert_named_array_entry(config_path, "issues.issue_accounts", name, |t| {
        t.insert("provider", value(provider));
        put_str_ref(t, "token", token_ref);
        put_str_ref(t, "team_id", team_id);
        put_str_ref(t, "workspace_slug", workspace_slug);
        put_str_ref(t, "base_url", base_url);
        put_str_ref(t, "email", email);
        put_str_ref(t, "project_key", project_key);
    })
}

/// Create or update a `[[forges]]` entry (keyed by `name`). `token_ref` must be
/// a SecretRef / `env:` ref; empty `host`/`token` are omitted.
pub fn upsert_forge(
    config_path: &Path,
    name: &str,
    kind: &str,
    host: &str,
    token_ref: &str,
) -> Result<()> {
    upsert_named_array_entry(config_path, "forges", name, |t| {
        t.insert("kind", value(kind));
        put_str_ref(t, "host", host);
        put_str_ref(t, "token", token_ref);
    })
}

/// In a **repo** `.thegn.toml`, select an env with a top-level `env = "<name>"`
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
        std::fs::write(&p, "# my thegn config\n[sandbox]\nbackend = \"bwrap\"\n").unwrap();
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
        assert!(out.contains("# my thegn config"), "comment preserved");
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
    fn set_string_array_round_trips_and_preserves_comments() {
        let p = tmp("array.toml");
        std::fs::write(&p, "# keep me\nworktrees_dir = \"~/wt\"\n").unwrap();
        set_string_array(&p, "repo_roots", &["~/code".into(), "~/oss".into()]).unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# keep me"), "comment preserved");
        let doc = out.parse::<DocumentMut>().unwrap();
        let arr = doc["repo_roots"].as_array().unwrap();
        let items: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(items, ["~/code", "~/oss"]);
        // Overwrite replaces the whole array.
        set_string_array(&p, "repo_roots", &["~/work".into()]).unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(doc["repo_roots"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn upsert_host_writes_reach_and_ssh_and_keeps_siblings() {
        let p = tmp("host.toml");
        std::fs::write(
            &p,
            "# cfg\n[host.other]\nreach = \"ssh\"\n[host.other.ssh]\nhost = \"me@old\"\n",
        )
        .unwrap();
        upsert_host(&p, "build-box", "me@build.example.com:2222").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# cfg"), "comment preserved");
        let doc = out.parse::<DocumentMut>().unwrap();
        assert_eq!(doc["host"]["build-box"]["reach"].as_str(), Some("ssh"));
        assert_eq!(
            doc["host"]["build-box"]["ssh"]["host"].as_str(),
            Some("me@build.example.com:2222")
        );
        assert_eq!(
            doc["host"]["other"]["ssh"]["host"].as_str(),
            Some("me@old"),
            "sibling host untouched"
        );
        // Re-upsert updates in place (no duplicate table).
        upsert_host(&p, "build-box", "me@new").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["host"]["build-box"]["ssh"]["host"].as_str(),
            Some("me@new")
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn upsert_host_rejects_empty_inputs() {
        let p = tmp("host-empty.toml");
        assert!(upsert_host(&p, " ", "me@box").is_err());
        assert!(upsert_host(&p, "box", "  ").is_err());
    }

    #[test]
    fn upsert_issue_account_appends_updates_and_removes() {
        let p = tmp("issue-accts.toml");
        std::fs::write(&p, "# keep\n[issues]\nprovider = \"none\"\n").unwrap();
        upsert_issue_account(&p, "work", "linear", "keyring:work", "TEAM", "", "", "", "").unwrap();
        upsert_issue_account(&p, "home", "linear", "env:LIN", "", "", "", "", "").unwrap();
        let out = std::fs::read_to_string(&p).unwrap();
        assert!(out.contains("# keep"), "comment preserved");
        let doc = out.parse::<DocumentMut>().unwrap();
        let aot = doc["issues"]["issue_accounts"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(aot.len(), 2, "two distinct accounts of one provider");
        assert_eq!(aot.get(0).unwrap()["name"].as_str(), Some("work"));
        assert_eq!(aot.get(0).unwrap()["token"].as_str(), Some("keyring:work"));
        assert_eq!(aot.get(0).unwrap()["team_id"].as_str(), Some("TEAM"));
        // The pre-existing `[issues] provider` scalar is preserved.
        assert_eq!(doc["issues"]["provider"].as_str(), Some("none"));
        // Empty scope fields are omitted.
        assert!(aot.get(1).unwrap().get("team_id").is_none());

        // Upsert by name updates in place (no duplicate).
        upsert_issue_account(&p, "work", "linear", "keyring:work2", "T2", "", "", "", "").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let aot = doc["issues"]["issue_accounts"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(aot.len(), 2, "upsert did not duplicate");
        assert_eq!(aot.get(0).unwrap()["token"].as_str(), Some("keyring:work2"));

        // Remove by name.
        remove_array_entry(&p, "issues.issue_accounts", "home").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            doc["issues"]["issue_accounts"]
                .as_array_of_tables()
                .unwrap()
                .len(),
            1
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn upsert_forge_writes_kind_and_host() {
        let p = tmp("forges.toml");
        let _ = std::fs::remove_file(&p);
        upsert_forge(&p, "ghe", "ghe", "git.corp.example", "env:GHE").unwrap();
        let doc = std::fs::read_to_string(&p)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let f = doc["forges"].as_array_of_tables().unwrap().get(0).unwrap();
        assert_eq!(f["name"].as_str(), Some("ghe"));
        assert_eq!(f["kind"].as_str(), Some("ghe"));
        assert_eq!(f["host"].as_str(), Some("git.corp.example"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn upsert_issue_account_rejects_empty_name() {
        let p = tmp("issue-empty.toml");
        assert!(upsert_issue_account(&p, " ", "linear", "", "", "", "", "", "").is_err());
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
