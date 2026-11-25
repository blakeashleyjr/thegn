//! The Lead's MCP wiring (THE-2171, THE-2188): a host-minted, scoped pairing
//! token on a dedicated loopback listener, the `thegn mcp proxy --profile` hub,
//! and `thegn mcp wire --lead`.
//!
//! ## The privilege boundary
//!
//! The daemon's unix socket grants implicit admin to any same-uid peer, so the
//! Lead (an agent with a shell tool, sandboxed with that socket masked) talks to
//! the daemon only through its own loopback listener (`local_admin = false`,
//! see `daemon::lead_caps::listener`) with a bearer token minted here:
//!
//! - scopes = `[lead] scopes` clamped by the `[mcp.serve]` ceiling (and the
//!   active profile's overlay); clamp-only, `admin`/`exec` never;
//! - labelled with the tab's caller id; that tab's older pairing is revoked
//!   first, so a token lives until the tab's next spawn (a TTL is queued
//!   separately);
//! - written only to an owner-only per-tab `mcp-<tab>.json` beneath
//!   `$XDG_STATE_HOME/thegn/lead-control/`, a directory deliberately distinct
//!   from (and never inside) the Lead's writable notes dir.
//!
//! No daemon means no Lead tools: [`prepare`] bails rather than inventing an
//! in-process path.

use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use thegn_core::config::Config;
use thegn_core::control::{Scope, ScopeSet};
use thegn_core::fsperm::PrivateDir;
use thegn_core::store::ControlStore;

/// The control dir, relative to `$XDG_STATE_HOME`. Not `thegn/lead`: that is
/// the default `[lead] notes_dir`, which the Lead can write.
pub(crate) const CONTROL_DIR_REL: &str = "thegn/lead-control";
/// The daemon's Lead listener address file inside [`CONTROL_DIR_REL`].
pub(crate) const CONTROL_ADDR_FILE: &str = "control-addr.json";
/// The pairing label of a bare Lead (`wire --lead`); tab Leads use their
/// `lead:<group>/<tab>` caller id.
pub(crate) const PAIRING_LABEL: &str = "lead";

const NO_LISTENER: &str = "no Lead listener: the daemon is not running, or was started before \
                           [lead] was added; restart it from the Lead setup (Alt i) or run \
                           `thegn daemon stop`, then reopen thegn";
/// The refusal when the daemon is disabled. Fixed text: the docs and tests
/// cite it.
const DAEMON_DISABLED: &str = "[daemon] enabled = false: the Lead's tools need the daemon; enable \
                               it, then `thegn daemon stop` and reopen thegn";
const MAX_ADDR_FILE_BYTES: u64 = 4096;

/// What a Lead tab's agent needs for its MCP server: extra agent argv, env,
/// and paths the sandbox must leave readable.
#[allow(dead_code, reason = "consumed by b152 lane 8 (Lead launch)")]
#[derive(Debug, Clone, Default)]
pub(crate) struct LeadMcpPlan {
    pub agent_args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub readable: Vec<std::path::PathBuf>,
    /// The Lead listener's loopback address from `control-addr.json` (b155):
    /// the isolated network namespace forwards exactly this port.
    pub control_addr: Option<SocketAddr>,
}

/// Open (creating owner-only as needed) the Lead control dir beneath
/// `state_home`.
pub(crate) fn control_dir_in(state_home: &Path) -> Result<PrivateDir> {
    // The anchor may be absent on a first run; only that creation path uses
    // `ensure_private_dir` (an existing anchor is never re-tightened).
    if !state_home.exists() {
        thegn_core::fsperm::ensure_private_dir(state_home)
            .with_context(|| format!("could not create state home {}", state_home.display()))?;
    }
    thegn_core::fsperm::ensure_private_dir_beneath(state_home, Path::new(CONTROL_DIR_REL))
        .context("could not secure the Lead control directory")
}

/// The `control-addr.json` body for `addr`.
pub(crate) fn control_addr_json(addr: SocketAddr) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "addr": addr.to_string() }))
        .expect("serialize a socket address")
}

/// Publish the daemon's Lead listener address (owner-only, atomic).
pub(crate) fn write_control_addr(state_home: &Path, addr: SocketAddr) -> Result<PathBuf> {
    let dir = control_dir_in(state_home)?;
    dir.write_owner_only_atomic(CONTROL_ADDR_FILE, &control_addr_json(addr))
        .with_context(|| format!("write {}", dir.path().join(CONTROL_ADDR_FILE).display()))
}

/// Parse `control-addr.json`, refusing anything but a loopback address (the
/// token must never be sent off-host because of a tampered file).
pub(crate) fn parse_control_addr(bytes: &[u8]) -> Result<SocketAddr> {
    #[derive(serde::Deserialize)]
    struct File {
        addr: String,
    }
    let file: File = serde_json::from_slice(bytes).context("malformed control-addr.json")?;
    let addr: SocketAddr = file
        .addr
        .parse()
        .with_context(|| format!("control-addr.json: bad address {:?}", file.addr))?;
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "control-addr.json: {addr} is not a loopback address"
    );
    Ok(addr)
}

fn read_control_addr(dir: &PrivateDir) -> Result<SocketAddr> {
    let file = match dir.open_regular_nofollow(CONTROL_ADDR_FILE) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(NO_LISTENER),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("read {}", dir.path().join(CONTROL_ADDR_FILE).display()));
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_ADDR_FILE_BYTES).read_to_end(&mut bytes)?;
    parse_control_addr(&bytes)
}

/// The Lead token's scopes: `[lead] scopes` clamped by the `[mcp.serve]`
/// ceiling and the profile overlay. Clamp-only: the Lead can never widen
/// `[mcp.serve]`, and `admin`/`exec` are never granted whatever the inputs.
pub(crate) fn clamp_lead_scopes(
    global: Option<ScopeSet>,
    profile: Option<ScopeSet>,
    lead: ScopeSet,
) -> ScopeSet {
    let (set, _) = thegn_core::control::resolve_serve_scopes(
        global,
        profile,
        None,
        Some(lead),
        ScopeSet::of(&[Scope::Read]),
    );
    set.intersect(ScopeSet::of(&[
        Scope::Read,
        Scope::Write,
        Scope::Git,
        Scope::MergeAdd,
    ]))
}

fn lead_scopes(cfg: &Config, lead: &thegn_core::config_lead::LeadConfig) -> ScopeSet {
    let global = cfg.mcp.serve.scope_set();
    let profile = cfg
        .profiles
        .get(&thegn_core::profile::name())
        .and_then(|p| p.mcp_serve.scope_set());
    clamp_lead_scopes(global, profile, lead.scope_set())
}

/// Revoke every live token whose label matches `pred`. Returns how many were
/// revoked.
fn revoke_pairings_where(
    store: &dyn ControlStore,
    now: i64,
    pred: impl Fn(&str) -> bool,
) -> Result<usize> {
    let mut n = 0;
    for row in store.pairings()? {
        if pred(&row.label) && row.revoked_at.is_none() {
            store.revoke_pairing(&row.pairing_id, now)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Revoke every live token of one Lead tab (`label`). Returns how many were
/// revoked.
fn revoke_lead_pairings(store: &dyn ControlStore, now: i64, label: &str) -> Result<usize> {
    revoke_pairings_where(store, now, |l| l == label)
}

/// Revoke every live token labelled `caller` (the headless `lead:wake/0`
/// turn, once it ends). TEMPORARY b153 lane 6 wrapper: lane 9's
/// `revoke_tab_token` supersedes it; the primary repoints the one call in
/// `lead::wake` and drops this at merge.
pub(crate) fn revoke_lead_caller(caller: &str) -> Result<usize> {
    let db = thegn_core::db::Db::open()?;
    revoke_lead_pairings(&db, now_ms(), caller)
}

/// Revoke every live Lead token, whatever its tab. Returns how many were
/// revoked.
fn revoke_all_lead_pairings(store: &dyn ControlStore, now: i64) -> Result<usize> {
    revoke_pairings_where(store, now, |l| l == PAIRING_LABEL || l.starts_with("lead:"))
}

/// The caller id (and pairing label) of Lead tab `tab` of `group`:
/// `lead:<group>/<tab>`. The one source for both `launch` and the close path,
/// so a revoke always names the label the spawn minted.
pub(crate) fn tab_caller(group: &str, tab: usize) -> String {
    format!("lead:{group}/{tab}")
}

/// Parse a [`tab_caller`] label back into `(group, tab index)`. Anything else
/// (the bare `lead` label, a foreign label) is `None`.
fn parse_tab_caller(label: &str) -> Option<(&str, usize)> {
    let (group, tab) = label.strip_prefix("lead:")?.rsplit_once('/')?;
    if tab.is_empty() || !tab.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((group, tab.parse().ok()?))
}

/// Whether closing the Lead tab `closing` invalidates the pairing `label`.
///
/// Tab indices shift on close (`Vec::remove`): the tab that was at `i + 1` is
/// now at `i`, but its running agent still holds the token labelled `i + 1`
/// and its env still says `THEGN_CALLER=lead:<group>/<i + 1>`. Left live, that
/// label would be ambiguous: the next tab spawned at index `i + 1` would
/// revoke a token another tab is using, and the shifted tab's own respawn (at
/// `i`) would never revoke its old one. So every label of the same group at
/// or above the closed index is revoked; each surviving tab re-mints under its
/// new index on its next spawn (`mint_lead_token` revokes first anyway).
/// A non-tab caller (the bare `wire --lead` label) invalidates only itself.
pub(crate) fn invalidated_by_close(closing: &str, label: &str) -> bool {
    match (parse_tab_caller(closing), parse_tab_caller(label)) {
        (Some((group, closed)), Some((other_group, index))) => {
            group == other_group && index >= closed
        }
        _ => label == closing,
    }
}

/// Revoke the token of the closed Lead tab `caller` (plus every label the
/// close shifted, see [`invalidated_by_close`]) and remove those tabs'
/// `mcp-<tab>.json` from the control dir. Blocking DB + disk I/O: callers on
/// the loop must run it through `sched::spawn_bg`. Returns how many tokens
/// were revoked.
///
/// The config carries nothing the revoke needs today; it stays in the
/// signature so a later pairing TTL or profile scoping lands without touching
/// the callers.
pub(crate) fn revoke_tab_token(_cfg: &Config, caller: &str) -> Result<usize> {
    let db = thegn_core::db::Db::open()?;
    revoke_tab_token_in(&db, &thegn_core::util::xdg_state_home(), now_ms(), caller)
}

/// Revoke a closed Lead tab's control token (and every label the close
/// shifted, see [`invalidated_by_close`]) and remove its `mcp-<tab>.json`,
/// OFF the loop: the DB write and the unlink run on the bounded background
/// pool, never on the caller. A no-op unless `[lead]` is enabled and `group`
/// is the Lead group. Tab 0 revokes the whole group.
pub(crate) fn revoke_closed_tab_off_loop(cfg: &Config, group: &str, tab: usize) {
    if !crate::lead::enabled(cfg) || !crate::lead::group::is_lead_group(group) {
        return;
    }
    let caller = tab_caller(group, tab);
    let cfg = cfg.clone();
    let admission = crate::sched::spawn_bg(move || match revoke_tab_token(&cfg, &caller) {
        Ok(revoked) => {
            tracing::debug!(target: "thegn::lead", %caller, revoked, "revoked closed Lead tab token");
        }
        // best-effort: a failed revoke leaves the token live until the
        // listener's next start (or the tab index's next mint) revokes it;
        // the close itself must not fail on it.
        Err(error) => {
            tracing::warn!(target: "thegn::lead", %caller, error = %format!("{error:#}"), "revoking a closed Lead tab token failed");
        }
    });
    if admission != crate::sched::Admission::Admitted {
        tracing::warn!(target: "thegn::lead", group, tab, ?admission, "Lead tab token revoke not scheduled");
    }
}

fn revoke_tab_token_in(
    store: &dyn ControlStore,
    state_home: &Path,
    now: i64,
    caller: &str,
) -> Result<usize> {
    // Every label the close touches, live or already revoked: a revoked
    // token's file may still be on disk.
    let mut labels: Vec<String> = store
        .pairings()?
        .into_iter()
        .map(|row| row.label)
        .filter(|label| invalidated_by_close(caller, label))
        .collect();
    labels.push(caller.to_string());
    labels.sort();
    labels.dedup();
    let revoked = revoke_pairings_where(store, now, |l| invalidated_by_close(caller, l))?;
    remove_mcp_files_in(state_home, &labels)?;
    Ok(revoked)
}

/// Remove the `mcp-<label>.json` of each label, through the held control-dir
/// handle only (`unlinkat` beneath it, never following a link; a sanitized
/// file name can never leave it). An absent control dir is never created
/// here: nothing was ever written. Returns how many files were removed.
fn remove_mcp_files_in(state_home: &Path, labels: &[String]) -> Result<usize> {
    if !state_home.join(CONTROL_DIR_REL).is_dir() {
        return Ok(0);
    }
    let dir = control_dir_in(state_home)?;
    let mut removed = 0;
    for label in labels {
        let file = mcp_file_name(label);
        if dir
            .remove_file(&file)
            .with_context(|| format!("remove {}", dir.path().join(&file).display()))?
        {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Revoke every Lead token at the Lead listener's start. The listener binds a
/// fresh port each start, so no running Lead can hold a usable token for it:
/// every Lead tab still open was wired to the previous address and re-mints on
/// its next spawn. Must run before the new address is published, so a mint
/// against the new listener is never caught by it.
pub(crate) fn revoke_stale_lead_tokens() -> Result<usize> {
    let db = thegn_core::db::Db::open()?;
    revoke_all_lead_pairings(&db, now_ms())
}

/// The per-tab Lead MCP config file name inside [`CONTROL_DIR_REL`].
pub(crate) fn mcp_file_name(caller: &str) -> String {
    format!(
        "mcp-{}.json",
        caller
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            })
            .collect::<String>()
    )
}

/// Mint one Lead tab's control token (approved at mint: the host is trusted),
/// revoking that tab's older tokens first. Returns the bearer token.
///
/// The token expires `ttl_secs` after mint (`[lead] token_ttl_secs`, b155);
/// active use slides it forward (`auth::slide_lead_expiry`), capped at
/// mint + 7 days. `0` (only reachable from tests) mints without an expiry.
fn mint_lead_token(
    store: &dyn ControlStore,
    scopes: ScopeSet,
    now: i64,
    label: &str,
    ttl_secs: u64,
) -> Result<String> {
    revoke_lead_pairings(store, now, label)?;
    let minted = thegn_svc::control::auth::mint(
        thegn_core::control::TokenKind::Control,
        scopes,
        label,
        None,
        lead_token_expiry(now, ttl_secs),
        now,
    );
    store.put_pairing(&minted.row)?;
    Ok(minted.token)
}

/// `now + ttl` in unix ms, or `None` for a zero TTL.
fn lead_token_expiry(now: i64, ttl_secs: u64) -> Option<i64> {
    (ttl_secs > 0).then(|| {
        let ttl_ms = i64::try_from(ttl_secs)
            .unwrap_or(i64::MAX)
            .saturating_mul(1000);
        now.saturating_add(ttl_ms)
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| thegn_core::time_policy::saturating_i64(d.as_millis()))
        .unwrap_or(0)
}

/// Whether two directories nest (either inside the other, or equal). Compared
/// lexically and, when both resolve, canonically.
fn dirs_nest(a: &Path, b: &Path) -> bool {
    let lexical = a.starts_with(b) || b.starts_with(a);
    let canonical = match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a.starts_with(&b) || b.starts_with(&a),
        _ => false,
    };
    lexical || canonical
}

/// Prepare the MCP plan for one Lead tab.
pub(crate) fn prepare(cfg: &Config, notes_dir: &Path, caller: &str) -> Result<LeadMcpPlan> {
    prepare_in(
        cfg,
        notes_dir,
        caller,
        &thegn_core::util::xdg_state_home(),
        &|scopes, label| {
            let ttl = cfg
                .lead
                .as_ref()
                .map_or(thegn_core::config_lead::DEFAULT_TOKEN_TTL_SECS, |l| {
                    l.token_ttl_secs
                });
            let db = thegn_core::db::Db::open()?;
            mint_lead_token(&db, scopes, now_ms(), label, ttl)
        },
    )
}

/// Mints the Lead token for the clamped scopes (the DB is opened only after
/// every refusal has passed).
type Mint<'a> = &'a dyn Fn(ScopeSet, &str) -> Result<String>;

fn prepare_in(
    cfg: &Config,
    notes_dir: &Path,
    caller: &str,
    state_home: &Path,
    mint: Mint<'_>,
) -> Result<LeadMcpPlan> {
    let lead = cfg
        .lead
        .as_ref()
        .context("[lead] is not configured (or is clamped off on this channel)")?;
    anyhow::ensure!(cfg.daemon.enabled, DAEMON_DISABLED);
    let harness = crate::daemon::agent_open::harness_for_agent(cfg, &lead.agent).map(|h| h.id());
    if harness != Some("claude") {
        anyhow::bail!(
            "the Lead's MCP wiring supports claude only for now ([lead] agent {} runs {})",
            lead.agent,
            harness.unwrap_or("an unknown harness")
        );
    }
    let dir = control_dir_in(state_home)?;
    anyhow::ensure!(
        !dirs_nest(dir.path(), notes_dir),
        "[lead] notes_dir {} overlaps the Lead control dir {}: pick a notes_dir outside it",
        notes_dir.display(),
        dir.path().display()
    );
    let addr = read_control_addr(&dir)?;

    let scopes = lead_scopes(cfg, lead);
    let token = mint(scopes, caller)?;
    let hub_profile = cfg
        .mcp_proxy
        .profiles
        .get(&lead.mcp_profile)
        .is_some_and(|servers| !servers.is_empty())
        .then_some(lead.mcp_profile.as_str());
    let exe = std::env::current_exe().context("resolve the thegn executable")?;
    let exe = exe.to_string_lossy();
    let csv = scopes.to_csv();
    let control_addr = addr;
    let addr = addr.to_string();
    let doc = thegn_core::mcp::wire::lead_entries(&thegn_core::mcp::wire::LeadEntries {
        command: &exe,
        scopes_csv: &csv,
        control_addr: &addr,
        control_token: &token,
        caller,
        hub_profile,
    });
    let bytes = serde_json::to_vec_pretty(&doc).context("serialize the Lead mcp.json")?;
    let file = mcp_file_name(caller);
    let path = dir
        .write_owner_only_atomic(&file, &bytes)
        .with_context(|| format!("write {}", dir.path().join(&file).display()))?;
    Ok(LeadMcpPlan {
        agent_args: vec![
            "--mcp-config".into(),
            path.to_string_lossy().into_owned(),
            "--strict-mcp-config".into(),
        ],
        env: Vec::new(),
        readable: vec![dir.path().to_path_buf()],
        control_addr: Some(control_addr),
    })
}

/// `thegn mcp proxy [--profile <name>]`.
pub(crate) fn run_proxy(cfg: &Config, profile: Option<&str>) -> Result<()> {
    crate::mcp_proxy::run_shim(cfg, profile)
}

/// `thegn mcp wire --lead [--remove]`: write (or remove) the Lead's private
/// mcp.json without spawning anything — a debugging aid. Never touches the
/// user's `~/.claude*` settings: the Lead never uses them.
pub(crate) fn wire_cli(cfg: &Config, remove: bool) -> Result<()> {
    let state_home = thegn_core::util::xdg_state_home();
    if remove {
        let dir = control_dir_in(&state_home)?;
        let file = mcp_file_name(PAIRING_LABEL);
        let removed = dir
            .remove_file(&file)
            .with_context(|| format!("remove {}", dir.path().join(&file).display()))?;
        let db = thegn_core::db::Db::open()?;
        let revoked = revoke_all_lead_pairings(&db, now_ms())?;
        thegn_core::outln!(
            "{} {} (revoked {revoked} Lead token{})",
            if removed { "removed" } else { "nothing at" },
            dir.path().join(&file).display(),
            if revoked == 1 { "" } else { "s" }
        );
        return Ok(());
    }
    let lead = cfg
        .lead
        .as_ref()
        .context("[lead] is not configured (or is clamped off on this channel)")?;
    prepare(cfg, &lead.notes_dir_resolved(), PAIRING_LABEL)?;
    thegn_core::outln!(
        "{}",
        state_home
            .join(CONTROL_DIR_REL)
            .join(mcp_file_name(PAIRING_LABEL))
            .display()
    );
    thegn_core::outln!("(older Lead tokens were revoked: a running Lead must be reopened)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lead_cfg(agent_cmd: &str) -> Config {
        let mut cfg = Config::default();
        cfg.agents.push(
            toml::from_str(&format!("name = \"lead-agent\"\ncommand = \"{agent_cmd}\"")).unwrap(),
        );
        cfg.lead = Some(thegn_core::config_lead::LeadConfig {
            agent: "lead-agent".into(),
            ..Default::default()
        });
        cfg.daemon.enabled = true;
        cfg
    }

    fn no_store(_: ScopeSet, _: &str) -> Result<String> {
        panic!("no token may be minted before the refusal")
    }

    #[test]
    fn prepare_refuses_when_control_addr_is_absent() {
        let state = tempfile::tempdir().unwrap();
        let notes = tempfile::tempdir().unwrap();
        let err = prepare_in(
            &lead_cfg("claude"),
            notes.path(),
            "lead",
            state.path(),
            &no_store,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), NO_LISTENER);
        for needle in [
            "no Lead listener:",
            "was started before",
            "`thegn daemon stop`",
            "Alt i",
        ] {
            assert!(NO_LISTENER.contains(needle), "{needle}");
        }
        assert!(
            !state
                .path()
                .join(CONTROL_DIR_REL)
                .join(mcp_file_name("lead"))
                .exists()
        );
    }

    #[test]
    fn prepare_refuses_without_lead_daemon_or_claude() {
        let state = tempfile::tempdir().unwrap();
        let notes = tempfile::tempdir().unwrap();
        let mut cfg = lead_cfg("claude");
        cfg.lead = None;
        assert!(prepare_in(&cfg, notes.path(), "lead", state.path(), &no_store).is_err());

        let mut cfg = lead_cfg("claude");
        cfg.daemon.enabled = false;
        let err = prepare_in(&cfg, notes.path(), "lead", state.path(), &no_store).unwrap_err();
        assert_eq!(err.to_string(), DAEMON_DISABLED);
        assert!(
            err.to_string().contains("then `thegn daemon stop`"),
            "{err}"
        );

        let err = prepare_in(
            &lead_cfg("codex"),
            notes.path(),
            "lead",
            state.path(),
            &no_store,
        )
        .unwrap_err();
        assert!(
            err.to_string().starts_with(
                "the Lead's MCP wiring supports claude only for now ([lead] agent lead-agent runs "
            ),
            "{err}"
        );
    }

    #[test]
    fn prepare_refuses_a_notes_dir_overlapping_the_control_dir() {
        let state = tempfile::tempdir().unwrap();
        let notes = state.path().join("thegn");
        let err =
            prepare_in(&lead_cfg("claude"), &notes, "lead", state.path(), &no_store).unwrap_err();
        assert!(
            err.to_string().contains("overlaps the Lead control dir"),
            "{err}"
        );
    }

    #[test]
    fn prepare_mints_a_clamped_token_and_writes_a_private_mcp_json() {
        let state = tempfile::tempdir().unwrap();
        let notes = tempfile::tempdir().unwrap();
        write_control_addr(state.path(), "127.0.0.1:4242".parse().unwrap()).unwrap();
        let mut cfg = lead_cfg("claude");
        // A read-only ceiling clamps the default `[lead] scopes` (read,write).
        cfg.mcp.serve.scopes = Some(vec!["read".into()]);
        cfg.mcp_proxy
            .profiles
            .insert("lead".into(), vec!["todoist".into()]);
        let db = thegn_core::db::Db::open_memory().unwrap();
        // An older Lead token, which the new mint must revoke.
        let old = mint_lead_token(&db, ScopeSet::of(&[Scope::Read]), 1, "lead", 0).unwrap();
        assert!(!old.is_empty());
        let ttl = cfg.lead.as_ref().unwrap().token_ttl_secs;
        let mint =
            |scopes: ScopeSet, label: &str| mint_lead_token(&db, scopes, now_ms(), label, ttl);
        let plan = prepare_in(&cfg, notes.path(), "lead", state.path(), &mint).unwrap();

        let dir = state.path().join(CONTROL_DIR_REL);
        let path = dir.join(mcp_file_name("lead"));
        assert_eq!(
            plan.agent_args,
            [
                "--mcp-config".to_string(),
                path.to_string_lossy().into_owned(),
                "--strict-mcp-config".to_string()
            ]
        );
        assert_eq!(plan.readable, std::slice::from_ref(&dir));
        assert_eq!(plan.control_addr, Some("127.0.0.1:4242".parse().unwrap()));
        crate::platform::test_assert_owner_only_permissions(&path, &dir);
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let serve = &doc["mcpServers"]["thegn"];
        assert_eq!(serve["args"][3], "read");
        assert_eq!(serve["env"]["THEGN_CONTROL_ADDR"], "127.0.0.1:4242");
        assert_eq!(serve["env"]["THEGN_CALLER"], "lead");
        assert_eq!(
            doc["mcpServers"]["thegn-lead-hub"]["args"],
            serde_json::json!(["mcp", "proxy", "--profile", "lead"])
        );

        let rows = db.pairings().unwrap();
        let live: Vec<_> = rows.iter().filter(|r| r.revoked_at.is_none()).collect();
        assert_eq!(live.len(), 1, "older Lead tokens are revoked");
        assert_eq!(live[0].label, PAIRING_LABEL);
        assert_eq!(live[0].scope, "read", "clamped by the read-only ceiling");
        assert!(live[0].approved_at.is_some());
        let ttl_ms = i64::try_from(ttl).unwrap() * 1000;
        let expires = live[0].expires_at.expect("a Lead token carries an expiry");
        assert_eq!(
            expires - live[0].created_at,
            ttl_ms,
            "expires at mint + ttl"
        );
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn tab_caller_is_the_launch_label_and_parses_back() {
        assert_eq!(tab_caller("lead", 0), "lead:lead/0");
        assert_eq!(tab_caller("lead", 12), "lead:lead/12");
        assert_eq!(parse_tab_caller(&tab_caller("lead", 3)), Some(("lead", 3)));
        assert_eq!(parse_tab_caller("lead:a/b/7"), Some(("a/b", 7)));
        assert_eq!(parse_tab_caller(PAIRING_LABEL), None);
        assert_eq!(parse_tab_caller("lead:lead/"), None);
        assert_eq!(parse_tab_caller("lead:lead/-1"), None);
        assert_eq!(parse_tab_caller("lead:lead/+1"), None);
        assert_eq!(parse_tab_caller("other:lead/1"), None);
    }

    #[test]
    fn closing_a_tab_invalidates_it_and_every_shifted_tab_above_it() {
        let closing = tab_caller("lead", 1);
        let set: Vec<&str> = [
            "lead:lead/0",
            "lead:lead/1",
            "lead:lead/2",
            "lead:lead/10",
            "lead:other/5",
            PAIRING_LABEL,
            "cli",
        ]
        .into_iter()
        .filter(|l| invalidated_by_close(&closing, l))
        .collect();
        assert_eq!(set, ["lead:lead/1", "lead:lead/2", "lead:lead/10"]);
        // The bare label invalidates only itself.
        assert!(invalidated_by_close(PAIRING_LABEL, PAIRING_LABEL));
        assert!(!invalidated_by_close(PAIRING_LABEL, "lead:lead/0"));
    }

    #[test]
    fn revoke_tab_token_revokes_at_or_above_and_removes_files_beneath_the_control_dir_only() {
        let state = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open_memory().unwrap();
        let ro = ScopeSet::of(&[Scope::Read]);
        for label in ["lead:lead/0", "lead:lead/1", "lead:lead/2", PAIRING_LABEL] {
            mint_lead_token(&db, ro, 1, label, 0).unwrap();
        }
        let dir = control_dir_in(state.path()).unwrap();
        for label in ["lead:lead/0", "lead:lead/1", "lead:lead/2"] {
            dir.write_owner_only_atomic(&mcp_file_name(label), b"{}")
                .unwrap();
        }
        // A same-named file outside the control dir must never be touched.
        let outside = state.path().join(mcp_file_name("lead:lead/1"));
        std::fs::write(&outside, b"keep").unwrap();

        let revoked = revoke_tab_token_in(&db, state.path(), 2, "lead:lead/1").unwrap();
        assert_eq!(revoked, 2);
        let mut live: Vec<String> = db
            .pairings()
            .unwrap()
            .into_iter()
            .filter(|r| r.revoked_at.is_none())
            .map(|r| r.label)
            .collect();
        live.sort();
        assert_eq!(live, [PAIRING_LABEL, "lead:lead/0"]);

        let ctl = state.path().join(CONTROL_DIR_REL);
        assert!(ctl.join(mcp_file_name("lead:lead/0")).exists());
        assert!(!ctl.join(mcp_file_name("lead:lead/1")).exists());
        assert!(!ctl.join(mcp_file_name("lead:lead/2")).exists());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");

        // Idempotent: a second close of the same index revokes nothing new.
        assert_eq!(
            revoke_tab_token_in(&db, state.path(), 3, "lead:lead/1").unwrap(),
            0
        );
    }

    #[test]
    fn mcp_file_removal_never_creates_an_absent_control_dir() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(
            remove_mcp_files_in(state.path(), &["lead:lead/0".to_string()]).unwrap(),
            0
        );
        assert!(!state.path().join(CONTROL_DIR_REL).exists());
    }

    #[test]
    fn lead_scopes_are_clamp_only() {
        let rw = ScopeSet::of(&[Scope::Read, Scope::Write]);
        let ro = ScopeSet::of(&[Scope::Read]);
        // A read-only ceiling can never be widened by `[lead] scopes`.
        assert_eq!(clamp_lead_scopes(Some(ro), None, rw), ro);
        assert_eq!(clamp_lead_scopes(Some(rw), Some(ro), rw), ro);
        assert_eq!(clamp_lead_scopes(None, None, rw), rw);
        // admin/exec never survive, whatever the inputs.
        let all = ScopeSet::universe();
        let got = clamp_lead_scopes(Some(all), None, all);
        assert!(!got.contains(Scope::Admin) && !got.contains(Scope::Exec));
        assert!(got.contains(Scope::Read) && got.contains(Scope::Write));
    }

    #[test]
    fn control_addr_round_trips_and_refuses_non_loopback() {
        let addr: SocketAddr = "127.0.0.1:5151".parse().unwrap();
        assert_eq!(parse_control_addr(&control_addr_json(addr)).unwrap(), addr);
        assert!(parse_control_addr(br#"{"addr":"10.0.0.2:80"}"#).is_err());
        assert!(parse_control_addr(b"nope").is_err());
    }

    #[test]
    fn control_addr_file_is_owner_only_and_outside_the_default_notes_dir() {
        let state = tempfile::tempdir().unwrap();
        let path = write_control_addr(state.path(), "127.0.0.1:1".parse().unwrap()).unwrap();
        crate::platform::test_assert_owner_only_permissions(&path, path.parent().unwrap());
        // `[lead] notes_dir` defaults to `<state>/thegn/lead`.
        assert!(!dirs_nest(
            &state.path().join(CONTROL_DIR_REL),
            &state.path().join("thegn/lead")
        ));
    }
}
