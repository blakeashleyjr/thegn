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

const NO_LISTENER: &str =
    "daemon not running or [lead] added after daemon start: restart the daemon";
const MAX_ADDR_FILE_BYTES: u64 = 4096;

/// What a Lead tab's agent needs for its MCP server: extra agent argv, env,
/// and paths the sandbox must leave readable.
#[allow(dead_code, reason = "consumed by b152 lane 8 (Lead launch)")]
#[derive(Debug, Clone, Default)]
pub(crate) struct LeadMcpPlan {
    pub agent_args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub readable: Vec<std::path::PathBuf>,
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
fn parse_control_addr(bytes: &[u8]) -> Result<SocketAddr> {
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
fn mint_lead_token(
    store: &dyn ControlStore,
    scopes: ScopeSet,
    now: i64,
    label: &str,
) -> Result<String> {
    revoke_lead_pairings(store, now, label)?;
    let minted = thegn_svc::control::auth::mint(
        thegn_core::control::TokenKind::Control,
        scopes,
        label,
        None,
        None,
        now,
    );
    store.put_pairing(&minted.row)?;
    Ok(minted.token)
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
            let db = thegn_core::db::Db::open()?;
            mint_lead_token(&db, scopes, now_ms(), label)
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
    anyhow::ensure!(
        cfg.daemon.enabled,
        "[daemon] enabled = false: the Lead's tools need the daemon; enable it and restart"
    );
    let harness = crate::daemon::agent_open::harness_for_agent(cfg, &lead.agent).map(|h| h.id());
    if harness != Some("claude") {
        anyhow::bail!(
            "Lead MCP wiring supports claude in b152 ([lead] agent {:?} runs {})",
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
        assert!(
            err.to_string().contains("[daemon] enabled = false"),
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
            err.to_string()
                .contains("Lead MCP wiring supports claude in b152"),
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
        let old = mint_lead_token(&db, ScopeSet::of(&[Scope::Read]), 1, "lead").unwrap();
        assert!(!old.is_empty());
        let mint = |scopes: ScopeSet, label: &str| mint_lead_token(&db, scopes, now_ms(), label);
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
        assert_eq!(rows.len(), 2);
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
