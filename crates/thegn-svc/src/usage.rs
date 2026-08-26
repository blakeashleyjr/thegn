//! The usage-tracker I/O seam (roadmap V 300): find each harness's credential
//! homes, read their local state, and hand the bytes to the pure parsers in
//! [`thegn_core::usage`]. This is the untested-by-coverage edge (filesystem +
//! network); the parsing/formatting/discovery logic it feeds is unit-tested in
//! core.
//!
//! Graceful degradation is the whole contract: [`gather`] **never errors**. A
//! missing file, a disabled provider, a network failure, or an unparseable body
//! all yield that account as [`thegn_core::usage::UsageState::Unavailable`] with a short reason,
//! mirroring orca's `unavailable` bar. Accounts are independent — one failing
//! never hides the others.
//!
//! **Per credential home, not per provider.** Both Codex and Claude Code locate
//! their entire credential home from one env var (`CODEX_HOME` /
//! `CLAUDE_CONFIG_DIR`), which is how a machine ends up with several logins side
//! by side — thegn's own `[[accounts]]` switcher works by pointing those vars at
//! different directories, and Claude Code's `--profile` convention parks them
//! under `~/.claude-profiles/`. So the homes are enumerated
//! ([`candidate_homes`]) and each is gathered separately; reading only the one
//! home the env var currently points at would show one account out of eight.
//!
//! Source per provider (see `thegn_core::usage`): **Codex** reads the newest
//! rollout `.jsonl` (offline); **Claude** and **Antigravity** don't persist
//! window state, so they read the locally-stored OAuth token and make one
//! lightweight authenticated request — gated behind `[usage] allow_network`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use thegn_core::account;
use thegn_core::config::UsageConfig;
use thegn_core::usage::{self, AccountUsage, HomeCandidate, HomeOrigin, UsageHome};

/// Anthropic's undocumented per-account usage endpoint (what Claude Code's
/// `/usage` reads). Authenticated with the OAuth token from `.credentials.json`.
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Google Cloud Code quota summary (the closed-app path Antigravity companions use).
const ANTIGRAVITY_QUOTA_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
/// Cap every live fetch so a hung endpoint can't wedge the gather thread.
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);
/// How many live per-account fetches run at once. Small on purpose: this is a
/// background poll against someone else's rate-limited endpoint, and eight
/// simultaneous requests to ask "am I near my limit" would be a poor joke.
const FETCH_CONCURRENCY: usize = 4;

/// Parse a harness's usage bytes through the seam — the one dispatch point for
/// per-vendor usage parsing. Delegates to the same pure parsers in
/// `thegn_core::usage`, so routing the three call sites through here is
/// behaviour-identical while the vendor knowledge lives in one place.
fn harness_parse_usage(id: &str, bytes: &[u8], now: i64) -> Option<AccountUsage> {
    thegn_core::harness::harness(id).and_then(|h| h.parse_usage(bytes, now))
}

/// Gather usage for every tracked account. Runs on a blocking thread (the host
/// calls it from the blocking pool). Never panics or errors — unreadable
/// accounts come back `Unavailable`.
///
/// `registered` carries credential homes the caller already knows about (thegn's
/// own `[[accounts]]` registry, which lives behind the DB and so can't be read
/// from here). Pass an empty slice when there are none.
pub fn gather_with(cfg: &UsageConfig, registered: &[HomeCandidate]) -> Vec<AccountUsage> {
    if !cfg.enabled {
        return Vec::new();
    }
    let now = thegn_core::util::now();
    let client = build_client();
    let homes = usage::discover_homes(&candidate_homes(cfg, registered));

    let mut rows: Vec<AccountUsage> = Vec::new();
    // Codex is a local file read, so it costs nothing to do inline and keeps the
    // thread-scope below to the requests that actually benefit from it.
    let (net_homes, local_homes): (Vec<_>, Vec<_>) = homes
        .into_iter()
        .partition(|h| h.provider == "claude" || h.provider == "antigravity");
    for home in &local_homes {
        rows.push(match home.provider.as_str() {
            "codex" => codex_usage(home, now),
            other => AccountUsage::unavailable(other, other, "unknown provider"),
        });
    }
    for chunk in net_homes.chunks(FETCH_CONCURRENCY) {
        std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|home| {
                    let client = client.as_ref();
                    s.spawn(move || match home.provider.as_str() {
                        "claude" => claude_usage(cfg, client, home),
                        "antigravity" => antigravity_usage(cfg, client, home, now),
                        other => AccountUsage::unavailable(other, other, "unknown provider"),
                    })
                })
                .collect();
            for h in handles {
                // A panicking fetch must not poison the whole gather; the
                // account it was for simply reports unreadable.
                match h.join() {
                    Ok(row) => rows.push(row),
                    Err(_) => rows.push(AccountUsage::unavailable(
                        "claude",
                        "Claude",
                        "gather panicked",
                    )),
                }
            }
        });
    }
    // Rows come back grouped by local-vs-network above, so restore the
    // provider order the user configured before handing them to the UI.
    rows.sort_by_key(|r| {
        cfg.providers
            .iter()
            .position(|p| *p == r.provider)
            .unwrap_or(usize::MAX)
    });
    usage::dedupe_by_identity(rows)
}

/// [`gather_with`] with no caller-supplied homes.
pub fn gather(cfg: &UsageConfig) -> Vec<AccountUsage> {
    gather_with(cfg, &[])
}

// --- host-wide transcript token rollup -------------------------------------

/// Cap on transcript files read in one rollup. A long-lived machine accumulates
/// thousands; reading every one on a background poll is not worth it, and the
/// newest N describe the period the rollup is about anyway. When the cap bites,
/// the caller says so rather than presenting a truncated total as complete.
const MAX_TRANSCRIPTS: usize = 2000;

/// How far back a rollup reads. Deliberately its own constant rather than
/// `[usage] history_days`: that key sizes the per-window sparkline, and coupling
/// the two would mean turning the sparkline off silently shrank the token totals
/// to a day. A month is long enough to be a useful number and short enough that
/// the scan stays bounded on a busy machine.
const ROLLUP_DAYS: u64 = 30;

/// The outcome of a rollup scan.
pub struct RollupResult {
    pub rollup: thegn_core::usage_tokens::TokenRollup,
    /// Files skipped because [`MAX_TRANSCRIPTS`] was reached. Non-zero means the
    /// totals are a floor, not a total — never present them as complete.
    pub skipped: usize,
}

/// Aggregate token counts from the harnesses' local transcripts, host-wide.
///
/// **Not attributable to an account**, for two independent reasons: transcript
/// records carry no account field, and credential homes routinely share one
/// transcript directory (profiles commonly symlink `projects/` at a single
/// tree). Homes are canonicalized and de-duplicated here precisely because of
/// that sharing — without it, eight profiles pointing at one directory would
/// count every token eight times.
///
/// Only files modified within [`ROLLUP_DAYS`] are read, so the scan cost tracks
/// the window being reported rather than the machine's whole history.
pub fn token_rollup(cfg: &UsageConfig) -> Option<RollupResult> {
    if !cfg.enabled || !cfg.token_rollups {
        return None;
    }
    let homes = usage::discover_homes(&candidate_homes(cfg, &[]));
    let cutoff =
        std::time::SystemTime::now().checked_sub(Duration::from_secs(ROLLUP_DAYS * 86_400))?;

    // Canonicalize: `projects/` is frequently a symlink shared by every profile.
    let mut roots: Vec<PathBuf> = Vec::new();
    for home in homes.iter().filter(|h| h.provider == "claude") {
        let dir = home.dir.join("projects");
        let Ok(real) = dir.canonicalize() else {
            continue;
        };
        if !roots.contains(&real) {
            roots.push(real);
        }
    }

    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for root in &roots {
        collect_transcripts(root, cutoff, &mut files);
    }
    // Newest first, so the cap keeps the most relevant files.
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let skipped = files.len().saturating_sub(MAX_TRANSCRIPTS);
    files.truncate(MAX_TRANSCRIPTS);

    let mut rollup = thegn_core::usage_tokens::TokenRollup::default();
    // One `seen` set across every file: a resumed session re-writes earlier
    // responses into a new transcript, and per-file dedup would count them twice.
    let mut seen = std::collections::HashSet::new();
    for (_, path) in &files {
        let default_project = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if let Ok(bytes) = std::fs::read(path) {
            // The transcript token fold is Claude's `TOKENS` seam op; it
            // delegates to the same `usage_tokens::fold_transcript` as before.
            if let Some(h) = thegn_core::harness::harness("claude") {
                h.fold_transcript(&bytes, &default_project, &mut seen, &mut rollup);
            }
        }
    }
    Some(RollupResult { rollup, skipped })
}

/// Every `*.jsonl` under `root` modified at or after `cutoff`, with its mtime.
fn collect_transcripts(
    root: &Path,
    cutoff: std::time::SystemTime,
    out: &mut Vec<(std::time::SystemTime, PathBuf)>,
) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if mtime >= cutoff {
                out.push((mtime, path));
            }
        }
    }
}

// --- credential-home enumeration -------------------------------------------

/// Every credential home worth considering, before core's
/// [`usage::discover_homes`] dedupes and orders them. Four sources: the caller's
/// registry, each harness's own default home, a scan of the profile roots, and
/// the explicit `[[usage.accounts]]` entries (which come last so their labels
/// and `enabled = false` win the dedup).
pub(crate) fn candidate_homes(
    cfg: &UsageConfig,
    registered: &[HomeCandidate],
) -> Vec<HomeCandidate> {
    let mut out: Vec<HomeCandidate> = registered.to_vec();

    for provider_id in &cfg.providers {
        // Antigravity has no relocatable credential home — its token lives at
        // one fixed path — so it gets a single synthetic home rather than being
        // dropped for lack of an `account::Provider` entry.
        let dir = match account::provider(provider_id) {
            Some(p) => account::effective_config_dir(p).map(PathBuf::from),
            None if provider_id == "antigravity" => antigravity_home(),
            None => None,
        };
        if let Some(dir) = dir {
            out.push(HomeCandidate {
                provider: provider_id.clone(),
                dir,
                origin: HomeOrigin::Default,
                label: None,
                enabled: true,
            });
        }
    }

    if cfg.discover_profiles {
        let mut roots: Vec<PathBuf> = cfg
            .profile_roots
            .iter()
            .map(|r| PathBuf::from(thegn_core::util::expand_tilde(r)))
            .collect();
        // Wherever the ACTIVE home lives, its siblings are profiles too. This
        // matters because `~` is not fixed: a harness launched with its
        // credential home relocated often has `$HOME` relocated with it, so
        // `~/.claude-profiles` resolves *inside* one profile and the scan finds
        // nothing but the one home already known. Deriving the root from the
        // active home instead of from `$HOME` finds the other seven.
        for provider_id in &cfg.providers {
            let Some(p) = account::provider(provider_id) else {
                continue;
            };
            let Some(home) = account::effective_config_dir(p).map(PathBuf::from) else {
                continue;
            };
            // Deliberately NOT guarded on "is the parent `$HOME`". A harness
            // launched under a relocated credential home usually has `$HOME`
            // relocated to the profile dir itself, so that test suppresses
            // exactly the case it is meant to allow — which is how this first
            // ran finding one account out of eight. The guard that matters is
            // structural: skip a root with no parent of its own (`/`), and let
            // `scan_profile_root` do the rest. It accepts only children that
            // actually hold the provider's auth marker, so a derived root of
            // `/home` finds that user on a single-user box and, on a shared
            // one, nothing it is allowed to read.
            if home.file_name().is_some_and(|n| n == p.default_dir)
                && let Some(root) = home.parent().and_then(Path::parent)
                && root.parent().is_some()
                && !roots.contains(&root.to_path_buf())
            {
                roots.push(root.to_path_buf());
            }
        }
        for root in &roots {
            out.extend(scan_profile_root(root, &cfg.providers));
        }
    }

    for a in &cfg.accounts {
        if a.dir.trim().is_empty() || a.provider.trim().is_empty() {
            continue;
        }
        let label = [a.label.as_str(), a.name.as_str()]
            .into_iter()
            .find(|s| !s.trim().is_empty())
            .map(str::to_string);
        out.push(HomeCandidate {
            provider: a.provider.clone(),
            dir: PathBuf::from(thegn_core::util::expand_tilde(&a.dir)),
            origin: HomeOrigin::Configured,
            label,
            enabled: a.enabled,
        });
    }
    out
}

/// Scan one profile root for credential homes: for each immediate child, a
/// provider claims it when its auth marker is present either at the child itself
/// or one level in under the provider's default dir name.
///
/// Both layouts are real. `~/.claude-profiles/<name>` is a fake `$HOME`, so the
/// credentials are at `<name>/.claude/.credentials.json`; but a directory
/// adopted as a `CLAUDE_CONFIG_DIR` holds them at its own top level. Checking
/// only one of the two would miss half the setups.
fn scan_profile_root(root: &Path, providers: &[String]) -> Vec<HomeCandidate> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new(); // absent root is the normal case, not an error
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let child = entry.path();
        if !child.is_dir() {
            continue;
        }
        for provider_id in providers {
            let Some(p) = account::provider(provider_id) else {
                continue;
            };
            let nested = child.join(p.default_dir);
            let home = if nested.join(p.auth_marker).is_file() {
                nested
            } else if child.join(p.auth_marker).is_file() {
                child.clone()
            } else {
                continue;
            };
            out.push(HomeCandidate {
                provider: provider_id.clone(),
                dir: home,
                origin: HomeOrigin::Discovered,
                label: None,
                enabled: true,
            });
        }
    }
    out
}

/// A blocking HTTP client for the live fetches, or `None` if it can't be built
/// (then the network-backed providers degrade to `Unavailable`).
fn build_client() -> Option<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent("thegn-usage")
        .build()
        .ok()
}

// --- Codex: offline, newest rollup ----------------------------------------

/// The `sessions/` dir under a Codex credential home, if it exists.
fn codex_sessions_dir(home: &Path) -> Option<PathBuf> {
    let dir = home.join("sessions");
    dir.is_dir().then_some(dir)
}

/// The most-recently-modified `rollout-*.jsonl` under `root` (recursively).
fn newest_rollout(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
                best = Some((mtime, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn codex_usage(home: &UsageHome, now: i64) -> AccountUsage {
    let label = home.fallback_label(".codex");
    let unavailable =
        |note: &str| AccountUsage::unavailable("codex", &label, note).with_home(&home.dir);
    let Some(dir) = codex_sessions_dir(&home.dir) else {
        return unavailable("not installed");
    };
    let Some(file) = newest_rollout(&dir) else {
        return unavailable("no sessions");
    };
    match std::fs::read(&file) {
        // The per-vendor usage parser now lives behind the harness seam (one
        // dispatch point); it delegates to the same `usage::parse_codex_rollup`
        // as before, so this is behaviour-identical.
        Ok(bytes) => match harness_parse_usage("codex", &bytes, now) {
            // The parser labels the row "Codex"; a multi-home box needs the
            // home's own name to tell two Codex logins apart.
            Some(mut u) => {
                u.account_label = label;
                u.with_home(&home.dir)
            }
            None => unavailable("no rate-limit data"),
        },
        Err(_) => unavailable("unreadable session"),
    }
}

// --- Claude: local token + identity + live GET ----------------------------

/// Read `<home>/.credentials.json` and extract the OAuth token + plan.
fn claude_credentials(home: &Path) -> Option<thegn_core::usage::ClaudeCreds> {
    let bytes = std::fs::read(home.join(".credentials.json")).ok()?;
    usage::parse_claude_credentials(&bytes)
}

/// Read `<home>/.claude.json` for the account's identity (email / org / tiers).
/// Absent on a home that has only ever been used headlessly, hence optional.
fn claude_identity(home: &Path) -> Option<thegn_core::usage::ClaudeIdentity> {
    let bytes = std::fs::read(home.join(".claude.json")).ok()?;
    usage::parse_claude_identity(&bytes)
}

fn claude_usage(
    cfg: &UsageConfig,
    client: Option<&reqwest::blocking::Client>,
    home: &UsageHome,
) -> AccountUsage {
    let identity = claude_identity(&home.dir);
    let label = home.fallback_label(".claude");
    // Every row — including the failures — carries the home and whatever
    // identity we could read. An "unavailable" row that can't say *which*
    // account it is about is useless on a machine with eight of them.
    let finish = |u: AccountUsage| {
        let u = u.with_home(&home.dir);
        let mut u = match &identity {
            Some(id) => u.with_identity(id),
            None => u,
        };
        // A configured label is the user's own name for this account and beats
        // the identity-derived one; `with_identity` still supplies the key and
        // the email/org/tier fields.
        if let Some(l) = home.label.as_deref().filter(|l| !l.trim().is_empty()) {
            u.account_label = l.to_string();
        }
        u
    };
    let unavailable = |note: &str| finish(AccountUsage::unavailable("claude", &label, note));

    let Some(creds) = claude_credentials(&home.dir) else {
        return unavailable("not logged in");
    };
    if !cfg.allow_network {
        return unavailable("network off");
    }
    let Some(client) = client else {
        return unavailable("no http client");
    };
    let resp = client
        .get(CLAUDE_USAGE_URL)
        .header("Authorization", format!("Bearer {}", creds.token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .send();
    match resp {
        Ok(r) if r.status().is_success() => match r.bytes() {
            // Routed through the harness seam; plan/tier come from the local
            // credentials file (not this body), folded in below exactly as
            // before — behaviour-identical.
            Ok(body) => match harness_parse_usage("claude", &body, thegn_core::util::now()) {
                Some(mut u) => {
                    u.account_label = label;
                    u.plan = creds.plan.clone();
                    // The credentials file's tier is the fallback for homes
                    // whose `.claude.json` is missing or has no oauthAccount.
                    u.rate_limit_tier = creds.rate_limit_tier.clone();
                    finish(u)
                }
                None => unavailable("no windows"),
            },
            Err(_) => unavailable("read failed"),
        },
        // 401 here means the on-disk token expired — worth saying plainly,
        // since the fix (re-run the harness once) is not obvious from "HTTP 401".
        Ok(r) if r.status().as_u16() == 401 => unavailable("token expired"),
        Ok(r) => unavailable(&format!("HTTP {}", r.status().as_u16())),
        Err(_) => unavailable("fetch failed"),
    }
}

// --- Antigravity: local token + live POST (schema unverified) -------------

/// Antigravity's one fixed credential dir. Unlike Codex/Claude there is no env
/// var to relocate it, so there is exactly one per machine.
fn antigravity_home() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(Path::new(&home).join(".gemini").join("antigravity-cli"))
}

/// Read the Antigravity OAuth token from its plain-file fallback
/// (`~/.gemini/antigravity-cli/antigravity-oauth-token`). The OS-keychain path is
/// out of scope for v1, so a keychain-only login reports `Unavailable`.
fn antigravity_token() -> Option<String> {
    let path = antigravity_home()?.join("antigravity-oauth-token");
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // The fallback file is usually the bare token, occasionally a JSON wrapper.
    if trimmed.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        for key in ["access_token", "accessToken", "token"] {
            if let Some(t) = v
                .get(key)
                .and_then(|t| t.as_str())
                .filter(|t| !t.is_empty())
            {
                return Some(t.to_string());
            }
        }
        return None;
    }
    Some(trimmed.to_string())
}

fn antigravity_usage(
    cfg: &UsageConfig,
    client: Option<&reqwest::blocking::Client>,
    _home: &UsageHome,
    now: i64,
) -> AccountUsage {
    let Some(token) = antigravity_token() else {
        return AccountUsage::unavailable("antigravity", "Antigravity", "no local token");
    };
    if !cfg.allow_network {
        return AccountUsage::unavailable("antigravity", "Antigravity", "network off");
    }
    let Some(client) = client else {
        return AccountUsage::unavailable("antigravity", "Antigravity", "no http client");
    };
    let resp = client
        .post(ANTIGRAVITY_QUOTA_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body("{}")
        .send();
    match resp {
        Ok(r) if r.status().is_success() => match r.bytes() {
            Ok(body) => harness_parse_usage("antigravity", &body, now).unwrap_or_else(|| {
                AccountUsage::unavailable("antigravity", "Antigravity", "unrecognized quota")
            }),
            Err(_) => AccountUsage::unavailable("antigravity", "Antigravity", "read failed"),
        },
        Ok(r) => AccountUsage::unavailable(
            "antigravity",
            "Antigravity",
            format!("HTTP {}", r.status().as_u16()),
        ),
        Err(_) => AccountUsage::unavailable("antigravity", "Antigravity", "fetch failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gather_disabled_is_empty() {
        let cfg = UsageConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(gather(&cfg).is_empty());
    }

    /// A config that touches nothing outside what the test sets up: no profile
    /// scan, no explicit accounts. The harnesses' own default homes are still
    /// resolved from the environment, which is why assertions below are about
    /// *states*, never about how many rows came back.
    fn hermetic(providers: &[&str]) -> UsageConfig {
        UsageConfig {
            enabled: true,
            allow_network: false,
            discover_profiles: false,
            profile_roots: Vec::new(),
            providers: providers.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn gather_ignores_unknown_providers() {
        // An unknown id resolves to no credential home, so it contributes no
        // row — matching what `[usage] providers` documents ("an unknown id is
        // ignored"). It used to emit an "unknown provider" row instead.
        assert!(gather(&hermetic(&["nope"])).is_empty());
    }

    #[test]
    fn claude_and_antigravity_respect_network_off() {
        // With network off, network-backed providers never Ok — they report a
        // deterministic Unavailable reason regardless of local creds.
        for acc in gather(&hermetic(&["claude", "antigravity"])) {
            assert_eq!(acc.state, thegn_core::usage::UsageState::Unavailable);
        }
    }

    #[test]
    fn configured_accounts_add_rename_and_exclude_homes() {
        let tmp = tmpdir("cfgacct");
        let work = tmp.join("work");
        let skip = tmp.join("skip");
        for d in [&work, &skip] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join(".credentials.json"), b"{}").unwrap();
        }
        let cfg = UsageConfig {
            accounts: vec![
                thegn_core::usage::UsageAccount {
                    name: "work".into(),
                    provider: "claude".into(),
                    dir: work.display().to_string(),
                    label: "Work account".into(),
                    enabled: true,
                },
                thegn_core::usage::UsageAccount {
                    name: "skip".into(),
                    provider: "claude".into(),
                    dir: skip.display().to_string(),
                    enabled: false,
                    ..Default::default()
                },
                // A dir-less entry is not a home and must be skipped rather
                // than producing a row for the current working directory.
                thegn_core::usage::UsageAccount {
                    name: "nodir".into(),
                    provider: "claude".into(),
                    ..Default::default()
                },
            ],
            ..hermetic(&["claude"])
        };
        let rows = gather(&cfg);
        assert!(
            rows.iter().any(|r| r.account_label == "Work account"),
            "configured label wins: {:?}",
            rows.iter().map(|r| &r.account_label).collect::<Vec<_>>()
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.home.as_deref() == Some(skip.as_path())),
            "an `enabled = false` entry must be excluded"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn scan_profile_root_finds_both_home_layouts() {
        let tmp = tmpdir("scan");
        // `~/.claude-profiles/<name>` is a fake $HOME: creds one level in.
        let nested = tmp.join("alpha").join(".claude");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(".credentials.json"), b"{}").unwrap();
        // An adopted CLAUDE_CONFIG_DIR holds them at its own top level.
        let flat = tmp.join("beta");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(flat.join(".credentials.json"), b"{}").unwrap();
        // A directory with no auth marker is not a home.
        std::fs::create_dir_all(tmp.join("empty")).unwrap();
        // Neither is a loose file.
        std::fs::write(tmp.join("stray"), b"x").unwrap();

        let found = scan_profile_root(&tmp, &["claude".to_string()]);
        let mut dirs: Vec<_> = found.iter().map(|c| c.dir.clone()).collect();
        dirs.sort();
        assert_eq!(dirs, vec![nested, flat]);
        assert!(found.iter().all(|c| c.origin == HomeOrigin::Discovered));

        // An absent root is the normal case, not an error.
        assert!(scan_profile_root(&tmp.join("nope"), &["claude".to_string()]).is_empty());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn discovered_homes_are_labelled_by_their_profile_dir() {
        let tmp = tmpdir("labels");
        for name in ["regclaude", "regclaude2"] {
            let d = tmp.join(name).join(".claude");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(".credentials.json"), b"{}").unwrap();
        }
        let cfg = UsageConfig {
            discover_profiles: true,
            profile_roots: vec![tmp.display().to_string()],
            ..hermetic(&["claude"])
        };
        let labels: Vec<_> = gather(&cfg).into_iter().map(|r| r.account_label).collect();
        // Every profile's leaf dir is ".claude"; labelling by it would make all
        // rows identical, which is the whole point of the parent fallback.
        assert!(labels.contains(&"regclaude".to_string()), "{labels:?}");
        assert!(labels.contains(&"regclaude2".to_string()), "{labels:?}");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A unique scratch dir. Includes the thread id as well as the clock: these
    /// tests run concurrently under nextest and a same-millisecond collision
    /// would have one test deleting another's fixture.
    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tg-usage-{tag}-{}-{:?}",
            thegn_core::util::now(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn newest_rollout_picks_latest_and_ignores_others() {
        let tmp = tmpdir("roll");
        let day = tmp.join("2026").join("08").join("12");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-1-a.jsonl"), b"{}").unwrap();
        // A non-rollout file must be ignored.
        std::fs::write(day.join("history.jsonl"), b"{}").unwrap();
        // Newer file (written second) should win by mtime.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let newer = day.join("rollout-2-b.jsonl");
        std::fs::write(&newer, b"{}").unwrap();
        assert_eq!(newest_rollout(&tmp), Some(newer));
        // Empty tree → None.
        let empty = tmp.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(newest_rollout(&empty), None);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
