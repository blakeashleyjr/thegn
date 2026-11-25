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
    /// Whether this process runs the dev channel (the Lead is dev-only).
    pub dev_channel: bool,
    /// Detected and configured agents.
    pub agents: Vec<AgentOption>,
    /// `[lead] agent` as written in the user config file.
    pub file_agent: Option<String>,
    /// `[lead] network = "host"` as written in the user config file.
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
    /// Use `[lead] network = "host"` (no pasta).
    pub host_network: bool,
    /// The daemon stop was already issued in this run.
    pub restarted: bool,
}

impl Plan {
    /// The plan a fresh run starts from: what the config file already says.
    pub(crate) fn from_facts(f: &Facts) -> Self {
        Plan {
            agent: f.file_agent.clone().unwrap_or_default(),
            host_network: f.file_host_network,
            restarted: false,
        }
    }

    fn encode(&self) -> String {
        format!(
            "{}|{}|{}",
            self.agent,
            u8::from(self.host_network),
            u8::from(self.restarted)
        )
    }

    fn decode(s: &str) -> Option<Plan> {
        let mut it = s.rsplitn(3, '|');
        let restarted = it.next()?;
        let host = it.next()?;
        let agent = it.next()?;
        Some(Plan {
            agent: agent.to_string(),
            host_network: host == "1",
            restarted: restarted == "1",
        })
    }

    /// The exact lines the config write adds, shown as a preview first.
    pub(crate) fn preview(&self, f: &Facts) -> Vec<String> {
        let mut lines = vec!["[lead]".to_string(), format!("agent = \"{}\"", self.agent)];
        if self.host_network {
            lines.push("network = \"host\"".into());
        } else if f.file_host_network {
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
    /// pasta is missing and the network is isolated: choose the fallback.
    NeedNetwork,
    /// Write `[lead]` and `[daemon] enabled` (with a preview).
    WriteConfig,
    /// Config is ready but this is the stable channel.
    RelaunchDev,
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
    if !plan.host_network && !f.pasta {
        return Step::NeedNetwork;
    }
    let written = f.file_agent.as_deref() == Some(plan.agent.as_str())
        && f.file_host_network == plan.host_network
        && f.daemon_enabled;
    if !written {
        return Step::WriteConfig;
    }
    if !f.dev_channel {
        return Step::RelaunchDev;
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
    /// Stop the daemon (it restarts on the next attach).
    fn stop_daemon(&mut self) -> Result<(), String>;
    /// argv for a thegn verb, run in a pane (`["lead","login"]`), with the
    /// dev channel carried over. `None` when the executable is unknown.
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

/// The first menu for Alt i: the dev-channel explanation on stable, else the
/// step the facts call for.
pub(crate) fn begin(host: &dyn Host) -> MenuOverlay {
    let f = host.facts();
    let plan = Plan::from_facts(&f);
    if f.dev_channel {
        return step_menu(&f, &plan);
    }
    choice_menu(
        TITLE,
        "The Lead is a dev-channel feature: this build runs the stable channel, so [lead] is \
         ignored until thegn is started with THEGN_CHANNEL=dev. You can still prepare the \
         config now (agent, sandbox and daemon checks, the [lead] write); the last step tells \
         you how to relaunch on dev.",
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
                 installed. {PASTA_HINT} Or run the Lead on the host network: simpler, but its \
                 sandbox then shares your network."
            ),
            TAG,
            vec![
                row('h', "use the host network", arg("host", plan, "")),
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
        Step::RelaunchDev => choice_menu(
            "Lead: relaunch on the dev channel",
            "The Lead config is written. This stable build ignores it: quit thegn and start it \
             again as `THEGN_CHANNEL=dev thegn`, then press Alt i to finish (daemon restart, \
             login, checks).",
            TAG,
            vec![
                row(
                    'x',
                    "reset: remove the [lead] config",
                    arg("reset", plan, ""),
                ),
                cancel(),
            ],
        ),
        Step::Restart => choice_menu(
            "Restart the daemon?",
            "The Lead needs the daemon to restart so it carries the Lead's control listener. \
             This stops the daemon: terminals it hosts (including Lead tabs) end, and thegn \
             starts a fresh daemon on the next attach. Windows you have open are not closed.",
            TAG,
            vec![
                row('y', "restart the daemon", arg("restart", plan, "")),
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
    let body = format!(
        "{} agent {}\n{} bubblewrap\n{} {}\n{} daemon enabled, Lead listener up\n{} agent login",
        mark(true),
        plan.agent,
        mark(f.bwrap),
        mark(plan.host_network || f.pasta),
        if plan.host_network {
            "host network"
        } else {
            "pasta (isolated network)"
        },
        mark(f.daemon_enabled && f.listener_up),
        mark(f.logged_in),
    );
    let mut rows = vec![
        row('o', "open the Lead", arg("open", plan, "")),
        row(
            'd',
            "run the full doctor checks in a tab",
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
        "host" => {
            let plan = Plan {
                host_network: true,
                ..plan
            };
            Applied {
                menu: menu_now(host, &plan),
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
        "restart" => match host.stop_daemon() {
            Ok(()) => {
                let plan = Plan {
                    restarted: true,
                    ..plan
                };
                Applied {
                    status: Some("daemon stopping; it restarts on the next attach".into()),
                    menu: menu_now(host, &plan),
                    ..Applied::default()
                }
            }
            Err(e) => Applied::status(format!("daemon not restarted: {e}")),
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
        "doctor" => match host.pane_argv(&["doctor"]) {
            Some(argv) => Applied {
                status: Some("Lead doctor checks opened in a tab".into()),
                tab: Some(argv),
                ..Applied::default()
            },
            None => Applied::status("Lead doctor: cannot find the thegn executable"),
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

/// `[lead] agent`, `[lead] network` and `[daemon] enabled` from config text.
pub(crate) fn parse_file_facts(text: &str) -> (Option<String>, bool, bool) {
    let Ok(v) = text.parse::<toml::Table>() else {
        return (None, false, false);
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
    (agent, host, daemon)
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
        let (file_agent, file_host_network, daemon_enabled) = parse_file_facts(&text);
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
            if plan.host_network {
                set_key(&path, "lead.network", "host")?;
            } else if f.file_host_network {
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

    fn stop_daemon(&mut self) -> Result<(), String> {
        let exe = thegn_core::util::self_exe_path().ok_or("cannot find the thegn executable")?;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["daemon", "stop"])
            .env("THEGN_CHANNEL", "dev")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if crate::actions::spawn_detached_reaped(cmd) {
            Ok(())
        } else {
            Err("could not start `thegn daemon stop`".into())
        }
    }

    fn pane_argv(&self, verb: &[&str]) -> Option<Vec<String>> {
        let exe = thegn_core::util::self_exe_path()?;
        // A pane starts from a scrubbed env: carry the channel over so the
        // verb sees `[lead]` the way this process does.
        let mut argv = vec!["env".to_string(), "THEGN_CHANNEL=dev".to_string()];
        argv.push(exe.to_string_lossy().into_owned());
        argv.extend(verb.iter().map(|s| (*s).to_string()));
        Some(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        facts: Facts,
        wrote: Vec<(Plan, Facts)>,
        resets: usize,
        stops: usize,
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
            self.facts.file_host_network = plan.host_network;
            self.facts.daemon_enabled = true;
            Ok(())
        }
        fn reset_config(&mut self) -> Result<(), String> {
            self.resets += 1;
            self.facts.file_agent = None;
            Ok(())
        }
        fn stop_daemon(&mut self) -> Result<(), String> {
            self.stops += 1;
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
            host_network: true,
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
        p.host_network = true;
        assert_eq!(next_step(&f, &p), Step::WriteConfig);
        f.file_agent = Some("claude".into());
        f.file_host_network = true;
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
        let p = Plan {
            host_network: true,
            ..plan(&f)
        };
        assert_eq!(next_step(&f, &p), Step::WriteConfig);
    }

    #[test]
    fn stable_channel_stops_at_relaunch_after_the_write() {
        let mut f = ready();
        f.dev_channel = false;
        assert_eq!(next_step(&f, &plan(&f)), Step::RelaunchDev);
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
            host_network: true,
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
    fn missing_pasta_offers_the_host_network_fallback() {
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
        let host_arg = args(&m).into_iter().flatten().next().unwrap();
        let out = apply(&mut host, &host_arg);
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
    fn restart_stops_the_daemon_once_and_moves_on() {
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
        assert_eq!(host.stops, 1);
        assert_eq!(out.menu.expect("login").title, "Log the Lead's agent in");
        // Skipping moves on without stopping it.
        let out = apply(&mut host, &arg("skip", &p, ""));
        assert_eq!(host.stops, 1);
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
        let out = apply(&mut host, &arg("doctor", &p, ""));
        assert_eq!(out.tab, Some(vec!["doctor".to_string()]));
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
        let (agent, host, daemon) = parse_file_facts(
            "[lead]\nagent = \"claude\"\nnetwork = \"host\"\n[daemon]\nenabled = true\n",
        );
        assert_eq!(agent.as_deref(), Some("claude"));
        assert!(host && daemon);
        assert_eq!(parse_file_facts("not toml ["), (None, false, false));
        assert_eq!(
            parse_file_facts("[lead]\nagent = \"\"\n"),
            (None, false, false)
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
            ["[lead]", "agent = \"claude\"", "[daemon]", "enabled = true"]
        );
        let h = Plan {
            host_network: true,
            ..p.clone()
        };
        assert!(h.preview(&f).contains(&"network = \"host\"".to_string()));
        f.file_host_network = true;
        assert!(
            p.preview(&f)
                .contains(&"network = \"isolated\"".to_string())
        );
    }
}
