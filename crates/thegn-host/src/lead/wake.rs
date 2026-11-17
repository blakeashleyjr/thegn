//! `lead.wake`: wake the Lead for a rule (THE-2197, THE-2194). See
//! `thegn_core::lead_wake` for the contract: the rule's prompt renders against
//! `LEAD_WAKE_TEMPLATE_VARS` (never `{message}`) and untrusted event text
//! rides only in a bounded, delimited event block.
//!
//! Delivery:
//! - an open Lead tab (the newest live daemon session whose cwd is the
//!   canonical notes dir, whose worktree is none or that same dir, and whose
//!   process argv carries a Lead MCP config under the Lead control dir) gets
//!   the envelope as one bracketed paste plus Enter;
//! - otherwise one bounded headless turn runs under the very same bwrap plan
//!   as a tab (`lead::launch::headless_spec`), single-flight, and its
//!   `lead:wake/0` token is revoked once it ends, whatever the outcome.
//!
//! Fails closed: no `[lead]`, no bwrap, or no unix bounded runner means no
//! turn, never a host shell.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
use thegn_core::automation::{AutomationEvent, PlannedAction};
use thegn_core::config::Config;

/// The refusal when a headless turn is already in flight. Recorded on the
/// run row, so a fast schedule shows its skipped ticks.
pub(crate) const ALREADY_RUNNING: &str = "a headless Lead turn is already running";

/// At most one headless Lead turn at a time (process-wide).
static HEADLESS_RUNNING: AtomicBool = AtomicBool::new(false);

/// Holds the single-flight slot; released on drop.
struct Flight(&'static AtomicBool);

impl Flight {
    fn acquire(slot: &'static AtomicBool) -> anyhow::Result<Self> {
        slot.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self(slot))
            .map_err(|_| anyhow::anyhow!(ALREADY_RUNNING))
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Sets the shared cancel flag when the awaiting future is dropped (the
/// executor's deadline or shutdown), so the bounded runner kills the group.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) async fn run(
    cfg: Arc<Config>,
    event: &AutomationEvent,
    action: &PlannedAction,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let (notes, text) = wake_text(&cfg, &action.params, event)?;
    // A missing notes dir means no tab can be open there.
    let notes = tokio::task::spawn_blocking(move || std::fs::canonicalize(notes).ok())
        .await
        .context("resolving the Lead notes dir")?;

    let client = crate::automation_executor::client().await?;
    if let Some(notes) = notes.as_deref().and_then(|n| n.to_str()) {
        let marker = crate::lead::mcp::control_dir_in(&thegn_core::util::xdg_state_home())?
            .path()
            .join("mcp-lead_")
            .to_string_lossy()
            .into_owned();
        let sessions = client.sessions().await?;
        let sessions = tokio::task::spawn_blocking(move || {
            sessions
                .into_iter()
                .filter(|s| {
                    s.pid
                        .and_then(crate::platform::proc::cmdline)
                        .is_some_and(|argv| argv.iter().any(|a| a.contains(&marker)))
                })
                .collect::<Vec<_>>()
        })
        .await
        .context("checking the Lead tab process")?;
        let rows: Vec<thegn_core::lead_wake::SessionRow> = sessions
            .into_iter()
            .map(|s| {
                let live = s.exited_at_ms.is_none();
                (s.id, s.cwd, s.worktree, live, s.created_at_ms)
            })
            .collect();
        if let Some(id) = thegn_core::lead_wake::pick_lead_session(&rows, notes) {
            return client
                .send_input(&id, &thegn_core::lead_wake::pty_bytes(&text), true)
                .await
                .with_context(|| format!("waking the Lead tab {id}"));
        }
    }
    drop(client);

    if let Some(why) = crate::agent_run::headless_unsupported() {
        bail!(why);
    }
    let flight = Flight::acquire(&HEADLESS_RUNNING)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    tokio::task::spawn_blocking(move || {
        let _flight = flight;
        let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            headless_turn(&cfg, &text, timeout_secs, &cancel)
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the headless Lead turn panicked")));
        // Whatever happened (a refused launch may already have minted), the
        // turn's token dies with it.
        let revoked = crate::lead::mcp::revoke_lead_caller(crate::lead::launch::HEADLESS_CALLER);
        match (turn, revoked) {
            (Ok(()), Ok(_)) => Ok(()),
            (Err(error), Ok(_)) => Err(error),
            (Ok(()), Err(error)) => {
                Err(error.context("the headless Lead turn ran but its token was not revoked"))
            }
            (Err(error), Err(revoke)) => {
                tracing::error!(
                    target: "thegn::automation",
                    error = %revoke,
                    "the headless Lead token was not revoked"
                );
                Err(error)
            }
        }
    })
    .await
    .context("joining the headless Lead turn")?
}

/// The Lead's notes dir and the wake envelope, or the fail-closed refusal
/// (no `[lead]`, no prompt). The prompt was already rendered by evaluate
/// against `LEAD_WAKE_TEMPLATE_VARS`.
fn wake_text(
    cfg: &Config,
    params: &std::collections::BTreeMap<String, String>,
    event: &AutomationEvent,
) -> anyhow::Result<(std::path::PathBuf, String)> {
    let Some(lead) = cfg.lead.as_ref() else {
        bail!("the Lead is not configured: add a [lead] section naming an [[agents]] entry");
    };
    let prompt = params
        .get("prompt")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .context("lead.wake.prompt is required")?;
    Ok((
        lead.notes_dir_resolved(),
        thegn_core::lead_wake::envelope(prompt, event),
    ))
}

/// One headless turn: build the sandboxed launch, run it bounded and reaped,
/// then drop the launch (its prompt file) after the reap.
fn headless_turn(
    cfg: &Config,
    text: &str,
    timeout_secs: u64,
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let launch = crate::lead::launch::headless_spec(cfg, text)?;
    let mut cmd = cmd_from(&launch.spec)?;
    let result = crate::agent_run::run_command_bounded(&mut cmd, timeout_secs, cancel);
    drop(launch);
    result.map_err(|why| anyhow::anyhow!("the headless Lead turn failed: {why}"))
}

/// The process for a launch spec: argv, cwd and env overlay as given, stdin
/// closed, output piped to the bounded runner, inherited git env removed.
fn cmd_from(spec: &crate::agent::LaunchSpec) -> anyhow::Result<std::process::Command> {
    use std::process::Stdio;
    let (program, args) = spec
        .argv
        .split_first()
        .context("the headless Lead launch has an empty argv")?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    for var in thegn_core::util::GIT_ENV_VARS {
        cmd.env_remove(var);
    }
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_flight_refuses_a_second_turn_until_the_first_ends() {
        static SLOT: AtomicBool = AtomicBool::new(false);
        let first = Flight::acquire(&SLOT).unwrap();
        let err = Flight::acquire(&SLOT).err().expect("second refused");
        assert_eq!(err.to_string(), ALREADY_RUNNING);
        drop(first);
        let again = Flight::acquire(&SLOT).expect("released on drop");
        drop(again);
        assert!(!SLOT.load(Ordering::Acquire));
    }

    #[test]
    fn cancel_on_drop_sets_the_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        drop(CancelOnDrop(flag.clone()));
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn cmd_from_applies_the_spec_and_strips_git_env() {
        let spec = crate::agent::LaunchSpec {
            argv: vec!["bwrap".into(), "--".into(), "true".into()],
            cwd: Some(std::path::PathBuf::from("/notes")),
            env: vec![("THEGN_CALLER".into(), "lead:wake/0".into())],
            backend: "bwrap".into(),
            warnings: Vec::new(),
            degraded: false,
            remote: false,
        };
        let cmd = cmd_from(&spec).unwrap();
        assert_eq!(cmd.get_program(), "bwrap");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--", "true"]);
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/notes")));
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(
            envs.iter()
                .any(|(k, v)| *k == "THEGN_CALLER" && *v == Some("lead:wake/0".as_ref()))
        );
        for var in thegn_core::util::GIT_ENV_VARS {
            assert!(
                envs.iter().any(|(k, v)| *k == var && v.is_none()),
                "{var} removed"
            );
        }
        let empty = crate::agent::LaunchSpec {
            argv: vec![],
            ..spec
        };
        assert!(cmd_from(&empty).is_err());
    }

    #[test]
    fn wake_text_fails_closed_and_wraps_the_event() {
        let event = AutomationEvent::bare(
            thegn_core::automation::AutomationEventKind::Notification,
            "k",
            0,
        );
        let params =
            |p: &str| std::collections::BTreeMap::from([("prompt".to_string(), p.to_string())]);
        let err = wake_text(&Config::default(), &params("hi"), &event).unwrap_err();
        assert!(err.to_string().contains("[lead]"), "{err}");

        let cfg = Config {
            lead: Some(thegn_core::config_lead::LeadConfig {
                agent: "lead-agent".into(),
                notes_dir: Some("/srv/notes".into()),
                ..Default::default()
            }),
            ..Config::default()
        };
        assert!(wake_text(&cfg, &params("  "), &event).is_err());
        let (notes, text) = wake_text(&cfg, &params("Summarize overview"), &event).unwrap();
        assert_eq!(notes, std::path::PathBuf::from("/srv/notes"));
        assert!(text.starts_with("Summarize overview"));
        assert!(text.ends_with(thegn_core::lead_wake::UNTRUSTED_NOTICE));
    }
}
