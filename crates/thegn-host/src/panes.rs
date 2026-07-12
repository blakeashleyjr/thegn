//! The PTY pane registry + spawn/layout helpers: `Panes` owns every live
//! `PtyPane` keyed by id, materializes a tab's `CenterTree` leaves on focus,
//! pre-warms neighbor tabs, replaces dead sole center panes, and resolves the
//! shell/tool argv new panes run.

use anyhow::Result;
use tokio::sync::mpsc as tokio_mpsc;

use termwiz::terminal::TerminalWaker;

use crate::compositor::Rect;
use crate::pane::{PaneEvent, PtyPane};
use thegn_core::store::WorkspaceStore;

/// The shell argv used for new panes. Non-login interactive shells are the
/// default because login startup files are expensive and can trigger user
/// autostart logic inside the compositor. Set `THEGN_LOGIN_SHELL=1` to opt
/// back into login-shell semantics.
fn shell_argv_from(shell: &str, login: bool) -> Vec<String> {
    if login {
        return vec![shell.into(), "-l".into()];
    }
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match name {
        // Common POSIX-ish shells support `-i` for interactive non-login mode.
        "bash" | "dash" | "fish" | "ksh" | "mksh" | "sh" | "zsh" => {
            vec![shell.into(), "-i".into()]
        }
        _ => vec![shell.into()],
    }
}

fn path_is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

fn command_on_path(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| path_is_executable_file(candidate))
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

fn resolve_pane_shell(shell_env: Option<String>) -> String {
    if let Some(shell) = shell_env {
        let trimmed = shell.trim();
        if !trimmed.is_empty() && path_is_executable_file(std::path::Path::new(trimmed)) {
            return trimmed.to_string();
        }
    }

    for name in ["zsh", "bash", "fish", "sh"] {
        if let Some(shell) = command_on_path(name) {
            return shell;
        }
    }

    for shell in [
        "/etc/profiles/per-user/blake/bin/zsh",
        "/run/current-system/sw/bin/zsh",
        "/bin/zsh",
        "/run/current-system/sw/bin/bash",
        "/bin/bash",
        "/run/current-system/sw/bin/sh",
        "/bin/sh",
    ] {
        if path_is_executable_file(std::path::Path::new(shell)) {
            return shell.to_string();
        }
    }

    "/bin/sh".into()
}

pub(crate) fn pane_shell_argv(
    _cfg: &thegn_core::config::Config,
    terminal_connection: &str,
) -> (String, Vec<String>) {
    let raw = if terminal_connection.is_empty() {
        let shell = resolve_pane_shell(std::env::var("SHELL").ok());
        let login = std::env::var("THEGN_LOGIN_SHELL")
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);
        shell_argv_from(&shell, login)
    } else {
        // SSH / Mosh connection. Split the remainder on whitespace so a target
        // carrying flags (e.g. `ssh -p 2222 user@host`, as the new-terminal
        // wizard emits for a registered host on a non-default port) becomes
        // distinct argv entries rather than one mangled hostname.
        let mut v = vec![];
        if let Some(rest) = terminal_connection.strip_prefix("mosh ") {
            v.push("mosh".to_string());
            v.extend(rest.split_whitespace().map(str::to_string));
        } else {
            let rest = terminal_connection
                .strip_prefix("ssh ")
                .unwrap_or(terminal_connection);
            v.push("ssh".to_string());
            v.extend(rest.split_whitespace().map(str::to_string));
        }
        v
    };
    if raw.is_empty() {
        return ("sh".to_string(), vec![]);
    }
    let exe = raw[0].clone();
    let args = raw.into_iter().skip(1).collect();
    (exe, args)
}

/// Build a host [`crate::agent::LaunchSpec`] for a terminal connection (local
/// shell / ssh / mosh). Terminals never run inside a sandbox — a terminal is a
/// host process that itself reaches out (ssh/mosh) — so this is the shared spec
/// builder used by both the synchronous creation path and the off-thread
/// materialize/pre-warm paths, keeping the two from diverging.
pub(crate) fn terminal_launch_spec(
    cfg: &thegn_core::config::Config,
    connection: &str,
    sandbox_backend: &str,
) -> crate::agent::LaunchSpec {
    let (cmd, args) = pane_shell_argv(cfg, connection);
    let mut argv = vec![cmd];
    argv.extend(args);
    // Wrap a LOCAL shell in the chosen sandbox (a remote ssh/mosh terminal is
    // isolated by the remote end, so it's never wrapped here). This stays PURE —
    // it only builds the wrapping argv; the sandbox command self-provisions at
    // exec (bwrap runs immediately, `podman run` pulls on first use), so no
    // blocking `ensure()` runs on the event loop.
    let backend = sandbox_backend.trim();
    if connection.is_empty()
        && !backend.is_empty()
        && backend != "host"
        && backend != "none"
        && let Some(wrapped) = sandbox_wrap_shell(cfg, backend, &argv)
    {
        return crate::agent::LaunchSpec {
            argv: wrapped,
            cwd: None,
            env: vec![],
            backend: backend.to_string(),
            warnings: vec![],
        };
    }
    crate::agent::LaunchSpec {
        argv,
        cwd: None,
        env: vec![],
        backend: "host".to_string(),
        warnings: vec![],
    }
}

/// Build the sandbox-wrapping argv for a local terminal shell: force the chosen
/// `backend` into a [`thegn_core::sandbox::SandboxSpec`] anchored at `$HOME`
/// and `exec` the shell inside it (via
/// [`thegn_core::sandbox::enter_argv`]). Returns `None` — so the caller falls
/// back to a plain host shell — when the backend name is unknown or the spec
/// can't be built (e.g. sandboxing disabled). Pure: no provisioning, safe on the
/// event loop.
fn sandbox_wrap_shell(
    cfg: &thegn_core::config::Config,
    backend: &str,
    shell_argv: &[String],
) -> Option<Vec<String>> {
    let be = thegn_core::config::SandboxBackend::from_str_validated(backend).ok()?;
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    let mut sb = cfg.sandbox.clone();
    sb.enabled = true;
    sb.backend = be;
    let loc = thegn_core::remote::GitLoc::for_worktree(std::path::Path::new(&home));
    let name = thegn_core::sandbox::container_name(&home);
    let spec = thegn_core::sandbox::resolve_placed(
        &sb,
        &loc,
        &name,
        sb.profile,
        thegn_core::placement::Placement::Local,
    )?;
    // `enter_argv` execs `inner` as the pane's foreground program; the shell
    // argv (path + login flags) is the interactive shell that owns the pane.
    let inner = shell_words_join(shell_argv);
    Some(thegn_core::sandbox::enter_argv(&spec, &inner))
}

/// Join a shell argv into a single command string for `sh -lc` execution,
/// single-quoting any element that isn't a bare word so paths/flags survive.
fn shell_words_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn tool_drawer_argv(command: &str) -> Vec<String> {
    vec![
        thegn_core::util::shell(),
        "-lc".into(),
        format!("exec {}", command.trim()),
    ]
}

/// Env for spawning yazi: an isolated `YAZI_CONFIG_HOME` so the user's own
/// `~/.config/yazi` (often written for a different yazi version — schema
/// breakage shows as TOML errors on every launch) can't break the drawer.
/// `[drawer] config_home`: `""` = a private thegn dir seeded once from the
/// bundled config, `"system"` = the user's own config, else an explicit path.
pub(crate) fn yazi_env(cfg: &thegn_core::config::Config) -> Vec<(String, String)> {
    let home = cfg.drawer.config_home.trim();
    let dir = match home {
        "system" => return Vec::new(),
        "" => {
            let dir = thegn_core::util::thegn_dir().join("yazi");
            if let Err(e) = seed_yazi_config(&dir) {
                tracing::warn!(target: "thegn", error = %e, "yazi config seed failed");
                return Vec::new();
            }
            dir
        }
        path => std::path::PathBuf::from(thegn_core::util::expand_tilde(path)),
    };
    vec![(
        "YAZI_CONFIG_HOME".into(),
        dir.to_string_lossy().into_owned(),
    )]
}

/// Write the bundled yazi config files into `dir` (only the missing ones, so
/// user tweaks to the private copy survive).
fn seed_yazi_config(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, body) in [
        ("yazi.toml", include_str!("../../../config/yazi/yazi.toml")),
        (
            "theme.toml",
            include_str!("../../../config/yazi/theme.toml"),
        ),
        (
            "keymap.toml",
            include_str!("../../../config/yazi/keymap.toml"),
        ),
    ] {
        let p = dir.join(name);
        if !p.exists() {
            std::fs::write(p, body)?;
        }
    }
    Ok(())
}

/// The global pane registry. A tab's panes are identified by the real ids in its
/// `CenterTree`; this just owns the live `PtyPane`s keyed by id.
pub(crate) struct Panes {
    pub(crate) table: std::collections::HashMap<u32, PtyPane>,
    next_id: u32,
    tx: tokio_mpsc::Sender<PaneEvent>,
    /// Pulsed by reader threads after each send so the main loop's blocking
    /// `poll_input(None)` wakes to drain PTY output. `None` in unit tests that
    /// construct `Panes` without a live terminal.
    waker: Option<TerminalWaker>,
    /// Wall-clock time each pane was spawned. Used by the crash debounce: if a
    /// pane exits within the threshold it's counted as a crash even if it wrote
    /// output (bwrap prints an error message before dying).
    spawn_times: std::collections::HashMap<u32, std::time::Instant>,
    /// The host tokio runtime handle, used to drive native-exec (`Stream`) panes'
    /// relay tasks. Captured at construction (present at runtime); `None` in unit
    /// tests that build `Panes` outside a runtime.
    rt: Option<tokio::runtime::Handle>,
    /// `[replay]` config — when `enabled`, each newly spawned pane gets a
    /// recording ring attached. `None`/disabled ⇒ panes record nothing.
    replay_cfg: Option<thegn_core::config::ReplayConfig>,
    /// `[daemon]` config — when `enabled`, new local panes route through the
    /// pane daemon (control plane) and survive UI exit. `None` ⇒ today's
    /// in-process PTYs, byte-for-byte.
    daemon_cfg: Option<thegn_core::config::DaemonConfig>,
}

impl Panes {
    #[cfg(test)]
    pub(crate) fn new(tx: tokio_mpsc::Sender<PaneEvent>) -> Self {
        Self {
            table: std::collections::HashMap::new(),
            next_id: 1,
            tx,
            waker: None,
            spawn_times: std::collections::HashMap::new(),
            rt: tokio::runtime::Handle::try_current().ok(),
            replay_cfg: None,
            daemon_cfg: None,
        }
    }

    pub(crate) fn with_waker(tx: tokio_mpsc::Sender<PaneEvent>, waker: TerminalWaker) -> Self {
        Self {
            table: std::collections::HashMap::new(),
            next_id: 1,
            tx,
            waker: Some(waker),
            spawn_times: std::collections::HashMap::new(),
            rt: tokio::runtime::Handle::try_current().ok(),
            replay_cfg: None,
            daemon_cfg: None,
        }
    }

    /// Install the `[replay]` config so subsequently spawned panes attach a
    /// recording ring when replay is enabled. Called at startup and on config
    /// reload.
    pub(crate) fn set_replay_config(&mut self, cfg: thegn_core::config::ReplayConfig) {
        self.replay_cfg = if cfg.enabled { Some(cfg) } else { None };
    }

    /// Install the `[daemon]` config: when enabled, subsequently spawned local
    /// panes route through the pane daemon (surviving UI exit). Called at
    /// startup and on config reload; existing panes keep their transport.
    pub(crate) fn set_daemon_config(&mut self, cfg: thegn_core::config::DaemonConfig) {
        self.daemon_cfg = if cfg.enabled { Some(cfg) } else { None };
    }

    /// Attach a fresh recording ring to a just-spawned pane when replay is on.
    fn maybe_record(&mut self, id: u32, rows: u16, cols: u16) {
        if let Some(cfg) = &self.replay_cfg
            && let Some(pane) = self.table.get_mut(&id)
        {
            pane.enable_recording(crate::replay::Recording::from_config(cfg, rows, cols));
        }
    }

    /// Spawn one shell pane in `cwd`, sized to `center`; returns its id.
    pub(crate) fn spawn(
        &mut self,
        cfg: &thegn_core::config::Config,
        cwd: Option<&std::path::Path>,
        center: Rect,
    ) -> Result<u32> {
        let (cmd, args) = pane_shell_argv(cfg, "");
        let mut argv = vec![cmd];
        argv.extend(args);
        self.spawn_argv(&argv, cwd, center)
    }

    /// Spawn a specific argv in `cwd`, sized to `center`; returns its id.
    pub(crate) fn spawn_argv(
        &mut self,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        center: Rect,
    ) -> Result<u32> {
        self.spawn_argv_env(argv, cwd, &[], center)
    }

    /// As [`Panes::spawn_argv`], but injects `env` into the child — used for
    /// agent panes that expect `THEGN_WORKTREE`/`THEGN_BRANCH` and for
    /// per-program env on pinned programs.
    ///
    /// The single spawn chokepoint for local panes: with `[daemon] enabled`
    /// the pane routes through the pane daemon (surviving UI exit); a daemon
    /// route that can't even be *constructed* (no runtime handle) falls back
    /// to the in-process PTY so the shell always works.
    pub(crate) fn spawn_argv_env(
        &mut self,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        center: Rect,
    ) -> Result<u32> {
        if self.daemon_cfg.is_some() {
            match self.spawn_daemon_backed(argv, cwd, env, center, None) {
                Ok(id) => return Ok(id),
                Err(e) => {
                    tracing::warn!(
                        target: "thegn::daemon",
                        "daemon-backed spawn unavailable; using in-process PTY: {e}"
                    );
                }
            }
        }
        self.spawn_in_process(argv, cwd, env, center)
    }

    /// As [`Panes::spawn_argv_env`], but ALWAYS in-process — never
    /// daemon-routed. For ephemeral chrome panes (pins, the tool drawer, the
    /// corner overlay): they belong to no tab's persisted center tree, so a
    /// daemon session backing one would outlive the UI as an orphan lease
    /// nobody ever reattaches.
    pub(crate) fn spawn_argv_env_local(
        &mut self,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        center: Rect,
    ) -> Result<u32> {
        self.spawn_in_process(argv, cwd, env, center)
    }

    /// The in-process PTY spawn shared by the chokepoint's fallback and the
    /// ephemeral-local path.
    fn spawn_in_process(
        &mut self,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        center: Rect,
    ) -> Result<u32> {
        let id = self.next_id;
        self.next_id += 1;
        let pane = PtyPane::spawn_with_env(
            id,
            argv,
            cwd,
            env,
            center.rows.max(1) as u16,
            center.cols.max(1) as u16,
            self.tx.clone(),
            self.waker.clone(),
        )?;
        self.table.insert(id, pane);
        self.spawn_times.insert(id, std::time::Instant::now());
        self.maybe_record(id, center.rows.max(1) as u16, center.cols.max(1) as u16);
        Ok(id)
    }

    /// Spawn a pane owned by the pane daemon: a `Stream` pane whose source
    /// lazily ensures the daemon inside the relay task (never blocking the
    /// event loop), opening a fresh session — or, with `attach`, warm-
    /// reattaching a persisted one (snapshot + live deltas), with the fresh
    /// spec as the reconnect ladder's fallback.
    pub(crate) fn spawn_daemon_backed(
        &mut self,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        env: &[(String, String)],
        center: Rect,
        attach: Option<String>,
    ) -> Result<u32> {
        let dcfg = self
            .daemon_cfg
            .clone()
            .ok_or_else(|| anyhow::anyhow!("[daemon] not enabled"))?;
        let rt = self
            .rt
            .clone()
            .ok_or_else(|| anyhow::anyhow!("daemon panes need a tokio runtime handle"))?;
        let rows = center.rows.max(1) as u16;
        let cols = center.cols.max(1) as u16;
        let cwd_s = cwd.map(|p| p.to_string_lossy().into_owned());
        let spec = thegn_svc::provider::ExecSpec {
            argv: argv.to_vec(),
            tty: true,
            cols,
            rows,
            env: env.to_vec(),
            cwd: cwd_s.clone(),
        };
        let open = match attach {
            Some(session) => crate::pane::ExecOpen::Attach {
                session,
                cols,
                rows,
                fallback: spec,
            },
            None => crate::pane::ExecOpen::Open(spec),
        };
        let source = std::sync::Arc::new(crate::daemon::client::LazyDaemonSource {
            cfg: dcfg,
            worktree: cwd_s.clone(),
        });
        let id = self.next_id;
        self.next_id += 1;
        let pane = PtyPane::spawn_stream(
            id,
            source,
            "daemon".to_string(),
            cwd_s.unwrap_or_else(|| "local".to_string()),
            open,
            crate::pane::program_name(argv),
            rows,
            cols,
            self.tx.clone(),
            self.waker.clone(),
            &rt,
        );
        self.table.insert(id, pane);
        self.spawn_times.insert(id, std::time::Instant::now());
        self.maybe_record(id, rows, cols);
        Ok(id)
    }

    /// Spawn a native provider-exec (`Stream`) pane — the CLI-free interactive
    /// pane backed by a managed sandbox's exec API. `provider` + `sandbox_id`
    /// identify the sandbox; `open` is a fresh exec or a reattach. The relay task
    /// surfaces a connect/exec failure as an error husk + non-zero exit, so the
    /// caller's fallback path can take over. Needs a runtime handle.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_native(
        &mut self,
        provider: thegn_svc::provider::Provider,
        provider_name: String,
        sandbox_id: String,
        open: crate::pane::ExecOpen,
        program: String,
        center: Rect,
    ) -> Result<u32> {
        let rt = self
            .rt
            .clone()
            .ok_or_else(|| anyhow::anyhow!("native exec needs a tokio runtime handle"))?;
        let id = self.next_id;
        self.next_id += 1;
        let source = std::sync::Arc::new(crate::pane_source::ProviderSource {
            provider,
            provider_name: provider_name.clone(),
            sandbox_id: sandbox_id.clone(),
        });
        let pane = PtyPane::spawn_stream(
            id,
            source,
            provider_name,
            sandbox_id,
            open,
            program,
            center.rows.max(1) as u16,
            center.cols.max(1) as u16,
            self.tx.clone(),
            self.waker.clone(),
            &rt,
        );
        self.table.insert(id, pane);
        self.spawn_times.insert(id, std::time::Instant::now());
        self.maybe_record(id, center.rows.max(1) as u16, center.cols.max(1) as u16);
        Ok(id)
    }

    /// Open a CLI-free `Stream` pane from a resolved [`crate::agent::NativeShell`]:
    /// a fresh login shell (or agent exec) inside the sandbox. `session` (when
    /// set) reattaches a persisted provider session instead — replaying its
    /// scrollback. `program` labels the pane (`PtyPane::program`): the agent's
    /// command stem for agent execs, `"sh"` for a plain shell — it keys
    /// per-program keybind overlays and the activity output signal's shell
    /// exclusion, so an agent pane must not be mislabeled as an idle shell.
    pub(crate) fn spawn_native_shell(
        &mut self,
        n: crate::agent::NativeShell,
        session: Option<String>,
        program: String,
        center: Rect,
    ) -> Result<u32> {
        let cols = center.cols.max(1) as u16;
        let rows = center.rows.max(1) as u16;
        let open = match session {
            // Carry a fresh-open spec as the fallback: if this reattach hits a dead
            // session (didn't survive a restart / the sandbox was suspended), the
            // relay's reconnect loop re-opens a fresh shell instead of flapping on
            // the corpse.
            Some(session) => crate::pane::ExecOpen::Attach {
                session,
                cols,
                rows,
                fallback: n.open_spec(cols, rows),
            },
            None => crate::pane::ExecOpen::Open(n.open_spec(cols, rows)),
        };
        self.spawn_native(
            n.provider,
            n.provider_name,
            n.sandbox_id,
            open,
            program,
            center,
        )
    }

    /// How long the pane has been alive. `None` if the id is unknown.
    /// Used by the crash debounce: exits within 5s of spawn are fast-crashes.
    pub(crate) fn pane_age(&self, id: u32) -> Option<std::time::Duration> {
        self.spawn_times.get(&id).map(|t| t.elapsed())
    }

    /// Remove the spawn-time entry for a pane that has exited.
    pub(crate) fn forget_spawn_time(&mut self, id: u32) {
        self.spawn_times.remove(&id);
    }

    /// Reserve `n` fresh, never-reused pane ids and return the first. Used to
    /// remap a cold-resurrected workspace's persisted tree ids onto a disjoint
    /// range so they cannot collide with the live panes of other resident
    /// workspaces — which, unlike before, we no longer reap on a workspace
    /// switch. The reserved ids are placeholders: `materialize_with_specs`
    /// spawns real panes (allocating even fresher ids) and remaps onto them.
    pub(crate) fn reserve_ids(&mut self, n: u32) -> u32 {
        let base = self.next_id;
        self.next_id += n;
        base
    }

    /// The leaves of `tab.center` not yet backed by live panes — the targets
    /// a spec-resolution pass must cover before [`Self::materialize_with_specs`].
    pub(crate) fn missing_leaves(&self, tab: &crate::session::Tab) -> Vec<u32> {
        tab.center
            .pane_ids()
            .into_iter()
            .filter(|id| !self.table.contains_key(id))
            .collect()
    }

    /// Finish materialization with pre-resolved launch specs: spawn a pane per
    /// missing leaf and remap the tree's leaf ids + focused id onto them. Spec
    /// resolution (sandbox ensure, DB lookups — potentially SLOW: a wedged
    /// podman pulls an image) happens off-thread via the loop's spec channel;
    /// this half is openpty+exec only, safe on the loop. `worktree` is the
    /// owning group's dir (tabs spawn their shells there).
    pub(crate) fn materialize_with_specs(
        &mut self,
        cfg: &thegn_core::config::Config,
        tab: &mut crate::session::Tab,
        worktree: &str,
        specs: &[(u32, crate::agent::LaunchSpec)],
        center: Rect,
    ) -> Result<()> {
        let cwd = (!worktree.is_empty() && std::path::Path::new(worktree).is_dir())
            .then(|| std::path::PathBuf::from(worktree))
            .or_else(|| std::env::current_dir().ok())
            .or_else(|| std::env::var("HOME").ok().map(std::path::PathBuf::from));

        let spawn_t0 = std::time::Instant::now();
        let mut map = std::collections::HashMap::new();
        for (old, spec) in specs {
            if self.table.contains_key(old) || map.contains_key(old) {
                continue; // raced a direct spawn; keep the live pane
            }
            // Pane-daemon warm-reattach: a persisted `provider = "daemon"`
            // session means this leaf's process may still be alive in the pane
            // daemon — reattach it (snapshot + live deltas) instead of spawning
            // fresh. The spec's argv rides along as the reconnect fallback, so
            // a reaped/expired session degrades to a fresh daemon shell.
            if self.daemon_cfg.is_some()
                && let Some(ps) = tab
                    .pane_sessions
                    .get(old)
                    .filter(|s| s.provider == "daemon")
            {
                let session = ps.session.clone();
                match self.spawn_daemon_backed(
                    &spec.argv,
                    spec.cwd.as_deref().or(cwd.as_deref()),
                    &spec.env,
                    center,
                    Some(session),
                ) {
                    Ok(fresh) => {
                        // Stash the restore payload the loop applies if the
                        // reattach turns out to be dead (lease expired / the
                        // daemon restarted — e.g. after a reboot) and the
                        // relay degrades to a fresh session
                        // (`PaneEvent::SessionFallback`): the persisted
                        // scrollback tail + the recorded foreground command.
                        if let Some(p) = self.table.get_mut(&fresh) {
                            p.set_fallback_restore(
                                tab.pane_scrollback.get(old).cloned(),
                                tab.pane_cmds
                                    .get(old)
                                    .map(|c| c.display())
                                    .filter(|s| !s.is_empty()),
                            );
                        }
                        map.insert(*old, fresh);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(target: "thegn::startup", "daemon reattach failed, falling back: {e}");
                    }
                }
            }
            // SSH-over-WSS (`connect = "ssh"`): attach the leaf as a LOCAL `ssh`
            // client tunneled over the `sprite-proxy` ProxyCommand — ssh owns the
            // PTY (no hand-rolled WSS relay). A normal local PTY pane. Checked
            // before native exec so it takes precedence when configured.
            if !worktree.is_empty()
                && let Some((key, user, workdir)) = crate::agent::sprite_ssh_connect(cfg, worktree)
            {
                let exe = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.to_str().map(str::to_string))
                    .unwrap_or_else(|| "thegn".to_string());
                let argv = crate::agent::sprite_ssh_argv(&exe, worktree, &key, &user, &workdir);
                match self.spawn_argv_env(&argv, cwd.as_deref(), &[], center) {
                    Ok(fresh) => {
                        map.insert(*old, fresh);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(target: "thegn::startup", "ssh-over-wss spawn failed, falling back: {e}");
                    }
                }
            }
            // Native provider exec (CLI-free): when this worktree's env wants it,
            // attach the leaf over the provider's WSS exec. A persisted session id
            // for this leaf reattaches the live remote session (replays
            // scrollback); otherwise a fresh login shell is opened. The off-thread
            // `launch_spec` still ran (so the sandbox is provisioned); its CLI argv
            // is simply unused on this path. Falls through to the PTY path if the
            // env isn't a native-exec provider or the provider can't be built.
            //
            // Run the worktree's chosen AGENT in the sprite (managed pi / claude /
            // codex / …) over the same native exec — not just a shell — so picking
            // an agent for a sprite actually launches it (the provider CLI prefix it
            // would otherwise need isn't installed on the host). The remembered
            // choice (set by `launch_spec` at create) drives it; absent / "shell" ⇒
            // the login shell.
            let native = if worktree.is_empty() {
                None
            } else {
                thegn_core::db::Db::open()
                    .ok()
                    .and_then(|db| db.worktree_agent(worktree).ok().flatten())
                    // A remembered tool drawer (yazi/…) is not the worktree's
                    // agent — fall through to the shell rather than resuming it.
                    .filter(|c| cfg.tool_command(c).is_none())
                    .as_deref()
                    .and_then(|c| {
                        crate::agent::native_agent_exec(cfg, worktree, c).map(|n| {
                            // Label the pane with the agent, not the exec's
                            // `/bin/sh -lc` wrapper.
                            let cmd = cfg.agent_command(c).unwrap_or(c);
                            (n, crate::pane::agent_program_name(cmd, c))
                        })
                    })
                    .or_else(|| {
                        crate::agent::native_shell_exec(cfg, worktree).map(|n| (n, "sh".into()))
                    })
            };
            if let Some((n, program)) = native {
                let session = tab.pane_sessions.get(old).map(|s| s.session.clone());
                match self.spawn_native_shell(n, session, program, center) {
                    Ok(fresh) => {
                        map.insert(*old, fresh);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(target: "thegn::startup", "native exec spawn failed, falling back: {e}");
                    }
                }
            }
            // Restore this leaf's last-known cwd when we have one. Only host
            // panes are honored: a sandbox/remote launch wraps the spawn in a
            // runtime whose `cwd` is the container spec, not a host path, so
            // overriding it would break the launch — those fall back to the
            // worktree root. A recorded dir that no longer exists is ignored.
            let leaf_cwd: Option<std::path::PathBuf> = if spec.backend == "host" {
                tab.pane_cwds
                    .get(old)
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.is_dir())
            } else {
                None
            };
            let spawn_cwd = leaf_cwd
                .as_deref()
                .or(spec.cwd.as_deref())
                .or(cwd.as_deref());
            match self.spawn_argv_env(&spec.argv, spawn_cwd, &spec.env, center) {
                Ok(fresh) => {
                    // Repaint this leaf's captured scrollback so the restored pane
                    // shows its recent history before the fresh shell produces new
                    // output. Host panes only: a stream pane replays its scrollback
                    // server-side, and a sandbox pane's host-side tail isn't stored.
                    if spec.backend == "host"
                        && let Some(text) = tab.pane_scrollback.get(old)
                        && let Some(p) = self.table.get_mut(&fresh)
                    {
                        p.repaint_scrollback(text);
                    }
                    // Offer to relaunch the program this leaf was last running.
                    // Host panes only: a sandbox/remote pane's captured command
                    // isn't host-runnable, and its cwd wasn't restored either.
                    if spec.backend == "host"
                        && let Some(cmd) = tab.pane_cmds.get(old)
                    {
                        let line = cmd.display();
                        if !line.is_empty()
                            && let Some(p) = self.table.get_mut(&fresh)
                        {
                            p.set_pending_relaunch(Some(line));
                        }
                    }
                    map.insert(*old, fresh);
                }
                Err(e) => {
                    let _ = std::fs::write(
                        "/tmp/thegn-spawn-err.log",
                        format!("Materialize spawn failed: {e:?}"),
                    );
                    return Err(e);
                }
            }
        }
        tracing::info!(
            target: "thegn::startup",
            spawn_ms = spawn_t0.elapsed().as_millis() as u64,
            panes = map.len(),
            "pty panes spawned"
        );
        let old_focus = tab.focused_pane;
        tab.center
            .remap(&mut |old| map.get(&old).copied().unwrap_or(old));
        tab.focused_pane = map
            .get(&old_focus)
            .copied()
            .or_else(|| tab.center.pane_ids().first().copied())
            .unwrap_or(0);
        Ok(())
    }
}

/// Tab indices to pre-warm: the `radius` neighbors on each side of `active`,
/// clamped to `[0, len)` and excluding `active` itself. Pure for unit testing.
fn prewarm_targets(active: usize, len: usize, radius: usize) -> Vec<usize> {
    if len == 0 || radius == 0 {
        return Vec::new();
    }
    let lo = active.saturating_sub(radius);
    let hi = (active + radius).min(len.saturating_sub(1));
    (lo..=hi).filter(|&i| i != active).collect()
}

/// Radius for pre-warming: immediate neighbors only, so we never fork a child
/// per tab on a large session.
const PREWARM_RADIUS: usize = 1;

/// The (group name, worktree path, tab, missing leaf ids) tuples a pre-warm
/// pass should resolve specs for: the tabs adjacent to the active one (within
/// the active worktree) and the neighboring worktrees' active tabs, so first
/// focus of a neighbor is instant. Pure enumeration — the caller requests
/// launch specs off-thread (sandbox ensure can block) and finishes the spawns
/// when they land, exactly like the lazy materialize path. The group name is
/// the routing key (unique per session); the path is the spawn cwd.
/// Pre-warm requests as `(group name, worktree path, tab index, missing leaves,
/// is_terminal)`. The `is_terminal` flag tells the caller's off-thread spec
/// resolver to build the spec from the terminal's connection (ssh/mosh/local)
/// rather than `launch_spec` over the — empty, for terminals — worktree path.
pub(crate) fn prewarm_requests(
    panes: &Panes,
    session: &mut crate::session::Session,
) -> Vec<(String, String, usize, Vec<u32>, bool)> {
    let mut out = Vec::new();
    if session.worktrees.is_empty() {
        return out;
    }
    // Sibling tabs within the active worktree.
    let g = &session.worktrees[session.active];
    let is_term = g.kind == crate::session::GroupKind::Terminal;
    for ti in prewarm_targets(g.active_tab, g.tabs.len(), PREWARM_RADIUS) {
        let missing = panes.missing_leaves(&g.tabs[ti]);
        if !missing.is_empty() {
            out.push((g.name.clone(), g.path.clone(), ti, missing, is_term));
        }
    }
    // Neighboring worktrees: their remembered active tab.
    for gi in prewarm_targets(session.active, session.worktrees.len(), PREWARM_RADIUS) {
        let g = &mut session.worktrees[gi];
        let at = g.active_tab.min(g.tabs.len().saturating_sub(1));
        g.active_tab = at;
        let is_term = g.kind == crate::session::GroupKind::Terminal;
        if let Some(tab) = g.tabs.get(at) {
            let missing = panes.missing_leaves(tab);
            if !missing.is_empty() {
                out.push((g.name.clone(), g.path.clone(), at, missing, is_term));
            }
        }
    }
    out
}

/// Resize each pane in `tree` to its CONTENT rect within `center` (inside the
/// 1-cell border ring the framed layout reserves — the PTY must agree with
/// what `compose_pane` paints).
pub(crate) fn relayout(panes: &mut Panes, tree: &crate::center::CenterTree, center: Rect) {
    for (id, _, content) in tree.layout_framed(center) {
        // A degenerate rect means the center is hidden behind a full-screen
        // panel / zoomed zone (`compute_full`/`compute_chrome` collapse it to
        // the mandatory single column). Resizing the PTY to ~1 col makes vt100
        // reflow the shell to one column and spill its content into scrollback,
        // which a later grow-back can't restore — the pane comes back blank.
        // Leave hidden panes at their real size; relayout runs again with a
        // real rect when the panel retracts.
        if content.cols <= 1 || content.rows <= 1 {
            continue;
        }
        if let Some(p) = panes.table.get_mut(&id) {
            let _ = p.resize(content.rows.max(1) as u16, content.cols.max(1) as u16);
        }
    }
}

/// Resize every live strip pane to the rect the supervisor apportions it
/// (minus the 1-row header).
pub(crate) fn relayout_strip(
    panes: &mut Panes,
    supervisor: &crate::pins::PinSupervisor,
    strip: Rect,
) {
    for (id, rect) in supervisor.strip_layout(strip) {
        if let Some(p) = panes.table.get_mut(&id) {
            let body_rows = rect.rows.saturating_sub(1).max(1);
            let _ = p.resize(body_rows as u16, rect.cols.max(1) as u16);
        }
    }
}

/// Replace an externally-dead sole center pane with a fresh shell pane without
/// closing the workspace tab. Explicit close-pane/close-worktree actions remove
/// panes from the session before their process exits, so this only handles
/// unexpected PTY child exits (killed shell, missing old child, etc.).
pub(crate) fn replace_single_dead_center_pane(
    tab: &mut crate::session::Tab,
    dead_id: u32,
    fresh_id: u32,
) -> bool {
    let ids = tab.center.pane_ids();
    if ids.as_slice() != [dead_id] {
        return false;
    }

    tab.center = crate::center::CenterTree::Leaf(fresh_id);
    tab.focused_pane = fresh_id;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout;
    use crate::session::{GroupKind, Session, WorktreeGroup};

    fn one_tab_session() -> Session {
        Session {
            id: "s1".into(),
            worktrees: vec![WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app")],
            active: 0,
        }
    }

    #[test]
    fn prewarm_requests_key_by_group_name_so_same_path_groups_stay_distinct() {
        // Two groups can share a worktree path (the resurrect adoption loop can
        // adopt two registry rows for one dir under different tab names). The
        // spec roundtrip must key by the unique group NAME, not the path, or a
        // result would route to the wrong group and the active tab + a same-path
        // neighbor would collide on one key. Assert prewarm carries the name.
        let mut session = Session {
            id: "s1".into(),
            worktrees: vec![
                WorktreeGroup::new("app/home", GroupKind::Home, "/tmp/app"),
                WorktreeGroup::new("app/dup", GroupKind::Branch, "/tmp/app"),
            ],
            active: 0,
        };
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(16);
        let panes = Panes::new(tx); // empty table → every leaf is "missing"

        let reqs = prewarm_requests(&panes, &mut session);
        let neighbor = reqs
            .iter()
            .find(|(name, _, _, _, _)| name == "app/dup")
            .expect("the same-path neighbor is pre-warmed under its own name");
        assert_eq!(neighbor.1, "/tmp/app", "the path is carried for the cwd");
        // The routing key (name, ti) is distinct from the active group's,
        // despite the shared path.
        assert_ne!(neighbor.0, "app/home");
    }

    #[test]
    fn terminal_launch_spec_builds_ssh_mosh_and_local_argv() {
        let cfg = thegn_core::config::Config::default();

        let ssh = terminal_launch_spec(&cfg, "ssh user@host", "");
        assert_eq!(ssh.argv, vec!["ssh".to_string(), "user@host".to_string()]);
        assert_eq!(ssh.backend, "host");
        assert!(ssh.cwd.is_none());

        // A bare target (no "ssh " prefix) is still treated as an ssh target.
        let bare = terminal_launch_spec(&cfg, "user@host", "");
        assert_eq!(bare.argv, vec!["ssh".to_string(), "user@host".to_string()]);

        let mosh = terminal_launch_spec(&cfg, "mosh user@host", "");
        assert_eq!(mosh.argv, vec!["mosh".to_string(), "user@host".to_string()]);

        // A target carrying flags (registered host on a non-default port) splits
        // into distinct argv entries, not one mangled hostname.
        let ported = terminal_launch_spec(&cfg, "ssh -p 2222 user@host", "");
        assert_eq!(
            ported.argv,
            vec![
                "ssh".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "user@host".to_string()
            ]
        );

        // Empty connection → a local interactive shell (argv[0] is env-dependent).
        let local = terminal_launch_spec(&cfg, "", "");
        assert!(!local.argv.is_empty());
        assert_eq!(local.backend, "host");

        // `host`/`none` are no-op backends: still a plain host shell.
        let host = terminal_launch_spec(&cfg, "", "host");
        assert_eq!(host.backend, "host");

        // A remote terminal is never wrapped locally even with a backend set.
        let remote_wrapped = terminal_launch_spec(&cfg, "ssh user@host", "bwrap");
        assert_eq!(remote_wrapped.argv[0], "ssh");
        assert_eq!(remote_wrapped.backend, "host");
    }

    #[test]
    fn terminal_launch_spec_wraps_local_shell_in_bwrap() {
        // A local shell + bwrap backend wraps the shell in a sandbox runtime and
        // exec's the raw shell inside. WHICH runtime is host-dependent:
        // `resolve_placed` selects bwrap only when it's actually available, and
        // otherwise falls THROUGH the sandbox chain to whatever is installed
        // (podman/docker on a CI runner with no bwrap) or a plain host shell.
        // So assert against the ACTUAL resolved argv — never the requested-
        // backend label (`terminal_launch_spec` records the requested name even
        // when the chain fell through) — so this test can't fail based on which
        // sandbox backends happen to be installed on the host.
        let mut cfg = thegn_core::config::Config::default();
        cfg.sandbox.enabled = true;
        let spec = terminal_launch_spec(&cfg, "", "bwrap");
        assert!(!spec.argv.is_empty());
        // When bwrap is the runtime that actually resolved, it must front the
        // wrapping argv (the shell is exec'd inside) — modulo the optional
        // cpu-priority prefix (`ionice -c3 nice -n N`) that `sandbox_cpucap`
        // prepends when the host lacks cgroup `cpu` delegation. If bwrap didn't
        // resolve, the chain fell through — nothing bwrap-specific to prove.
        if let Some(pos) = spec.argv.iter().position(|a| a.contains("bwrap")) {
            let prefix_ok = spec.argv[..pos].iter().all(|a| {
                matches!(a.as_str(), "ionice" | "-c3" | "nice" | "-n") || a.parse::<i64>().is_ok()
            });
            assert!(
                prefix_ok,
                "only a cpu-priority prefix may precede bwrap: {:?}",
                spec.argv
            );
        }
    }

    #[test]
    fn prewarm_targets_returns_clamped_neighbors_excluding_active() {
        // Middle of a list: both neighbors.
        assert_eq!(prewarm_targets(2, 5, 1), vec![1, 3]);
        // First tab: only the right neighbor.
        assert_eq!(prewarm_targets(0, 5, 1), vec![1]);
        // Last tab: only the left neighbor.
        assert_eq!(prewarm_targets(4, 5, 1), vec![3]);
        // Single tab: nothing to pre-warm.
        assert_eq!(prewarm_targets(0, 1, 1), Vec::<usize>::new());
        // Empty / zero-radius: nothing.
        assert_eq!(prewarm_targets(0, 0, 1), Vec::<usize>::new());
        assert_eq!(prewarm_targets(2, 5, 0), Vec::<usize>::new());
        // Wider radius clamps at the ends.
        assert_eq!(prewarm_targets(1, 5, 2), vec![0, 2, 3]);
    }

    #[test]
    fn relayout_skips_panes_hidden_behind_a_fullscreen_panel() {
        // Spawned panes read SHELL; pin it to one that exists, restored on drop.
        let _env = crate::testenv::EnvVarGuard::set(&[("SHELL", "/bin/sh")]);
        let mut session = one_tab_session();
        let chrome = layout::compute(160, 40, true, true);
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        let path = session.worktrees[0].path.clone();
        let mut cfg = thegn_core::config::Config::default();
        cfg.sandbox.enabled = false;
        let specs: Vec<(u32, crate::agent::LaunchSpec)> = panes
            .missing_leaves(&session.worktrees[0].tabs[0])
            .into_iter()
            .map(|id| {
                (
                    id,
                    crate::agent::launch_spec(&cfg, &path, None, "shell").unwrap(),
                )
            })
            .collect();
        panes
            .materialize_with_specs(
                &cfg,
                &mut session.worktrees[0].tabs[0],
                &path,
                &specs,
                chrome.center,
            )
            .unwrap();

        let tree = session.worktrees[0].tabs[0].center.clone();
        let id = match &tree {
            crate::center::CenterTree::Leaf(id) => *id,
            _ => panic!("expected a single leaf pane"),
        };

        // A real rect sizes the pane.
        relayout(
            &mut panes,
            &tree,
            Rect {
                x: 0,
                y: 0,
                cols: 120,
                rows: 30,
            },
        );
        let real = panes.table.get(&id).unwrap().size();
        assert!(
            real.0 > 1 && real.1 > 1,
            "pane sized to a real rect: {real:?}"
        );

        // A degenerate rect (center hidden behind a full-screen panel) must NOT
        // resize the pane — that 1-col reflow destroys the shell's content.
        relayout(
            &mut panes,
            &tree,
            Rect {
                x: 0,
                y: 0,
                cols: 1,
                rows: 30,
            },
        );
        assert_eq!(
            panes.table.get(&id).unwrap().size(),
            real,
            "a hidden (1-col) pane keeps its real size"
        );

        // Retracting back to a real rect resizes it again.
        relayout(
            &mut panes,
            &tree,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 20,
            },
        );
        assert_ne!(
            panes.table.get(&id).unwrap().size(),
            real,
            "a real rect resizes the pane"
        );
    }

    #[test]
    fn toggle_drawer_spawns_and_closes_drawer_pane() {
        // The test spawns a drawer, which reads SHELL. Pin it to one that
        // exists, under the env guard so it's restored on drop.
        let _env = crate::testenv::EnvVarGuard::set(&[("SHELL", "/bin/sh")]);
        let mut session = one_tab_session();
        let chrome = layout::compute(160, 40, true, true);

        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        let path = session.worktrees[0].path.clone();
        let mut cfg = thegn_core::config::Config::default();
        cfg.sandbox.enabled = false;
        // Two-phase materialize: resolve a shell launch spec per missing leaf
        // (the loop does this off-thread), then spawn + remap on the tree.
        let specs: Vec<(u32, crate::agent::LaunchSpec)> = panes
            .missing_leaves(&session.worktrees[0].tabs[0])
            .into_iter()
            .map(|id| {
                (
                    id,
                    crate::agent::launch_spec(&cfg, &path, None, "shell").unwrap(),
                )
            })
            .collect();
        panes
            .materialize_with_specs(
                &cfg,
                &mut session.worktrees[0].tabs[0],
                &path,
                &specs,
                chrome.center,
            )
            .unwrap();

        let mut drawer: Option<u32> = None;
        let mut dirty = false;

        let simulate_toggle = |drawer: &mut Option<u32>, panes: &mut Panes, dirty: &mut bool| {
            if drawer.is_some() {
                if let Some(id) = drawer.take() {
                    panes.table.remove(&id);
                }
            } else {
                let p = panes
                    .spawn(&thegn_core::config::Config::default(), None, chrome.center)
                    .ok();
                if let Some(id) = p {
                    *drawer = Some(id);
                }
            }
            *dirty = true;
        };

        // Initially no drawer
        assert!(drawer.is_none());
        assert_eq!(panes.table.len(), 1); // just the materialized center pane

        // Toggle ON
        simulate_toggle(&mut drawer, &mut panes, &mut dirty);
        assert!(drawer.is_some());
        assert_eq!(panes.table.len(), 2);
        assert!(dirty);

        // Toggle OFF
        simulate_toggle(&mut drawer, &mut panes, &mut dirty);
        assert!(drawer.is_none());
        assert_eq!(panes.table.len(), 1);
    }

    #[test]
    fn shell_argv_defaults_to_interactive_non_login() {
        assert_eq!(
            shell_argv_from("/run/current-system/sw/bin/fish", false),
            vec![
                "/run/current-system/sw/bin/fish".to_string(),
                "-i".to_string()
            ]
        );
        assert_eq!(
            shell_argv_from("/bin/zsh", false),
            vec!["/bin/zsh".to_string(), "-i".to_string()]
        );
        assert_eq!(
            shell_argv_from("/opt/custom-shell", false),
            vec!["/opt/custom-shell".to_string()]
        );
    }

    #[test]
    fn external_sole_center_pane_exit_is_replaced_with_fresh_shell_pane() {
        let mut tab = crate::session::Tab::new("1");
        tab.center = crate::center::CenterTree::Leaf(7);
        tab.focused_pane = 7;

        assert!(replace_single_dead_center_pane(&mut tab, 7, 42));
        assert_eq!(tab.center.pane_ids(), vec![42]);
        assert_eq!(tab.focused_pane, 42);
    }

    #[test]
    fn pane_shell_resolution_falls_back_when_shell_env_points_to_missing_binary() {
        let shell = resolve_pane_shell(Some("/definitely/missing/thegn-shell".into()));

        assert_ne!(shell, "/definitely/missing/thegn-shell");
        assert!(
            std::path::Path::new(&shell).is_file(),
            "fallback shell should exist on disk: {shell}"
        );
    }

    #[test]
    fn shell_argv_honors_login_override() {
        assert_eq!(
            shell_argv_from("/bin/bash", true),
            vec!["/bin/bash".to_string(), "-l".to_string()]
        );
    }

    #[test]
    fn tool_drawer_argv_runs_configured_command_inside_shell() {
        let argv = tool_drawer_argv("${EDITOR:-vi} .");
        assert_eq!(argv[1], "-lc");
        assert_eq!(argv[2], "exec ${EDITOR:-vi} .");
    }

    #[test]
    fn spawn_records_time_and_forget_clears_it() {
        // Spawned panes read SHELL; pin it to one that exists, restored on drop.
        let _env = crate::testenv::EnvVarGuard::set(&[("SHELL", "/bin/sh")]);
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        let chrome = layout::compute(80, 24, false, false);

        let id = panes
            .spawn(&thegn_core::config::Config::default(), None, chrome.center)
            .expect("spawn");

        // pane_age returns Some(duration) right after spawn.
        let age = panes.pane_age(id);
        assert!(age.is_some(), "pane_age should be Some after spawn");
        assert!(
            age.unwrap() < std::time::Duration::from_secs(1),
            "age should be near-zero just after spawn"
        );

        // forget_spawn_time removes the entry.
        panes.forget_spawn_time(id);
        assert!(
            panes.pane_age(id).is_none(),
            "pane_age should be None after forget_spawn_time"
        );
    }

    #[test]
    fn pane_age_unknown_id_returns_none() {
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let panes = Panes::new(tx);
        assert!(panes.pane_age(999).is_none());
    }

    #[test]
    fn ephemeral_local_spawn_bypasses_daemon_route() {
        // Pins/drawer/corner spawn through `spawn_argv_env_local`: even with
        // `[daemon]` enabled (the default), they must stay in-process PTYs —
        // a daemon session backing chrome would outlive the UI as an orphan
        // lease nobody reattaches.
        let _env = crate::testenv::EnvVarGuard::set(&[("SHELL", "/bin/sh")]);
        let (tx, _rx) = tokio_mpsc::channel::<PaneEvent>(1024);
        let mut panes = Panes::new(tx);
        panes.set_daemon_config(thegn_core::config::DaemonConfig {
            enabled: true,
            socket: "/nonexistent/never.sock".into(),
            ..Default::default()
        });
        let chrome = layout::compute(80, 24, false, false);
        let id = panes
            .spawn_argv_env_local(
                &["/bin/sh".into(), "-c".into(), "true".into()],
                None,
                &[],
                chrome.center,
            )
            .expect("local spawn");
        assert!(
            !panes.table.get(&id).unwrap().is_daemon_backed(),
            "ephemeral panes must stay in-process with the daemon enabled"
        );
    }
}
