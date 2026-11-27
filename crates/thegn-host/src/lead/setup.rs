//! In-app Lead setup (THE-2725): the guided flow Alt i opens when the Lead is
//! not configured or not ready, replacing hand-edited config, `thegn daemon
//! stop`, `thegn lead login` and `thegn doctor` typed in a shell.
//!
//! The flow is a pure state machine over [`Facts`] (what is true on disk and on
//! the host right now) and a [`Plan`] (what the user has chosen so far, carried
//! in each menu row's confirm arg). [`next_step`] derives the step from those
//! two, so the flow is resumable: pressing Alt i again re-derives it from the
//! files, never from hidden state. [`apply`] runs one confirmed row through a
//! [`Host`] (the effects: config write, daemon stop, facts), so every
//! transition is testable with a fake host.
//!
//! On the stable channel the Lead is a labelled preview: the flow writes
//! `[lead] preview = true`, the marker the channel clamp honours, so nothing
//! else changes for users who never run it. The two things that touch the
//! outside world while the menu is open (the Lead checks, the live daemon
//! session count) run as background [`Job`]s whose [`Async`] result comes back
//! through the event loop, never on it.
//!
//! The login steps do not reimplement `thegn lead login`: they open the very
//! same verb in a thegn pane, so its isolation guarantees (fresh temp dir, host
//! network, only the credentials copied back, refusal for a Lead or pipeline
//! caller) hold unchanged.

use crate::menu::{ChoiceRow, MenuOverlay, choice_menu, confirm_menu};

/// The menu confirm tag every setup row resolves to.
pub(crate) const TAG: &str = "lead-setup";

/// Install hint for bubblewrap, the Lead sandbox's hard requirement.
const BWRAP_HINT: &str = "Install bubblewrap: apt install bubblewrap (Debian/Ubuntu), \
     dnf install bubblewrap (Fedora), pacman -S bubblewrap (Arch), or add \
     pkgs.bubblewrap on Nix. The Lead needs Linux.";
/// Install hint for pasta (the `passt` package).
const PASTA_HINT: &str = "Install pasta: apt install passt (Debian/Ubuntu), dnf install passt \
     (Fedora), pacman -S passt (Arch), or add pkgs.passt on Nix.";

/// One agent the flow can offer for the Lead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentOption {
    /// The `[lead] agent` value: an `[[agents]]` entry or a built-in harness.
    pub name: String,
    /// The resolved harness id (`claude`, `codex`, ...).
    pub harness: String,
    /// Whether its binary resolves on `PATH`.
    pub on_path: bool,
}

impl AgentOption {
    /// Only claude is supported as the Lead's agent today (login, harness home
    /// and the sandbox bind are claude-specific).
    pub(crate) fn supported(&self) -> bool {
        self.harness == "claude" && !self.name.contains([';', '|'])
    }
}

/// What is true right now. Gathered by [`Host::facts`]; every field is cheap
/// (config file read, a few `stat`s), so it is safe on the event loop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// Whether this process runs the dev channel. On stable the Lead is a
    /// preview and needs the `[lead] preview = true` marker.
    pub dev_channel: bool,
    /// `[lead] preview = true` as written in the user config file.
    pub file_preview: bool,
    /// Detected and configured agents.
    pub agents: Vec<AgentOption>,
    /// `[lead] agent` as written in the user config file.
    pub file_agent: Option<String>,
    /// `[lead] network = "host"` as written in the user config file. The Lead
    /// refuses it, so the flow rewrites it to `isolated`.
    pub file_host_network: bool,
    /// `[daemon] enabled = true` as written in the user config file.
    pub daemon_enabled: bool,
    /// Whether `bwrap` is installed.
    pub bwrap: bool,
    /// Whether `pasta` is installed.
    pub pasta: bool,
    /// Whether the daemon's Lead control listener has announced itself.
    pub listener_up: bool,
    /// Whether the Lead's private home holds credentials.
    pub logged_in: bool,
    /// Servers in the Lead's `mcp_profile`.
    pub mcp_servers: Vec<String>,
}

/// The user's choices so far, carried in every row's arg.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The chosen `[lead] agent` (empty until picked).
    pub agent: String,
    /// The daemon stop was already issued in this run.
    pub restarted: bool,
}

impl Plan {
    /// The plan a fresh run starts from: what the config file already says.
    pub(crate) fn from_facts(f: &Facts) -> Self {
        Plan {
            agent: f.file_agent.clone().unwrap_or_default(),
            restarted: false,
        }
    }

    fn encode(&self) -> String {
        format!("{}|{}", self.agent, u8::from(self.restarted))
    }

    fn decode(s: &str) -> Option<Plan> {
        let (agent, restarted) = s.rsplit_once('|')?;
        Some(Plan {
            agent: agent.to_string(),
            restarted: restarted == "1",
        })
    }

    /// The exact lines the config write adds, shown as a preview first.
    pub(crate) fn preview(&self, f: &Facts) -> Vec<String> {
        let mut lines = vec![
            "[lead]".to_string(),
            format!("agent = \"{}\"", self.agent),
            "preview = true".to_string(),
        ];
        if f.file_host_network {
            lines.push("network = \"isolated\"".into());
        }
        lines.push("[daemon]".into());
        lines.push("enabled = true".into());
        lines
    }
}

/// One step of the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Pick the Lead's agent.
    ChooseAgent,
    /// bubblewrap is missing: nothing can proceed.
    NeedBwrap,
    /// pasta is missing: nothing can proceed until it is installed.
    NeedNetwork,
    /// Write `[lead]` and `[daemon] enabled` (with a preview).
    WriteConfig,
    /// Stop the daemon so it restarts with the Lead's listener.
    Restart,
    /// Log the agent in (a pane running `thegn lead login`).
    Login,
    /// Everything is ready: checks, MCP logins, open, reset.
    Review,
}

/// Pure: the next step given the facts and the choices so far.
pub(crate) fn next_step(f: &Facts, plan: &Plan) -> Step {
    if plan.agent.is_empty() {
        return Step::ChooseAgent;
    }
    if !f.bwrap {
        return Step::NeedBwrap;
    }
    if !f.pasta {
        return Step::NeedNetwork;
    }
    let written = f.file_agent.as_deref() == Some(plan.agent.as_str())
        && !f.file_host_network
        && f.daemon_enabled
        && (f.dev_channel || f.file_preview);
    if !written {
        return Step::WriteConfig;
    }
    if !f.listener_up && !plan.restarted {
        return Step::Restart;
    }
    if !f.logged_in {
        return Step::Login;
    }
    Step::Review
}

/// Whether the Lead still needs setup before its first open: not logged in, or
/// the daemon's listener is not up. The jump key sends the user to the flow
/// instead of opening a Lead that would fail.
pub(crate) fn incomplete(f: &Facts) -> bool {
    !f.logged_in || !f.listener_up
}

/// The effects and observations the flow needs. The real host reads files and
/// spawns processes; tests supply a fake.
pub(crate) trait Host {
    fn facts(&self) -> Facts;
    /// Write `[lead]` and `[daemon] enabled` through the config writer.
    fn write_config(&mut self, plan: &Plan, f: &Facts) -> Result<(), String>;
    /// Remove the `[lead]` section.
    fn reset_config(&mut self) -> Result<(), String>;
    /// argv for a thegn verb, run in a pane (`["lead","login"]`), with this
    /// process's channel carried over (never forced). `None` when the
    /// executable is unknown.
    fn pane_argv(&self, verb: &[&str]) -> Option<Vec<String>>;
}

/// The outcome of one confirmed row.
#[derive(Default)]
pub(crate) struct Applied {
    /// A status line.
    pub status: Option<String>,
    /// The next menu.
    pub menu: Option<MenuOverlay>,
    /// argv to run in a new visible tab.
    pub tab: Option<Vec<String>>,
    /// Open the Lead now.
    pub open_lead: bool,
    /// Work to run off the event loop; its result reopens a menu.
    pub job: Option<Job>,
}

/// Work the flow needs but must not do on the event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Job {
    /// Run the doctor's Lead checks.
    Doctor(Plan),
    /// Count the daemon's live sessions before a restart.
    Sessions(Plan),
    /// Stop the daemon off the event loop before retrying Lead.
    Restart(Plan),
}

/// A finished [`Job`], delivered through the event loop's refresh channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Async {
    /// The Lead checks, as report lines.
    Doctor { plan: Plan, lines: Vec<String> },
    /// Live daemon sessions (`None`: the count failed).
    Sessions { plan: Plan, live: Option<usize> },
    /// Daemon stop result; success means the caller can reopen Lead.
    Restarted {
        plan: Plan,
        result: Result<(), String>,
    },
}

impl Applied {
    fn status(s: impl Into<String>) -> Self {
        Applied {
            status: Some(s.into()),
            ..Applied::default()
        }
    }
}

fn arg(verb: &str, plan: &Plan, extra: &str) -> String {
    format!("{verb};{};{extra}", plan.encode())
}

fn row(key: char, label: impl Into<String>, arg: String) -> ChoiceRow {
    ChoiceRow {
        key: Some(key),
        label: label.into(),
        arg: Some(arg),
    }
}

fn cancel() -> ChoiceRow {
    ChoiceRow {
        key: Some('n'),
        label: "close".into(),
        arg: None,
    }
}

const TITLE: &str = "Set up the Lead";

/// The first menu for Alt i: the preview note on stable, else the step the
/// facts call for.
pub(crate) fn begin(host: &dyn Host) -> MenuOverlay {
    let f = host.facts();
    let plan = Plan::from_facts(&f);
    if f.dev_channel {
        return step_menu(&f, &plan);
    }
    choice_menu(
        TITLE,
        "The Lead is a preview on the stable channel. Setting it up writes `preview = true` \
         under [lead], the marker that lets this build keep it; nothing changes for anyone \
         who does not run this. Undo any time from the last step's reset row.",
        TAG,
        vec![row('c', "continue setup", arg("go", &plan, "")), cancel()],
    )
}

/// The menu for `plan` given fresh facts.
pub(crate) fn step_menu(f: &Facts, plan: &Plan) -> MenuOverlay {
    match next_step(f, plan) {
        Step::ChooseAgent => agent_menu(f, plan),
        Step::NeedBwrap => choice_menu(
            "Lead: bubblewrap is missing",
            format!(
                "The Lead runs in a bubblewrap sandbox and refuses to open without it. {BWRAP_HINT}"
            ),
            TAG,
            vec![row('r', "check again", arg("go", plan, "")), cancel()],
        ),
        Step::NeedNetwork => choice_menu(
            "Lead: pasta is missing",
            format!(
                "The Lead's sandbox gets an isolated network through pasta, which is not \
                 installed and the Lead refuses to open without it. {PASTA_HINT}"
            ),
            TAG,
            vec![
                row(
                    'r',
                    "check again (after installing pasta)",
                    arg("go", plan, ""),
                ),
                cancel(),
            ],
        ),
        Step::WriteConfig => choice_menu(
            "Write the Lead config?",
            format!(
                "Adds to your config file, keeping everything else:\n{}\nUndo any time from \
                 this flow's reset row.",
                plan.preview(f).join("\n")
            ),
            TAG,
            vec![row('y', "write config", arg("write", plan, "")), cancel()],
        ),
        Step::Restart => choice_menu(
            "Restart the daemon?",
            "The Lead needs the daemon to restart so it carries the Lead's control listener. \
             This stops the daemon: terminals it hosts (including Lead tabs) end, and thegn \
             starts a fresh daemon on the next attach. Windows you have open are not closed. \
             The next screen says how many sessions would end.",
            TAG,
            vec![
                row(
                    'y',
                    "restart the daemon and retry the Lead",
                    arg("restartask", plan, ""),
                ),
                row('s', "skip: I will restart it later", arg("skip", plan, "")),
                cancel(),
            ],
        ),
        Step::Login => choice_menu(
            "Log the Lead's agent in",
            "Opens a tab running the Lead's own login on the host network, from a fresh \
             temporary directory; only the credentials file is copied into the Lead's private \
             home. Type /login there, finish in your browser, exit, then press Alt i again.",
            TAG,
            vec![
                row('y', "open the login tab", arg("login", plan, "")),
                cancel(),
            ],
        ),
        Step::Review => review_menu(f, plan),
    }
}

fn agent_menu(f: &Facts, plan: &Plan) -> MenuOverlay {
    let supported: Vec<&AgentOption> = f.agents.iter().filter(|a| a.supported()).collect();
    let unsupported: Vec<String> = f
        .agents
        .iter()
        .filter(|a| !a.supported())
        .map(|a| a.name.clone())
        .collect();
    let mut body = if supported.is_empty() {
        "No supported agent was found. Install Claude Code (the `claude` binary), then check \
         again."
            .to_string()
    } else {
        "Pick the agent the Lead runs.".to_string()
    };
    if !unsupported.is_empty() {
        body.push_str(&format!(
            " Not offered: {} (only claude is supported as the Lead's agent today).",
            unsupported.join(", ")
        ));
    }
    let mut rows: Vec<ChoiceRow> = Vec::new();
    let keys = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'];
    for (a, key) in supported.iter().zip(keys) {
        let label = if a.on_path {
            a.name.clone()
        } else {
            format!("{} (binary not on PATH)", a.name)
        };
        rows.push(row(key, label, arg("pick", plan, &a.name)));
    }
    rows.push(row('r', "check again", arg("go", plan, "")));
    rows.push(cancel());
    choice_menu("Lead: choose the agent", body, TAG, rows)
}

fn mark(ok: bool) -> &'static str {
    if ok { "ok  " } else { "todo" }
}

fn review_menu(f: &Facts, plan: &Plan) -> MenuOverlay {
    review_menu_with(f, plan, None)
}

/// The last step; `checks` is the Lead checks report once they have run.
fn review_menu_with(f: &Facts, plan: &Plan, checks: Option<&[String]>) -> MenuOverlay {
    let mut body = format!(
        "{} agent {}\n{} bubblewrap\n{} pasta (isolated network)\n{} daemon enabled, Lead listener \
         up\n{} agent login",
        mark(true),
        plan.agent,
        mark(f.bwrap),
        mark(f.pasta),
        mark(f.daemon_enabled && f.listener_up),
        mark(f.logged_in),
    );
    if let Some(lines) = checks {
        body.push_str("\n\n");
        body.push_str(&lines.join("\n"));
    }
    let mut rows = vec![
        row('o', "open the Lead", arg("open", plan, "")),
        row(
            'd',
            if checks.is_some() {
                "run the Lead checks again"
            } else {
                "run the Lead checks"
            },
            arg("doctor", plan, ""),
        ),
    ];
    for server in &f.mcp_servers {
        rows.push(ChoiceRow {
            key: None,
            label: format!("log in MCP server {server}"),
            arg: Some(arg("mcp", plan, server)),
        });
    }
    rows.push(row(
        'x',
        "disable: reset the Lead config",
        arg("reset", plan, ""),
    ));
    rows.push(cancel());
    choice_menu("The Lead is ready", body, TAG, rows)
}

/// The restart confirm's note: how many live daemon sessions the stop ends.
pub(crate) fn restart_body(live: Option<usize>) -> String {
    match live {
        Some(0) => "The daemon has no live sessions, so nothing ends. It restarts on the next \
                    attach."
            .to_string(),
        Some(n) => format!(
            "{n} live daemon session{} would end (terminals, agent runs and Lead tabs the daemon \
             hosts). Windows you have open are not closed, and the daemon restarts on the next \
             attach.",
            if n == 1 { "" } else { "s" }
        ),
        None => "The daemon's live sessions could not be counted; any it hosts would end. \
                 Windows you have open are not closed."
            .to_string(),
    }
}

/// The menu for a finished background [`Job`].
pub(crate) fn async_menu(host: &dyn Host, done: Async) -> MenuOverlay {
    match done {
        Async::Doctor { plan, lines } => review_menu_with(&host.facts(), &plan, Some(&lines)),
        Async::Sessions { plan, live } => confirm_menu(
            "Restart the daemon?",
            restart_body(live),
            TAG,
            arg("restart", &plan, ""),
            true,
        ),
        Async::Restarted { .. } => choice_menu(
            TITLE,
            "The daemon restart did not reopen Lead. Press Alt i to continue setup.",
            TAG,
            vec![cancel()],
        ),
    }
}

/// Run a [`Job`] off the event loop; its [`Async`] result arrives as
/// `RefreshKind::LeadSetup`.
pub(crate) fn spawn_job(
    job: Job,
    cfg: &thegn_core::config::Config,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    let (tx, waker, cfg) = (tx.clone(), waker.clone(), cfg.clone());
    tokio::spawn(async move {
        let done = match job {
            Job::Doctor(plan) => {
                let lines = tokio::task::spawn_blocking(move || {
                    crate::cmd::doctor_lead::lines(&crate::cmd::doctor_lead::inspect(&cfg))
                })
                .await
                .unwrap_or_else(|e| vec![format!("the Lead checks failed to run: {e}")]);
                Async::Doctor { plan, lines }
            }
            Job::Sessions(plan) => {
                let live = match crate::daemon::client::connect_daemon(&cfg.daemon).await {
                    Some(c) => c
                        .sessions()
                        .await
                        .ok()
                        .map(|v| v.iter().filter(|s| s.exited_at_ms.is_none()).count()),
                    None => Some(0),
                };
                Async::Sessions { plan, live }
            }
            Job::Restart(plan) => {
                let result = tokio::task::spawn_blocking(|| {
                    let exe = thegn_core::util::self_exe_path()
                        .ok_or_else(|| "cannot find the thegn executable".to_string())?;
                    let status = std::process::Command::new(exe)
                        .args(["daemon", "stop"])
                        .env("THEGN_CHANNEL", crate::channel_state::current().as_str())
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .map_err(|error| format!("could not run `thegn daemon stop`: {error}"))?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(format!("`thegn daemon stop` exited with {status}"))
                    }
                })
                .await
                .unwrap_or_else(|error| Err(format!("daemon stop worker failed: {error}")));
                Async::Restarted { plan, result }
            }
        };
        if tx
            .send(crate::hydrate::RefreshKind::LeadSetup(Box::new(done)))
            .is_ok()
            && let Err(error) = waker.wake()
        {
            // Best-effort: the loop also wakes on its own ticker.
            tracing::debug!(target: "thegn::lead", %error, "lead setup: waker pulse failed");
        }
    });
}

fn menu_now(host: &dyn Host, plan: &Plan) -> Option<MenuOverlay> {
    Some(step_menu(&host.facts(), plan))
}

/// Run one confirmed row (`arg` is its confirm arg). Unknown args are ignored
/// with a status line, never an error.
pub(crate) fn apply(host: &mut dyn Host, arg_str: &str) -> Applied {
    let mut parts = arg_str.splitn(3, ';');
    let verb = parts.next().unwrap_or_default();
    let Some(plan) = parts.next().and_then(Plan::decode) else {
        return Applied::status("Lead setup: unreadable step, press Alt i to start again");
    };
    let extra = parts.next().unwrap_or_default();
    match verb {
        "go" => Applied {
            menu: menu_now(host, &plan),
            ..Applied::default()
        },
        "pick" => {
            let facts = host.facts();
            let ok = facts
                .agents
                .iter()
                .any(|a| a.name == extra && a.supported());
            if !ok {
                return Applied::status(format!("Lead setup: {extra:?} is not a supported agent"));
            }
            let plan = Plan {
                agent: extra.to_string(),
                ..plan
            };
            Applied {
                menu: Some(step_menu(&facts, &plan)),
                ..Applied::default()
            }
        }
        "write" => {
            let facts = host.facts();
            match host.write_config(&plan, &facts) {
                Ok(()) => Applied {
                    status: Some("Lead config written".into()),
                    menu: menu_now(host, &plan),
                    ..Applied::default()
                },
                Err(e) => Applied::status(format!("Lead config not written: {e}")),
            }
        }
        "restartask" => Applied {
            status: Some("checking the daemon's live sessions…".into()),
            job: Some(Job::Sessions(plan)),
            ..Applied::default()
        },
        "restart" => Applied {
            status: Some("stopping the daemon; Lead will reopen when it has stopped…".into()),
            job: Some(Job::Restart(plan)),
            ..Applied::default()
        },
        "skip" => {
            let plan = Plan {
                restarted: true,
                ..plan
            };
            Applied {
                menu: menu_now(host, &plan),
                ..Applied::default()
            }
        }
        "login" => match host.pane_argv(&["lead", "login"]) {
            Some(argv) => Applied {
                status: Some(
                    "Lead login: type /login in the new tab, then press Alt i again".into(),
                ),
                tab: Some(argv),
                ..Applied::default()
            },
            None => Applied::status("Lead login: cannot find the thegn executable"),
        },
        "mcp" => {
            let facts = host.facts();
            if !facts.mcp_servers.iter().any(|s| s == extra) {
                return Applied::status(format!(
                    "Lead setup: {extra:?} is not in the Lead's MCP profile"
                ));
            }
            match host.pane_argv(&["lead", "login", "--mcp", extra]) {
                Some(argv) => Applied {
                    status: Some(format!(
                        "MCP login for {extra}: open the URL it prints, then press Ctrl-C"
                    )),
                    tab: Some(argv),
                    ..Applied::default()
                },
                None => Applied::status("Lead login: cannot find the thegn executable"),
            }
        }
        "doctor" => Applied {
            status: Some("running the Lead checks…".into()),
            job: Some(Job::Doctor(plan)),
            ..Applied::default()
        },
        "open" => Applied {
            open_lead: true,
            ..Applied::default()
        },
        "reset" => Applied {
            menu: Some(confirm_menu(
                "Disable the Lead?",
                "Removes the [lead] section from your config. [daemon] enabled and the Lead's \
                 saved login are left alone. Run this flow again to set up afresh.",
                TAG,
                arg("resetdo", &plan, ""),
                true,
            )),
            ..Applied::default()
        },
        "resetdo" => match host.reset_config() {
            Ok(()) => Applied::status("Lead disabled: [lead] removed from the config"),
            Err(e) => Applied::status(format!("Lead not disabled: {e}")),
        },
        other => Applied::status(format!("Lead setup: unknown step {other:?}")),
    }
}

/// The real host: files, config writer, daemon, executable.
pub(crate) struct RealHost<'a> {
    pub cfg: &'a thegn_core::config::Config,
}

/// `[lead] agent`, `[lead] network`, `[daemon] enabled` and `[lead] preview`
/// from config text.
pub(crate) fn parse_file_facts(text: &str) -> (Option<String>, bool, bool, bool) {
    let Ok(v) = text.parse::<toml::Table>() else {
        return (None, false, false, false);
    };
    let lead = v.get("lead").and_then(toml::Value::as_table);
    let agent = lead
        .and_then(|t| t.get("agent"))
        .and_then(toml::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned);
    let host = lead
        .and_then(|t| t.get("network"))
        .and_then(toml::Value::as_str)
        == Some("host");
    let daemon = v
        .get("daemon")
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("enabled"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let preview = lead
        .and_then(|t| t.get("preview"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    (agent, host, daemon, preview)
}

/// Agent options: configured `[[agents]]` entries plus built-in harnesses whose
/// binary is on `PATH` (when no entry already carries the name).
fn agent_options(cfg: &thegn_core::config::Config) -> Vec<AgentOption> {
    let mut out: Vec<AgentOption> = Vec::new();
    for name in cfg.agents.iter().map(|a| a.name.clone()) {
        if let Ok(eff) = thegn_core::agent_task::effective_agent(cfg, &name, None) {
            let on_path = eff
                .interactive_command()
                .ok()
                .and_then(|c| c.split_whitespace().next().map(str::to_owned))
                .is_some_and(|w| thegn_core::util::which_path(&w).is_some());
            out.push(AgentOption {
                name,
                harness: eff.harness,
                on_path,
            });
        }
    }
    for builtin in ["claude", "codex", "gemini"] {
        if out.iter().all(|a| a.name != builtin) && thegn_core::util::which_path(builtin).is_some()
        {
            out.push(AgentOption {
                name: builtin.into(),
                harness: builtin.into(),
                on_path: true,
            });
        }
    }
    out
}

impl Host for RealHost<'_> {
    fn facts(&self) -> Facts {
        let text = std::fs::read_to_string(thegn_core::config::Config::path()).unwrap_or_default();
        let (file_agent, file_host_network, daemon_enabled, file_preview) = parse_file_facts(&text);
        let state = thegn_core::util::xdg_state_home();
        let harness = file_agent
            .as_deref()
            .and_then(|a| thegn_core::agent_task::effective_agent(self.cfg, a, None).ok())
            .map_or_else(|| "claude".to_string(), |e| e.harness);
        let profile = self
            .cfg
            .lead
            .as_ref()
            .map_or("lead", |l| l.mcp_profile.as_str());
        Facts {
            dev_channel: crate::channel_state::current() == thegn_core::channel::Channel::Dev,
            file_preview,
            agents: agent_options(self.cfg),
            file_agent,
            file_host_network,
            daemon_enabled,
            bwrap: thegn_core::lead_sandbox::bwrap_available(),
            pasta: thegn_core::lead_net::pasta_available(),
            listener_up: state
                .join(crate::lead::mcp::CONTROL_DIR_REL)
                .join(crate::lead::mcp::CONTROL_ADDR_FILE)
                .is_file(),
            logged_in: state
                .join("thegn")
                .join(thegn_core::lead_sandbox::HARNESS_HOME_DIR)
                .join(harness)
                .join(".credentials.json")
                .is_file(),
            mcp_servers: self
                .cfg
                .mcp_proxy
                .profiles
                .get(profile)
                .cloned()
                .unwrap_or_default(),
        }
    }

    fn write_config(&mut self, plan: &Plan, f: &Facts) -> Result<(), String> {
        use thegn_core::config_write::set_key;
        let path = thegn_core::config::Config::path();
        let run = || -> anyhow::Result<()> {
            set_key(&path, "lead.agent", &plan.agent)?;
            // The marker the stable-channel clamp honours (preview).
            set_key(&path, "lead.preview", "true")?;
            if f.file_host_network {
                set_key(&path, "lead.network", "isolated")?;
            }
            set_key(&path, "daemon.enabled", "true")
        };
        run().map_err(|e| format!("{e:#}"))
    }

    fn reset_config(&mut self) -> Result<(), String> {
        thegn_core::config_write::remove_key(&thegn_core::config::Config::path(), "lead")
            .map_err(|e| format!("{e:#}"))
    }

    fn pane_argv(&self, verb: &[&str]) -> Option<Vec<String>> {
        let exe = thegn_core::util::self_exe_path()?;
        let dev = crate::channel_state::current() == thegn_core::channel::Channel::Dev;
        Some(pane_argv_for(&exe.to_string_lossy(), dev, verb))
    }
}

/// A pane starts from a scrubbed env: carry a dev channel over so the verb
/// sees `[lead]` the way this process does. Stable needs nothing (the setup's
/// `preview = true` marker keeps `[lead]`), and nothing here forces dev.
fn pane_argv_for(exe: &str, dev: bool, verb: &[&str]) -> Vec<String> {
    let mut argv = Vec::new();
    if dev {
        argv.push("env".to_string());
        argv.push("THEGN_CHANNEL=dev".to_string());
    }
    argv.push(exe.to_string());
    argv.extend(verb.iter().map(|s| (*s).to_string()));
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        facts: Facts,
        wrote: Vec<(Plan, Facts)>,
        resets: usize,
        fail_write: bool,
    }

    impl Host for Fake {
        fn facts(&self) -> Facts {
            self.facts.clone()
        }
        fn write_config(&mut self, plan: &Plan, f: &Facts) -> Result<(), String> {
            if self.fail_write {
                return Err("denied".into());
            }
            self.wrote.push((plan.clone(), f.clone()));
            self.facts.file_agent = Some(plan.agent.clone());
            self.facts.file_host_network = false;
            self.facts.daemon_enabled = true;
            Ok(())
        }
        fn reset_config(&mut self) -> Result<(), String> {
            self.resets += 1;
            self.facts.file_agent = None;
            Ok(())
        }
        fn pane_argv(&self, verb: &[&str]) -> Option<Vec<String>> {
            Some(verb.iter().map(|s| (*s).to_string()).collect())
        }
    }

    fn claude() -> AgentOption {
        AgentOption {
            name: "claude".into(),
            harness: "claude".into(),
            on_path: true,
        }
    }

    fn ready() -> Facts {
        Facts {
            dev_channel: true,
            file_preview: true,
            agents: vec![claude()],
            file_agent: Some("claude".into()),
            file_host_network: false,
            daemon_enabled: true,
            bwrap: true,
            pasta: true,
            listener_up: true,
            logged_in: true,
            mcp_servers: vec!["todoist".into()],
        }
    }

    fn plan(f: &Facts) -> Plan {
        Plan::from_facts(f)
    }

    fn args(m: &MenuOverlay) -> Vec<Option<String>> {
        m.items()
            .iter()
            .map(|i| match &i.choice {
                crate::menu::MenuChoice::Confirm { arg, .. } => Some(arg.clone()),
                _ => None,
            })
            .collect()
    }

    fn first_arg(m: &MenuOverlay) -> String {
        args(m).into_iter().flatten().next().expect("a confirm row")
    }

    #[test]
    fn plan_round_trips_and_rejects_garbage() {
        let p = Plan {
            agent: "my-claude".into(),
            restarted: true,
        };
        assert_eq!(Plan::decode(&p.encode()), Some(p));
        assert_eq!(Plan::decode("nopipes"), None);
    }

    #[test]
    fn steps_follow_the_order_of_what_is_missing() {
        let mut f = Facts {
            dev_channel: true,
            agents: vec![claude()],
            ..Facts::default()
        };
        let mut p = plan(&f);
        assert_eq!(next_step(&f, &p), Step::ChooseAgent);
        p.agent = "claude".into();
        assert_eq!(next_step(&f, &p), Step::NeedBwrap);
        f.bwrap = true;
        assert_eq!(next_step(&f, &p), Step::NeedNetwork);
        f.pasta = true;
        assert_eq!(next_step(&f, &p), Step::WriteConfig);
        f.file_agent = Some("claude".into());
        f.daemon_enabled = true;
        assert_eq!(next_step(&f, &p), Step::Restart);
        p.restarted = true;
        assert_eq!(next_step(&f, &p), Step::Login);
        f.logged_in = true;
        assert_eq!(next_step(&f, &p), Step::Review);
    }

    #[test]
    fn pasta_present_skips_the_network_step() {
        let mut f = ready();
        f.file_agent = None;
        f.daemon_enabled = false;
        let p = Plan {
            agent: "claude".into(),
            ..Plan::default()
        };
        assert_eq!(next_step(&f, &p), Step::WriteConfig);
    }

    #[test]
    fn a_written_config_that_disagrees_with_the_plan_is_rewritten() {
        let f = ready();
        let p = Plan {
            agent: "other".into(),
            ..plan(&f)
        };
        assert_eq!(next_step(&f, &p), Step::WriteConfig);
        let mut f = f;
        f.file_host_network = true;
        assert_eq!(next_step(&f, &plan(&f)), Step::WriteConfig);
    }

    #[test]
    fn stable_channel_needs_the_preview_marker_then_carries_on_without_a_relaunch() {
        let mut f = ready();
        f.dev_channel = false;
        f.file_preview = false;
        f.listener_up = false;
        // A config without the marker would be clamped off on stable: rewrite it.
        assert_eq!(next_step(&f, &plan(&f)), Step::WriteConfig);
        f.file_preview = true;
        // No "relaunch on dev" step: straight on to the daemon restart.
        assert_eq!(next_step(&f, &plan(&f)), Step::Restart);
        f.listener_up = true;
        f.logged_in = true;
        assert_eq!(next_step(&f, &plan(&f)), Step::Review);
    }

    #[test]
    fn stable_begin_describes_the_preview_not_a_relaunch() {
        let host = Fake {
            facts: Facts {
                agents: vec![claude()],
                ..Facts::default()
            },
            ..Fake::default()
        };
        let m = begin(&host);
        let body = m.body().unwrap_or_default().to_string();
        assert!(body.contains("preview"), "{body}");
        assert!(!body.contains("THEGN_CHANNEL"), "{body}");
    }

    #[test]
    fn pane_argv_never_forces_the_dev_channel() {
        assert_eq!(
            pane_argv_for("/bin/thegn", false, &["lead", "login"]),
            ["/bin/thegn", "lead", "login"]
        );
        assert_eq!(
            pane_argv_for("/bin/thegn", true, &["lead", "login"]),
            ["env", "THEGN_CHANNEL=dev", "/bin/thegn", "lead", "login"]
        );
    }

    #[test]
    fn restart_asks_for_a_session_count_then_confirms_with_it() {
        let mut host = Fake {
            facts: ready(),
            ..Fake::default()
        };
        let p = plan(&host.facts);
        let asked = apply(&mut host, &arg("restartask", &p, ""));
        assert_eq!(asked.job, Some(Job::Sessions(p.clone())));
        assert!(asked.menu.is_none());
        let menu = async_menu(
            &host,
            Async::Sessions {
                plan: p.clone(),
                live: Some(3),
            },
        );
        assert_eq!(menu.title, "Restart the daemon?");
        assert!(
            menu.body()
                .unwrap_or_default()
                .to_string()
                .contains("3 live daemon sessions would end")
        );
        let confirm = first_arg(&menu);
        assert!(confirm.starts_with("restart;"), "{confirm}");
        let restart = apply(&mut host, &confirm);
        assert_eq!(restart.job, Some(Job::Restart(p)));
        assert!(restart.menu.is_none());
    }

    #[test]
    fn restart_body_counts_and_pluralises() {
        assert!(restart_body(Some(0)).contains("nothing ends"));
        assert!(restart_body(Some(1)).contains("1 live daemon session would end"));
        assert!(restart_body(Some(2)).contains("2 live daemon sessions would end"));
        assert!(restart_body(None).contains("could not be counted"));
    }

    #[test]
    fn doctor_runs_as_a_job_and_the_result_lands_in_the_last_step() {
        let mut host = Fake {
            facts: ready(),
            ..Fake::default()
        };
        let p = plan(&host.facts);
        let out = apply(&mut host, &arg("doctor", &p, ""));
        assert_eq!(out.job, Some(Job::Doctor(p.clone())));
        assert!(out.tab.is_none(), "no separate tab");
        let menu = async_menu(
            &host,
            Async::Doctor {
                plan: p,
                lines: vec!["Lead readiness".into(), "ok    daemon answers".into()],
            },
        );
        assert_eq!(menu.title, "The Lead is ready");
        let body = menu.body().unwrap_or_default().to_string();
        assert!(body.contains("Lead readiness") && body.contains("daemon answers"));
    }

    #[test]
    fn a_ready_lead_with_a_live_listener_skips_restart() {
        let mut f = ready();
        f.logged_in = false;
        assert_eq!(next_step(&f, &plan(&f)), Step::Login);
        assert!(incomplete(&f));
        f.logged_in = true;
        f.listener_up = false;
        assert!(incomplete(&f));
        assert_eq!(next_step(&f, &plan(&f)), Step::Restart);
        f.listener_up = true;
        assert!(!incomplete(&f));
    }

    #[test]
    fn only_claude_is_a_supported_agent() {
        let codex = AgentOption {
            name: "codex".into(),
            harness: "codex".into(),
            on_path: true,
        };
        assert!(claude().supported());
        assert!(!codex.supported());
        let odd = AgentOption {
            name: "a;b".into(),
            harness: "claude".into(),
            on_path: true,
        };
        assert!(!odd.supported());
        let f = Facts {
            agents: vec![claude(), codex],
            ..Facts::default()
        };
        let m = agent_menu(&f, &Plan::default());
        let rows: Vec<&str> = m.items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(rows, ["claude", "check again", "close"]);
    }

    #[test]
    fn begin_explains_the_dev_channel_on_stable_and_continues() {
        let mut host = Fake {
            facts: Facts {
                agents: vec![claude()],
                ..Facts::default()
            },
            ..Fake::default()
        };
        let m = begin(&host);
        assert!(m.title.contains("Lead"));
        let go = first_arg(&m);
        assert!(go.starts_with("go;"), "{go}");
        let next = apply(&mut host, &go);
        let menu = next.menu.expect("agent step");
        assert_eq!(menu.title, "Lead: choose the agent");
    }

    #[test]
    fn begin_on_dev_goes_straight_to_the_step() {
        let host = Fake {
            facts: Facts {
                dev_channel: true,
                agents: vec![claude()],
                ..Facts::default()
            },
            ..Fake::default()
        };
        assert_eq!(begin(&host).title, "Lead: choose the agent");
    }

    #[test]
    fn pick_then_write_runs_the_whole_config_path() {
        let mut host = Fake {
            facts: Facts {
                dev_channel: true,
                bwrap: true,
                pasta: true,
                agents: vec![claude()],
                ..Facts::default()
            },
            ..Fake::default()
        };
        let m = begin(&host);
        let pick = first_arg(&m);
        assert!(pick.starts_with("pick;"), "{pick}");
        let step = apply(&mut host, &pick);
        let write_menu = step.menu.expect("write step");
        assert_eq!(write_menu.title, "Write the Lead config?");
        // The preview is in the body of the menu the user confirms.
        let write = first_arg(&write_menu);
        let done = apply(&mut host, &write);
        assert_eq!(host.wrote.len(), 1);
        assert_eq!(host.wrote[0].0.agent, "claude");
        assert_eq!(done.status.as_deref(), Some("Lead config written"));
        assert_eq!(
            done.menu.expect("restart step").title,
            "Restart the daemon?"
        );
    }

    #[test]
    fn a_failed_write_reports_and_keeps_no_menu() {
        let mut host = Fake {
            facts: ready(),
            fail_write: true,
            ..Fake::default()
        };
        let p = Plan {
            agent: "claude".into(),
            ..Plan::default()
        };
        let out = apply(&mut host, &arg("write", &p, ""));
        assert!(out.menu.is_none());
        assert!(out.status.is_some_and(|s| s.contains("denied")));
    }

    #[test]
    fn pick_refuses_an_unsupported_or_unknown_agent() {
        let mut host = Fake {
            facts: Facts {
                agents: vec![
                    claude(),
                    AgentOption {
                        name: "codex".into(),
                        harness: "codex".into(),
                        on_path: true,
                    },
                ],
                ..Facts::default()
            },
            ..Fake::default()
        };
        for who in ["codex", "ghost"] {
            let out = apply(&mut host, &arg("pick", &Plan::default(), who));
            assert!(out.menu.is_none(), "{who}");
            assert!(out.status.is_some_and(|s| s.contains("not a supported")));
        }
    }

    #[test]
    fn missing_pasta_blocks_without_a_host_network_option() {
        let mut host = Fake {
            facts: Facts {
                dev_channel: true,
                bwrap: true,
                agents: vec![claude()],
                ..Facts::default()
            },
            ..Fake::default()
        };
        let p = Plan {
            agent: "claude".into(),
            ..Plan::default()
        };
        let m = step_menu(&host.facts, &p);
        assert_eq!(m.title, "Lead: pasta is missing");
        assert_eq!(args(&m).len(), 2, "check again and cancel only");
        let retry = args(&m).into_iter().flatten().next().unwrap();
        let out = apply(&mut host, &retry);
        assert_eq!(
            out.menu.expect("still blocked").title,
            "Lead: pasta is missing"
        );
        host.facts.pasta = true;
        let out = apply(&mut host, &retry);
        assert_eq!(out.menu.expect("write").title, "Write the Lead config?");
    }

    #[test]
    fn missing_bwrap_blocks_with_an_install_hint() {
        let f = Facts {
            agents: vec![claude()],
            ..Facts::default()
        };
        let p = Plan {
            agent: "claude".into(),
            ..Plan::default()
        };
        let m = step_menu(&f, &p);
        assert_eq!(m.title, "Lead: bubblewrap is missing");
    }

    #[test]
    fn restart_runs_off_loop_and_retries_only_after_stop_completes() {
        let mut f = ready();
        f.listener_up = false;
        f.logged_in = false;
        let mut host = Fake {
            facts: f,
            ..Fake::default()
        };
        let p = plan(&host.facts);
        assert_eq!(step_menu(&host.facts, &p).title, "Restart the daemon?");
        let out = apply(&mut host, &arg("restart", &p, ""));
        assert_eq!(out.job, Some(Job::Restart(p.clone())));
        assert!(out.menu.is_none());
        // Skipping moves on without stopping it.
        let out = apply(&mut host, &arg("skip", &p, ""));
        assert_eq!(out.menu.expect("login").title, "Log the Lead's agent in");
    }

    #[test]
    fn login_and_mcp_login_open_the_lead_verbs_in_a_tab() {
        let mut host = Fake {
            facts: ready(),
            ..Fake::default()
        };
        let p = plan(&host.facts);
        let out = apply(&mut host, &arg("login", &p, ""));
        assert_eq!(out.tab, Some(vec!["lead".to_string(), "login".to_string()]));
        let out = apply(&mut host, &arg("mcp", &p, "todoist"));
        assert_eq!(
            out.tab,
            Some(
                ["lead", "login", "--mcp", "todoist"]
                    .map(String::from)
                    .to_vec()
            )
        );
        // A server outside the Lead's profile never reaches a pane.
        let out = apply(&mut host, &arg("mcp", &p, "other"));
        assert!(out.tab.is_none());
    }

    #[test]
    fn review_lists_mcp_logins_and_open_opens_the_lead() {
        let mut host = Fake {
            facts: ready(),
            ..Fake::default()
        };
        let p = plan(&host.facts);
        let m = step_menu(&host.facts, &p);
        assert_eq!(m.title, "The Lead is ready");
        let labels: Vec<&str> = m.items().iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"log in MCP server todoist"), "{labels:?}");
        assert!(apply(&mut host, &arg("open", &p, "")).open_lead);
    }

    #[test]
    fn reset_confirms_before_removing_the_config() {
        let mut host = Fake {
            facts: ready(),
            ..Fake::default()
        };
        let p = plan(&host.facts);
        let out = apply(&mut host, &arg("reset", &p, ""));
        assert_eq!(host.resets, 0);
        let confirm = first_arg(&out.menu.expect("confirm"));
        assert!(confirm.starts_with("resetdo;"));
        let out = apply(&mut host, &confirm);
        assert_eq!(host.resets, 1);
        assert!(out.status.is_some_and(|s| s.contains("disabled")));
        assert_eq!(host.facts.file_agent, None);
    }

    #[test]
    fn unreadable_args_never_panic() {
        let mut host = Fake::default();
        assert!(apply(&mut host, "").status.is_some());
        assert!(apply(&mut host, "go;garbage").status.is_some());
        assert!(apply(&mut host, "weird;a|0|0;").status.is_some());
    }

    #[test]
    fn file_facts_read_the_lead_and_daemon_keys() {
        let (agent, host, daemon, preview) = parse_file_facts(
            "[lead]\nagent = \"claude\"\nnetwork = \"host\"\npreview = true\n[daemon]\nenabled = true\n",
        );
        assert_eq!(agent.as_deref(), Some("claude"));
        assert!(host && daemon && preview);
        assert_eq!(parse_file_facts("not toml ["), (None, false, false, false));
        assert_eq!(
            parse_file_facts("[lead]\nagent = \"\"\n"),
            (None, false, false, false)
        );
    }

    #[test]
    fn preview_shows_exactly_what_is_written() {
        let mut f = ready();
        let p = Plan {
            agent: "claude".into(),
            ..Plan::default()
        };
        assert_eq!(
            p.preview(&f),
            [
                "[lead]",
                "agent = \"claude\"",
                "preview = true",
                "[daemon]",
                "enabled = true"
            ]
        );
        f.file_host_network = true;
        assert!(
            p.preview(&f)
                .contains(&"network = \"isolated\"".to_string())
        );
    }
}
