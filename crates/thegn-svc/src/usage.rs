//! The usage-tracker I/O seam (roadmap V 300): resolve each harness's local
//! state, read it, and hand the bytes to the pure parsers in
//! [`thegn_core::usage`]. This is the untested-by-coverage edge (filesystem +
//! network); the parsing/formatting it feeds is unit-tested in core.
//!
//! Graceful degradation is the whole contract: [`gather`] **never errors**. A
//! missing file, a disabled provider, a network failure, or an unparseable body
//! all yield that account as [`thegn_core::usage::UsageState::Unavailable`] with a short reason,
//! mirroring orca's `unavailable` bar. Providers are independent — one failing
//! never hides the others.
//!
//! Source per provider (see `thegn_core::usage`): **Codex** reads the newest
//! rollout `.jsonl` (offline); **Claude** and **Antigravity** don't persist
//! window state, so they read the locally-stored OAuth token and make one
//! lightweight authenticated request — gated behind `[usage] allow_network`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use thegn_core::account;
use thegn_core::config::UsageConfig;
use thegn_core::usage::{self, AccountUsage};

/// Anthropic's undocumented per-account usage endpoint (what Claude Code's
/// `/usage` reads). Authenticated with the OAuth token from `.credentials.json`.
const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// Google Cloud Code quota summary (the closed-app path Antigravity companions use).
const ANTIGRAVITY_QUOTA_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
/// Cap every live fetch so a hung endpoint can't wedge the gather thread.
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);

/// Gather usage for every enabled provider, in the configured display order. Runs
/// on a blocking thread (the host calls this from `spawn_blocking`). Never panics
/// or errors — unreadable providers come back `Unavailable`.
pub fn gather(cfg: &UsageConfig) -> Vec<AccountUsage> {
    if !cfg.enabled {
        return Vec::new();
    }
    let now = thegn_core::util::now();
    let client = build_client();
    cfg.providers
        .iter()
        .map(|p| match p.as_str() {
            "codex" => codex_usage(now),
            "claude" => claude_usage(cfg, client.as_ref(), now),
            "antigravity" => antigravity_usage(cfg, client.as_ref(), now),
            other => AccountUsage::unavailable(other, other, "unknown provider"),
        })
        .collect()
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

/// The `sessions/` root for Codex (honoring `CODEX_HOME`), if the home exists.
fn codex_sessions_dir() -> Option<PathBuf> {
    let p = account::provider("codex")?;
    let home = account::effective_config_dir(p)?;
    let dir = Path::new(&home).join("sessions");
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

fn codex_usage(now: i64) -> AccountUsage {
    let Some(dir) = codex_sessions_dir() else {
        return AccountUsage::unavailable("codex", "Codex", "not installed");
    };
    let Some(file) = newest_rollout(&dir) else {
        return AccountUsage::unavailable("codex", "Codex", "no sessions");
    };
    match std::fs::read(&file) {
        Ok(bytes) => usage::parse_codex_rollup(&bytes, now)
            .unwrap_or_else(|| AccountUsage::unavailable("codex", "Codex", "no rate-limit data")),
        Err(_) => AccountUsage::unavailable("codex", "Codex", "unreadable session"),
    }
}

// --- Claude: local token + live GET ---------------------------------------

/// Read `<claude home>/.credentials.json` and extract the OAuth token + plan.
fn claude_credentials() -> Option<thegn_core::usage::ClaudeCreds> {
    let p = account::provider("claude")?;
    let home = account::effective_config_dir(p)?;
    let bytes = std::fs::read(Path::new(&home).join(".credentials.json")).ok()?;
    usage::parse_claude_credentials(&bytes)
}

fn claude_usage(
    cfg: &UsageConfig,
    client: Option<&reqwest::blocking::Client>,
    _now: i64,
) -> AccountUsage {
    let Some(creds) = claude_credentials() else {
        return AccountUsage::unavailable("claude", "Claude", "not logged in");
    };
    if !cfg.allow_network {
        return AccountUsage::unavailable("claude", "Claude", "network off");
    }
    let Some(client) = client else {
        return AccountUsage::unavailable("claude", "Claude", "no http client");
    };
    let resp = client
        .get(CLAUDE_USAGE_URL)
        .header("Authorization", format!("Bearer {}", creds.token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .send();
    match resp {
        Ok(r) if r.status().is_success() => match r.bytes() {
            Ok(body) => usage::parse_claude_usage(&body, creds.plan.clone())
                .unwrap_or_else(|| AccountUsage::unavailable("claude", "Claude", "no windows")),
            Err(_) => AccountUsage::unavailable("claude", "Claude", "read failed"),
        },
        Ok(r) => {
            AccountUsage::unavailable("claude", "Claude", format!("HTTP {}", r.status().as_u16()))
        }
        Err(_) => AccountUsage::unavailable("claude", "Claude", "fetch failed"),
    }
}

// --- Antigravity: local token + live POST (schema unverified) -------------

/// Read the Antigravity OAuth token from its plain-file fallback
/// (`~/.gemini/antigravity-cli/antigravity-oauth-token`). The OS-keychain path is
/// out of scope for v1, so a keychain-only login reports `Unavailable`.
fn antigravity_token() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = Path::new(&home)
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");
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
            Ok(body) => usage::parse_antigravity_quota(&body, now).unwrap_or_else(|| {
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

    #[test]
    fn gather_unknown_provider_is_unavailable() {
        let cfg = UsageConfig {
            enabled: true,
            providers: vec!["nope".into()],
            allow_network: false,
            poll_interval_secs: 60,
        };
        let out = gather(&cfg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].provider, "nope");
        assert_eq!(out[0].state, thegn_core::usage::UsageState::Unavailable);
    }

    #[test]
    fn claude_and_antigravity_respect_network_off() {
        // With network off, network-backed providers never Ok — they report a
        // deterministic Unavailable reason regardless of local creds.
        let cfg = UsageConfig {
            enabled: true,
            providers: vec!["claude".into(), "antigravity".into()],
            allow_network: false,
            poll_interval_secs: 60,
        };
        for acc in gather(&cfg) {
            assert_eq!(acc.state, thegn_core::usage::UsageState::Unavailable);
        }
    }

    #[test]
    fn newest_rollout_picks_latest_and_ignores_others() {
        let tmp = std::env::temp_dir().join(format!("tg-usage-roll-{}", thegn_core::util::now()));
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
