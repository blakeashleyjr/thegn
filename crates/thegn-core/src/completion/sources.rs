//! The value sources behind [`CATALOG`](super::CATALOG).
//!
//! A source is a seam in the repo's sense: an object-safe trait, a `kind` that
//! is implemented-or-`reserved`, and no vendor knowledge outside its own
//! implementation. Three families:
//!
//! - **DB-derived** ([`DbSource`]) — the only I/O in this module. See the
//!   read-only contract below; it is the load-bearing part of the whole change.
//! - **config-derived** ([`ConfigSource`], and the pure
//!   [`config_candidates`] behind it) — pure functions over an already-loaded
//!   [`Config`], so the caller decides when (and whether) to pay for the load.
//! - **in-process** ([`StaticSource`]) — theme presets plus bounded local theme
//!   metadata, capability ids, and action ids. This path never creates state.
//!
//! ## The read-only contract
//!
//! Pressing `<TAB>` on a machine that has never run thegn must leave the
//! filesystem untouched. So the DB is opened with
//! `OpenFlags::SQLITE_OPEN_READ_ONLY` — the **first** `OpenFlags` use in this
//! codebase, and deliberately so:
//!
//! - [`crate::db::Db::open`] creates the state directory and runs a one-shot
//!   startup prune;
//! - `Db::init` sets `journal_mode=WAL`, which is a **header write**, runs
//!   migrations, and takes a 5 s busy timeout.
//!
//! Any of those on a keypress is a contract violation, so this path never calls
//! them. `SQLITE_OPEN_READ_ONLY` (no `CREATE`) makes "the DB does not exist" an
//! open error rather than a fresh file, and the busy timeout is 50 ms rather
//! than 5 s because the user is waiting with their hand on the keyboard.
//!
//! Every failure — missing file, locked DB, unrecognised schema, a column that
//! moved — yields an **empty vector**, never an error. The shell then falls
//! back to filename completion, which is exactly today's behaviour.

use std::path::{Path, PathBuf};

use super::Deadline;
use super::candidate::Candidate;
use super::catalog::SourceKind;
use crate::config::Config;

const MAX_THEME_FILES: usize = 256;
const MAX_THEME_FILE_BYTES: usize = 64 * 1024;

/// Busy timeout for the read-only handle. Short on purpose: a `<TAB>` that
/// blocks behind a compositor's write transaction should give up and complete
/// nothing rather than stall the keypress.
const BUSY_TIMEOUT_MS: u64 = 50;

/// A source of candidate values for one [`SourceKind`].
///
/// Object-safe (no generics, no `async fn`) so the host can hold a
/// `Box<dyn CompletionSource>` per slot and pay for it only when that slot is
/// the one being completed.
pub trait CompletionSource: Send + Sync {
    /// Which kind this source serves.
    fn kind(&self) -> SourceKind;

    /// Candidates for the word being completed. `current` is the partial word;
    /// implementations may use it to bound their own work, but need not filter
    /// on it — [`super::candidate::refine`] is the authority on filtering.
    ///
    /// Returning an empty vector is always a valid answer, and is what every
    /// error path does.
    fn candidates(&self, current: &str, deadline: &Deadline) -> Vec<Candidate>;
}

// ── DB-derived ───────────────────────────────────────────────────────────────

/// The state DB's path.
///
/// Mirrors the private `db::db_path` deliberately: taking a dependency on the
/// `Db` type is exactly the mistake this path must not make (see the module
/// doc), and this is a one-line join that has not moved in the schema's life.
pub fn state_db_path() -> PathBuf {
    crate::util::xdg_state_home().join("thegn/thegn.db")
}

/// A read-only reader over the state DB, serving one [`SourceKind`].
pub struct DbSource {
    kind: SourceKind,
    path: PathBuf,
}

impl DbSource {
    /// A source for `kind` over the process's state DB.
    ///
    /// # Panics
    /// Never at runtime for a catalog-derived kind; debug-asserts that `kind`
    /// is actually DB-derived, which is a catalog bug if it fires.
    pub fn new(kind: SourceKind) -> Self {
        Self::at(kind, state_db_path())
    }

    /// A source for `kind` over an explicit DB path (tests, and any future
    /// profile-scoped caller that resolves the path itself).
    pub fn at(kind: SourceKind, path: impl Into<PathBuf>) -> Self {
        debug_assert!(kind.reads_db(), "{kind:?} is not a DB-derived kind");
        Self {
            kind,
            path: path.into(),
        }
    }

    /// The query for this kind: `(value, description)`, most useful first.
    ///
    /// `expires_at` on a lease is stored in **milliseconds** (see
    /// `db_control::put_lease`), hence the `* 1000`.
    fn query(&self) -> &'static str {
        match self.kind {
            SourceKind::Worktree => {
                "SELECT worktree, COALESCE(branch, '') FROM worktrees \
                 ORDER BY created_at DESC"
            }
            SourceKind::Repo => {
                "SELECT COALESCE(name, ''), path FROM repos \
                 ORDER BY last_opened DESC"
            }
            SourceKind::Session => {
                "SELECT DISTINCT session_id, '' FROM session_leases \
                 WHERE expires_at IS NULL OR expires_at > strftime('%s', 'now') * 1000 \
                 ORDER BY session_id"
            }
            SourceKind::Host => {
                "SELECT host_id, COALESCE(name, '') FROM hosts ORDER BY last_used DESC"
            }
            // Unreachable via `new`/`at` (debug-asserted); an empty query is
            // the fail-open answer rather than a panic on a <TAB>.
            _ => "",
        }
    }

    /// Open the DB read-only. `None` for every failure — see the module doc.
    fn open(path: &Path) -> Option<rusqlite::Connection> {
        // READ_ONLY, and NOT `Db::open`/`open_at`: no dir creation, no WAL
        // pragma, no migration, no startup prune. NO_MUTEX matches rusqlite's
        // own default threading mode; without CREATE, a missing file is an
        // error here instead of a new (empty, unmigrated) DB on disk.
        let flags =
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = rusqlite::Connection::open_with_flags(path, flags).ok()?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
            .ok()?;
        Some(conn)
    }
}

impl CompletionSource for DbSource {
    fn kind(&self) -> SourceKind {
        self.kind
    }

    fn candidates(&self, _current: &str, deadline: &Deadline) -> Vec<Candidate> {
        let sql = self.query();
        if sql.is_empty() || deadline.expired() {
            return Vec::new();
        }
        let Some(conn) = Self::open(&self.path) else {
            return Vec::new();
        };
        // Every step from here is `ok()?`-shaped: an unrecognised schema (a
        // renamed column, a table this build predates) must complete nothing,
        // not fail the keypress.
        let rows = (|| -> Option<Vec<Candidate>> {
            let mut stmt = conn.prepare(sql).ok()?;
            let mapped = stmt
                .query_map([], |r| {
                    Ok(Candidate::described(
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                    ))
                })
                .ok()?;
            Some(
                mapped
                    .filter_map(Result::ok)
                    .take(super::MAX_CANDIDATES * 4)
                    .collect(),
            )
        })();
        let mut out = rows.unwrap_or_default();
        // `wt rm` takes "a worktree path OR a branch name", and the branch is
        // already in the row we read — so offer it too. This is NOT the
        // reserved `branch` source: that one is about arbitrary git refs, which
        // would need a `git` invocation. Nothing here leaves the DB.
        if self.kind == SourceKind::Worktree {
            let branches: Vec<Candidate> = out
                .iter()
                .filter(|c| c.description.is_some())
                .map(|c| Candidate::described(c.description.clone().unwrap_or_default(), &c.value))
                .collect();
            out.extend(branches);
        }
        // Same deal for repos: `thegn open` resolves a bare basename as well as
        // a path, and the name column can be empty for a path-only row.
        if self.kind == SourceKind::Repo {
            let paths: Vec<Candidate> = out
                .iter()
                .map(|c| Candidate::described(c.description.clone().unwrap_or_default(), &c.value))
                .collect();
            out.retain(|c| !c.value.is_empty());
            out.extend(paths);
        }
        out
    }
}

// ── config-derived ───────────────────────────────────────────────────────────

/// Candidates a config-derived kind yields for `cfg`. A pure function over the
/// config struct: no I/O, so the whole family unit-tests off `Config::default`
/// plus a literal.
pub fn config_candidates(kind: SourceKind, cfg: &Config) -> Vec<Candidate> {
    match kind {
        SourceKind::Env => cfg.env.keys().map(Candidate::new).collect(),
        SourceKind::Profile => cfg.profiles.keys().map(Candidate::new).collect(),
        SourceKind::Agent => cfg
            .agents
            .iter()
            .map(|a| Candidate::described(&a.name, &a.command))
            .collect(),
        SourceKind::Tool => cfg
            .tools
            .iter()
            .map(|t| Candidate::described(&t.name, &t.command))
            .collect(),
        SourceKind::Plugin => cfg
            .plugins
            .iter()
            .map(|p| Candidate::described(p.manifest.id.as_str(), &p.manifest.name))
            .collect(),
        SourceKind::Stage => cfg
            .pipeline
            .stages
            .iter()
            .filter_map(|s| s.stage_name().map(|n| Candidate::described(n, &s.agent)))
            .collect(),
        SourceKind::McpServer => cfg.mcp_servers.keys().map(Candidate::new).collect(),
        SourceKind::ConfigKey => config_key_candidates(cfg),
        _ => Vec::new(),
    }
}

/// Every dotted key `thegn config get|set` (and `--set KEY=VALUE`) accepts,
/// derived from the config's own serialization so it can never drift from the
/// struct. Both leaves (`theme.accent`) and the tables above them (`theme`) are
/// offered — `config get theme` prints the whole table.
pub fn config_key_candidates(cfg: &Config) -> Vec<Candidate> {
    let Ok(value) = serde_json::to_value(cfg) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    flatten_keys(&value, &mut String::new(), &mut out);
    out
}

/// Depth-first walk emitting `a`, `a.b`, `a.b.c`. Arrays are leaves: their
/// elements are addressed by index, which is not a thing `config set` takes.
fn flatten_keys(value: &serde_json::Value, prefix: &mut String, out: &mut Vec<Candidate>) {
    let serde_json::Value::Object(map) = value else {
        return;
    };
    for (k, v) in map {
        let base = prefix.len();
        if base > 0 {
            prefix.push('.');
        }
        prefix.push_str(k);
        out.push(Candidate::new(prefix.as_str()));
        flatten_keys(v, prefix, out);
        prefix.truncate(base);
    }
}

/// A [`CompletionSource`] over an already-loaded config. Borrowed, so the host
/// loads the config at most once per request no matter how many config-derived
/// slots are involved.
pub struct ConfigSource<'a> {
    kind: SourceKind,
    cfg: &'a Config,
}

impl<'a> ConfigSource<'a> {
    pub fn new(kind: SourceKind, cfg: &'a Config) -> Self {
        debug_assert!(kind.reads_config(), "{kind:?} is not a config-derived kind");
        Self { kind, cfg }
    }
}

impl CompletionSource for ConfigSource<'_> {
    fn kind(&self) -> SourceKind {
        self.kind
    }

    fn candidates(&self, _current: &str, deadline: &Deadline) -> Vec<Candidate> {
        if deadline.expired() {
            return Vec::new();
        }
        config_candidates(self.kind, self.cfg)
    }
}

// ── in-process ───────────────────────────────────────────────────────────────

/// The selectable merged built-in and local theme catalog.
pub fn theme_candidates() -> Vec<Candidate> {
    let mut out: Vec<Candidate> = crate::theme::PRESETS
        .iter()
        .map(|p| Candidate::new(*p))
        .collect();
    let builtins: std::collections::HashSet<&str> = crate::theme::PRESETS.iter().copied().collect();
    let dir = crate::util::xdg_config_home().join("thegn/themes");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut users = Vec::new();
    for entry in entries.flatten().take(MAX_THEME_FILES) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml")
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if size > MAX_THEME_FILE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(theme) = crate::theme_user::UserTheme::from_toml(&text) else {
            continue;
        };
        if !builtins.contains(theme.meta.name.as_str()) {
            users.push(theme.meta.name);
        }
    }
    users.sort();
    out.extend(users.into_iter().map(Candidate::new));
    out
}

/// Capability ids, from the one capability catalog. This is the right direction
/// of projection: completion consumes the catalog, it does not add a row to it.
pub fn capability_candidates() -> Vec<Candidate> {
    crate::capability::CATALOG
        .iter()
        .map(|c| Candidate::described(c.id.as_str(), c.summary))
        .collect()
}

/// Bindable action ids, from the keymap registry.
pub fn action_candidates() -> Vec<Candidate> {
    crate::keymap::BUILTINS
        .iter()
        .map(|a| Candidate::described(a.id, a.menu_label))
        .collect()
}

/// Candidates for a kind that needs neither the DB nor the config.
pub fn in_process_candidates(kind: SourceKind) -> Vec<Candidate> {
    match kind {
        SourceKind::Theme => theme_candidates(),
        SourceKind::Capability => capability_candidates(),
        SourceKind::Action => action_candidates(),
        _ => Vec::new(),
    }
}

/// A [`CompletionSource`] over a fixed list — the in-process kinds, and a
/// convenient shape for tests.
pub struct StaticSource {
    kind: SourceKind,
    values: Vec<Candidate>,
}

impl StaticSource {
    /// The in-process source for `kind`.
    pub fn new(kind: SourceKind) -> Self {
        Self {
            kind,
            values: in_process_candidates(kind),
        }
    }

    /// An explicit list (tests).
    pub fn with(kind: SourceKind, values: Vec<Candidate>) -> Self {
        Self { kind, values }
    }
}

impl CompletionSource for StaticSource {
    fn kind(&self) -> SourceKind {
        self.kind
    }

    fn candidates(&self, _current: &str, deadline: &Deadline) -> Vec<Candidate> {
        if deadline.expired() {
            return Vec::new();
        }
        self.values.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::candidate::refine;
    use crate::db::Db;

    fn budget() -> Deadline {
        Deadline::new(60_000)
    }

    fn expired() -> Deadline {
        Deadline::starting_at(
            std::time::Instant::now() - std::time::Duration::from_secs(1),
            1,
        )
    }

    /// A seeded temp DB, built through the real `Db::open_at` path (the
    /// `db_tests.rs` fixture shape) so the schema is genuinely the current one.
    fn seeded() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state/thegn.db");
        {
            let db = Db::open_at(&path).expect("open");
            let conn = db.conn();
            conn.execute(
                "INSERT INTO worktrees (worktree, session_name, tab_name, repo_path, branch, created_at) \
                 VALUES ('/wt/alpha', 's', 'alpha', '/code/alpha', 'tg/alpha', 20)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO worktrees (worktree, session_name, tab_name, repo_path, branch, created_at) \
                 VALUES ('/wt/beta', 's', 'beta', '/code/beta', '', 10)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repos (path, name, last_opened) VALUES ('/code/alpha', 'alpha', 20)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repos (path, name, last_opened) VALUES ('/code/beta', '', 10)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO hosts (host_id, name, last_used) VALUES ('h-1', 'builder', 5)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_leases (session_id, daemon_id, kind, created_at, expires_at) \
                 VALUES ('live-1', 'd', 'relay', 1, NULL)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session_leases (session_id, daemon_id, kind, created_at, expires_at) \
                 VALUES ('gone-1', 'd', 'relay', 1, 1)",
                [],
            )
            .unwrap();
        }
        (dir, path)
    }

    fn values(kind: SourceKind, path: &Path, current: &str) -> Vec<String> {
        let src = DbSource::at(kind, path);
        refine(src.candidates(current, &budget()), current)
            .into_iter()
            .map(|c| c.value)
            .collect()
    }

    #[test]
    fn worktrees_offer_paths_and_their_recorded_branches() {
        let (_d, path) = seeded();
        let all = values(SourceKind::Worktree, &path, "");
        assert!(all.contains(&"/wt/alpha".to_string()));
        assert!(all.contains(&"/wt/beta".to_string()));
        // The branch column is offered too — `wt rm` takes either — and the
        // empty branch on /wt/beta does not become a blank candidate.
        assert!(all.contains(&"tg/alpha".to_string()));
        assert!(!all.iter().any(String::is_empty));
        // Newest first.
        assert_eq!(all[0], "/wt/alpha");
        assert_eq!(values(SourceKind::Worktree, &path, "tg/"), ["tg/alpha"]);
    }

    #[test]
    fn repos_offer_names_and_paths() {
        let (_d, path) = seeded();
        let all = values(SourceKind::Repo, &path, "");
        assert!(all.contains(&"alpha".to_string()));
        assert!(all.contains(&"/code/alpha".to_string()));
        // The name-less row still completes by path, and never as "".
        assert!(all.contains(&"/code/beta".to_string()));
        assert!(!all.iter().any(String::is_empty));
    }

    #[test]
    fn sessions_exclude_expired_leases_and_hosts_come_from_the_hosts_table() {
        let (_d, path) = seeded();
        assert_eq!(values(SourceKind::Session, &path, ""), ["live-1"]);
        let hosts = values(SourceKind::Host, &path, "");
        assert_eq!(hosts, ["h-1"]);
        let described = DbSource::at(SourceKind::Host, &path).candidates("", &budget());
        assert_eq!(described[0].description.as_deref(), Some("builder"));
    }

    #[test]
    fn a_missing_or_unreadable_db_completes_nothing_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope/thegn.db");
        for kind in [
            SourceKind::Worktree,
            SourceKind::Repo,
            SourceKind::Session,
            SourceKind::Host,
        ] {
            assert!(
                DbSource::at(kind, &missing)
                    .candidates("", &budget())
                    .is_empty()
            );
        }
        // READ_ONLY without CREATE: the keypress left the filesystem alone.
        assert!(!missing.exists());
        assert!(!missing.parent().unwrap().exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        // A file that is not a database at all is the same non-event.
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, b"not a database").unwrap();
        assert!(
            DbSource::at(SourceKind::Repo, &junk)
                .candidates("", &budget())
                .is_empty()
        );
    }

    #[test]
    fn a_schema_without_the_expected_table_completes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("CREATE TABLE unrelated (x TEXT)", [])
            .unwrap();
        assert!(
            DbSource::at(SourceKind::Worktree, &path)
                .candidates("", &budget())
                .is_empty()
        );
    }

    #[test]
    fn an_expired_deadline_short_circuits_every_source() {
        let (_d, path) = seeded();
        let cfg = Config::default();
        assert!(
            DbSource::at(SourceKind::Repo, &path)
                .candidates("", &expired())
                .is_empty()
        );
        assert!(
            ConfigSource::new(SourceKind::Env, &cfg)
                .candidates("", &expired())
                .is_empty()
        );
        assert!(
            StaticSource::new(SourceKind::Theme)
                .candidates("", &expired())
                .is_empty()
        );
    }

    #[test]
    fn db_source_kind_round_trips_and_unknown_kinds_are_inert() {
        let (_d, path) = seeded();
        let src = DbSource::at(SourceKind::Repo, &path);
        assert_eq!(src.kind(), SourceKind::Repo);
        // Constructed past the debug assert (release builds), a non-DB kind has
        // no query and completes nothing rather than panicking on a <TAB>.
        let bogus = DbSource {
            kind: SourceKind::Theme,
            path,
        };
        assert!(bogus.candidates("", &budget()).is_empty());
    }

    #[test]
    fn config_sources_are_pure_over_the_config_struct() {
        let mut cfg = Config::default();
        cfg.env.insert("nix".into(), Default::default());
        cfg.env.insert("docker".into(), Default::default());
        cfg.profiles.insert("work".into(), Default::default());
        cfg.mcp_servers.insert("git".into(), Default::default());
        let named = |name: &str, command: &str| crate::config::NamedCommand {
            name: name.into(),
            command: command.into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
        };
        cfg.agents.push(named("claude", "claude --dangerously"));
        cfg.tools.push(named("lazygit", "lazygit"));

        let names = |k| {
            config_candidates(k, &cfg)
                .into_iter()
                .map(|c| c.value)
                .collect::<Vec<_>>()
        };
        // BTreeMap order: deterministic, which is what a shell list wants.
        assert_eq!(names(SourceKind::Env), ["docker", "nix"]);
        assert_eq!(names(SourceKind::Profile), ["work"]);
        assert_eq!(names(SourceKind::McpServer), ["git"]);
        assert_eq!(names(SourceKind::Agent), ["claude"]);
        assert_eq!(names(SourceKind::Tool), ["lazygit"]);
        assert_eq!(
            config_candidates(SourceKind::Agent, &cfg)[0]
                .description
                .as_deref(),
            Some("claude --dangerously")
        );
        // A kind this family does not serve is empty, not a panic.
        assert!(config_candidates(SourceKind::Worktree, &cfg).is_empty());
        assert!(config_candidates(SourceKind::Plugin, &cfg).is_empty());

        let src = ConfigSource::new(SourceKind::Env, &cfg);
        assert_eq!(src.kind(), SourceKind::Env);
        assert_eq!(src.candidates("", &budget()).len(), 2);
    }

    #[test]
    fn config_keys_come_from_the_config_struct_itself() {
        let cfg = Config::default();
        let keys: Vec<String> = config_key_candidates(&cfg)
            .into_iter()
            .map(|c| c.value)
            .collect();
        // Both the table and its leaves are addressable.
        assert!(keys.contains(&"theme".to_string()));
        assert!(keys.contains(&"theme.accent".to_string()));
        assert!(keys.contains(&"theme.colors".to_string()));
        assert!(keys.contains(&"base_branch".to_string()));
        // Every key is dotted, never padded, never empty.
        for k in &keys {
            assert!(
                !k.is_empty() && !k.starts_with('.') && !k.ends_with('.'),
                "{k}"
            );
        }
        // Prefix filtering is what makes the (large) list usable.
        let themed = refine(config_key_candidates(&cfg), "theme.");
        assert!(themed.len() >= 5);
        assert!(themed.iter().all(|c| c.value.starts_with("theme.")));
    }

    #[test]
    fn flatten_keys_ignores_non_object_roots() {
        let mut out = Vec::new();
        flatten_keys(&serde_json::json!(["a", "b"]), &mut String::new(), &mut out);
        assert!(out.is_empty());
        // Arrays are leaves: `[[agents]]` completes as `agents`, not `agents.0`.
        let mut out = Vec::new();
        flatten_keys(
            &serde_json::json!({"agents": [{"name": "x"}], "t": {"k": 1}}),
            &mut String::new(),
            &mut out,
        );
        let vals: Vec<&str> = out.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(vals, ["agents", "t", "t.k"]);
    }

    #[test]
    fn in_process_sources_project_the_existing_catalogs() {
        let themes: Vec<String> = theme_candidates().into_iter().map(|c| c.value).collect();
        assert!(themes.contains(&"prism".to_string()));
        assert!(themes.len() >= crate::theme::PRESETS.len());

        let caps = capability_candidates();
        assert_eq!(caps.len(), crate::capability::CATALOG.len());
        assert!(caps.iter().any(|c| c.value == "sessions.list"));
        assert!(caps.iter().all(|c| c.description.is_some()));

        let actions = action_candidates();
        assert_eq!(actions.len(), crate::keymap::BUILTINS.len());
        assert!(actions.iter().any(|c| c.value == "new-worktree"));

        assert_eq!(
            in_process_candidates(SourceKind::Theme)
                .iter()
                .filter(|candidate| crate::theme::PRESETS.contains(&candidate.value.as_str()))
                .count(),
            crate::theme::PRESETS.len()
        );
        assert!(in_process_candidates(SourceKind::Worktree).is_empty());

        let s = StaticSource::new(SourceKind::Capability);
        assert_eq!(s.kind(), SourceKind::Capability);
        assert_eq!(s.candidates("", &budget()).len(), caps.len());
        let explicit = StaticSource::with(SourceKind::Action, vec![Candidate::new("x")]);
        assert_eq!(explicit.candidates("", &budget()).len(), 1);
    }

    #[test]
    fn state_db_path_lives_under_the_state_home() {
        // Not spelled out as a literal: `xdg_state_home` is per-OS, and this
        // module must not grow a platform `#[cfg]` to assert its own path.
        let p = state_db_path();
        assert!(p.ends_with("thegn/thegn.db"), "{}", p.display());
        assert_eq!(p, crate::util::xdg_state_home().join("thegn/thegn.db"));
        // `DbSource::new` is `at(kind, state_db_path())` and opens nothing until
        // it is asked for candidates, so constructing one here is free.
        assert_eq!(DbSource::new(SourceKind::Repo).kind(), SourceKind::Repo);
    }
}
