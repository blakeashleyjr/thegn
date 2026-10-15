//! Display snapshots for the System ▸ Environments panel section — one row per
//! configured `[env.<name>]` with its placement kind, region/size, and (for
//! managed-provider envs) whether a token resolves. Pure over the config +
//! secret backend; built off-loop by hydration into `model.panel.environments`
//! (mirroring `host_ui::host_snapshots` → `model.panel.hosts`), so the render
//! path just reads the precomputed list.

use thegn_core::config::{Config, EnvProviderConfig, PlacementMode};

/// One environment as shown in the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSnapshot {
    pub name: String,
    /// The provider name for a `provider` placement (`fly`/`hetzner`/…), else the
    /// placement kind (`local`/`ssh`/`k8s`).
    pub kind: String,
    /// The raw placement kind (`local`/`ssh`/`k8s`/`provider`).
    pub placement: String,
    pub region: String,
    pub size: String,
    /// Whether this is the worktree's effective environment (including inherited defaults).
    pub bound: bool,
    /// `Some(true/false)` whether a token resolves for a provider env; `None` for
    /// non-provider placements (which need no token).
    pub token: Option<bool>,
    /// Sanitized-at-render config details. Never contains secret values or identity paths.
    pub detail: Vec<(String, String)>,
}

/// Build the display list for every configured env (sorted by name). Cheap: a
/// map walk + one secret probe per provider env.
pub fn env_snapshots(cfg: &Config, bound: Option<&str>) -> Vec<EnvSnapshot> {
    let mut v: Vec<EnvSnapshot> = cfg
        .env
        .iter()
        .map(|(name, e)| {
            let placement = e.placement.as_str().to_string();
            let is_provider = e.placement == PlacementMode::Provider;
            let kind = if is_provider {
                let p = e.provider.provider.trim();
                if p.is_empty() {
                    "provider".to_string()
                } else {
                    p.to_string()
                }
            } else {
                placement.clone()
            };
            let token = is_provider.then(|| {
                let key = effective_token_env(&e.provider);
                // Presence-memoized: this runs per provider env per hydration.
                !key.is_empty() && crate::secret::resolve_present_cached(&key)
            });
            let mut detail = Vec::new();
            let mode = e
                .placement_mode
                .map(|mode| format!(" ({mode})"))
                .unwrap_or_else(String::new);
            push_detail(
                &mut detail,
                "placement",
                format!("{}{mode}", e.placement.as_str()),
            );
            push_detail(&mut detail, "host", e.host.clone());
            let target = match e.placement {
                PlacementMode::Ssh => {
                    let mut target = e.ssh.host.clone();
                    if !target.is_empty() && e.ssh.port != 22 {
                        target.push_str(&format!(":{}", e.ssh.port));
                    }
                    if !e.ssh.jump_host.is_empty() {
                        target.push_str(if target.is_empty() { "" } else { " via " });
                        target.push_str(&e.ssh.jump_host);
                    }
                    target
                }
                PlacementMode::K8s => [
                    e.k8s.context.as_str(),
                    e.k8s.namespace.as_str(),
                    e.k8s.pod.as_str(),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("/"),
                PlacementMode::Provider => {
                    let mut target = [
                        e.provider.provider.as_str(),
                        e.provider.region.as_str(),
                        e.provider.size.as_str(),
                    ]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                    if !e.provider.template.is_empty() {
                        target.push_str(if target.is_empty() { "" } else { " " });
                        target.push_str("template ");
                        target.push_str(&e.provider.template);
                    }
                    target
                }
                PlacementMode::Local => String::new(),
            };
            push_detail(&mut detail, "target", target);
            push_detail(&mut detail, "data", e.data.as_str().to_string());
            let overlay = [
                e.sandbox.backend.map(|value| value.as_str().to_string()),
                e.sandbox.image.clone(),
                e.sandbox.profile.map(|value| value.as_str().to_string()),
            ]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
            push_detail(&mut detail, "sandbox", overlay);
            push_detail(
                &mut detail,
                "failover",
                e.failover
                    .map(|value| value.as_str().to_string())
                    .unwrap_or_else(|| "inherit".to_string()),
            );
            let key = effective_token_env(&e.provider);
            if is_provider && !key.is_empty() {
                push_detail(
                    &mut detail,
                    "token",
                    format!(
                        "{key} {}",
                        if token == Some(true) {
                            "present"
                        } else {
                            "missing"
                        }
                    ),
                );
            }
            if bound == Some(name.as_str()) {
                push_detail(&mut detail, "bound", "this worktree".to_string());
            }
            EnvSnapshot {
                name: name.clone(),
                kind,
                placement,
                region: e.provider.region.clone(),
                size: e.provider.size.clone(),
                bound: bound == Some(name.as_str()),
                token,
                detail,
            }
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn push_detail(detail: &mut Vec<(String, String)>, label: &str, value: String) {
    if !value.is_empty() {
        detail.push((label.to_string(), value));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindDecision {
    NoWorktree,
    AlreadyBound,
    Bind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindAction {
    Confirm {
        name: String,
        from: String,
        to: String,
    },
    Bind {
        name: String,
        worktree: String,
    },
    Status(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfirmBindDecision {
    Bind { worktree: String, name: String },
    Status(String),
}

pub(crate) fn placement_change(
    envs: &[EnvSnapshot],
    target: &EnvSnapshot,
) -> Option<(String, String)> {
    let from = envs
        .iter()
        .find(|env| env.bound)
        .map_or("local", |env| env.placement.as_str());
    (from != target.placement).then(|| (from.to_string(), target.placement.clone()))
}

pub(crate) fn bind_action(envs: &[EnvSnapshot], cursor: usize, worktree: &str) -> BindAction {
    let Some(env) = envs.get(cursor) else {
        return BindAction::Status("no environment selected".to_string());
    };
    match bind_decision(env, worktree) {
        BindDecision::NoWorktree => {
            BindAction::Status("no active worktree to bind (open one first)".to_string())
        }
        BindDecision::AlreadyBound => BindAction::Status(already_bound_status(env)),
        BindDecision::Bind => match placement_change(envs, env) {
            Some((from, to)) => BindAction::Confirm {
                name: env.name.clone(),
                from,
                to,
            },
            None => BindAction::Bind {
                name: env.name.clone(),
                worktree: worktree.to_string(),
            },
        },
    }
}

pub(crate) fn bind_decision(env: &EnvSnapshot, worktree: &str) -> BindDecision {
    if worktree.trim().is_empty() {
        BindDecision::NoWorktree
    } else if env.bound {
        BindDecision::AlreadyBound
    } else {
        BindDecision::Bind
    }
}

fn already_bound_status(env: &EnvSnapshot) -> String {
    format!(
        "env '{}' is already this worktree's effective env",
        env.name
    )
}

pub(crate) fn spawn_bind(
    worktree: String,
    name: String,
    refresh_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: termwiz::terminal::TerminalWaker,
) {
    let toast = refresh_tx.clone();
    let refresh = refresh_tx;
    drop(tokio::task::spawn_blocking(move || {
        use thegn_core::notification::Priority;
        let (message, priority) = match thegn_core::db::Db::open() {
            Ok(db) => bind_env_blocking(&db, &worktree, &name),
            Err(error) => (format!("bind failed: {error}"), Priority::Alert),
        };
        if let Err(error) = toast.send(crate::hydrate::RefreshKind::Toast { message, priority }) {
            tracing::debug!(target: "thegn::env", %error, "bind toast consumer is gone");
        }
        if let Err(error) = refresh.send(crate::hydrate::RefreshKind::Model) {
            tracing::debug!(target: "thegn::env", %error, "bind model refresh consumer is gone");
        }
        if let Err(error) = waker.wake() {
            tracing::debug!(target: "thegn::env", %error, "bind wake failed");
        }
    }));
}

pub(crate) fn confirm_bind(
    arg: &str,
    envs: &[EnvSnapshot],
    active_worktree: &str,
    refresh_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: termwiz::terminal::TerminalWaker,
) -> String {
    match confirm_bind_decision(arg, envs, active_worktree) {
        ConfirmBindDecision::Status(status) => status,
        ConfirmBindDecision::Bind { worktree, name } => {
            spawn_bind(worktree, name.clone(), refresh_tx, waker);
            format!("binding env '{name}'…")
        }
    }
}

fn confirm_bind_decision(
    arg: &str,
    envs: &[EnvSnapshot],
    active_worktree: &str,
) -> ConfirmBindDecision {
    let (worktree, name) = match arg.split_once('\u{1f}') {
        Some((worktree, name))
            if !worktree.is_empty() && !name.is_empty() && !name.contains('\u{1f}') =>
        {
            (worktree, name)
        }
        _ => {
            return ConfirmBindDecision::Status(
                "cannot bind env: invalid confirmation".to_string(),
            );
        }
    };
    if worktree != active_worktree {
        return ConfirmBindDecision::Status("cannot bind env: active worktree changed".to_string());
    }
    let Some(env) = envs.iter().find(|env| env.name == name) else {
        return ConfirmBindDecision::Status(format!(
            "cannot bind env '{name}': it is no longer configured"
        ));
    };
    if env.bound {
        return ConfirmBindDecision::Status(already_bound_status(env));
    }
    ConfirmBindDecision::Bind {
        worktree: worktree.to_string(),
        name: name.to_string(),
    }
}

pub(crate) fn bind_env_blocking(
    db: &thegn_core::db::Db,
    worktree: &str,
    env: &str,
) -> (String, thegn_core::notification::Priority) {
    use thegn_core::notification::Priority;
    use thegn_core::store::WorkspaceStore;

    match db.set_worktree_env(worktree, env) {
        Ok(()) => (
            format!("bound env '{env}' to this worktree"),
            Priority::Info,
        ),
        Err(error) => (format!("bind failed: {error}"), Priority::Alert),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TestDecision {
    NotProvider(String),
    Provider,
}

pub(crate) fn test_decision(env: &EnvSnapshot) -> TestDecision {
    if env.placement == "provider" {
        TestDecision::Provider
    } else {
        TestDecision::NotProvider(format!(
            "env '{}' is a {} env — `t` tests provider tokens only",
            env.name, env.placement
        ))
    }
}

/// The SecretRef (or bare env-var name) a provider env's token resolves from:
/// the explicit `api_key_env`, else the provider's built-in default. The single
/// source of truth for the per-provider default token var (also consulted by
/// `env_halt_reason` so its halt modal names the RIGHT var — a machine0 env must
/// report `MACHINE0_API_KEY`, not the old hardcoded `SPRITES_TOKEN`).
pub(crate) fn effective_token_env(pc: &EnvProviderConfig) -> String {
    let k = pc.api_key_env.trim();
    if !k.is_empty() {
        return k.to_string();
    }
    match pc.provider.trim() {
        "sprites" => "SPRITES_TOKEN".to_string(),
        "fly" => "FLY_API_TOKEN".to_string(),
        "machine0" => "MACHINE0_API_KEY".to_string(),
        p => thegn_svc::vps::VpsKind::parse(p)
            .map(|k| k.token_env_default().to_string())
            .unwrap_or_default(),
    }
}

/// The CONFIRMED env removal (the `env-remove` menu tag): delete the
/// `[env.<name>]` table from config.toml and forget its stored token
/// (keyring + secrets file). Returns the status line.
pub(crate) fn remove_env_confirmed(
    name: &str,
    refresh_tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
) -> String {
    let path = Config::path();
    if let Err(e) = thegn_core::config_write::remove_env(&path, name) {
        return format!("remove failed: {e}");
    }
    crate::secret::forget(name);
    let _ = refresh_tx.send(crate::hydrate::RefreshKind::Model); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
    format!("removed env '{name}' (token forgotten)")
}

/// Handle an action key on the `Environments` panel row at `cursor`:
/// - **Enter** — bind this env to the active worktree off-loop.
/// - **x** — CONFIRM, then remove the `[env.<name>]` from config + forget its
///   stored token. The confirm is non-negotiable: this deletes the provider
///   credential from the OS keyring, which one stray keystroke must not do.
/// - **t** — test provider envs off-loop (a `list()` call); the outcome
///   pops as a toast where the user is looking (and is logged to System ▸
///   Logs; blocking the render loop is not acceptable).
///
/// Returns `true` when the key was consumed. `n` (add) is handled by the loop
/// (it owns the wizard modal).
#[allow(clippy::too_many_arguments)]
pub fn panel_key(
    key: &termwiz::input::KeyCode,
    cursor: usize,
    model: &mut crate::chrome::FrameModel,
    cfg: &Config,
    worktree: &str,
    refresh_tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
    wizard: &mut Option<crate::env_wizard::EnvWizard>,
    active_menu: &mut Option<crate::menu::MenuOverlay>,
) -> bool {
    use termwiz::input::KeyCode;
    // `n` opens the Add-environment wizard (no cursor row needed).
    if matches!(key, KeyCode::Char('n')) {
        *wizard = Some(crate::env_wizard::EnvWizard::new(cfg));
        return true;
    }
    let Some(env) = model.panel.environments.get(cursor).cloned() else {
        return false;
    };
    match key {
        KeyCode::Enter => {
            match bind_action(&model.panel.environments, cursor, worktree) {
                BindAction::Status(status) => model.status = status,
                BindAction::Bind { name, worktree } => {
                    model.status = format!("binding env '{name}'…");
                    spawn_bind(worktree, name, refresh_tx.clone(), waker.clone());
                }
                BindAction::Confirm { name, from, to } => {
                    *active_menu = Some(crate::menu::confirm_menu(
                        format!("bind env '{name}'?"),
                        format!(
                            "Moves this worktree's new panes from {from} to {to} placement. Running panes stay where they are."
                        ),
                        "env-bind",
                        format!("{worktree}\u{1f}{name}"),
                        false,
                    ));
                }
            };
            true
        }
        KeyCode::Char('x') => {
            *active_menu = Some(crate::menu::confirm_menu(
                format!("remove env '{}'?", env.name),
                "Deletes the [env.*] table from config.toml AND its stored \
                 provider token (keyring + secrets file). Worktrees bound to \
                 this env fall back per their failover setting."
                    .to_string(),
                "env-remove",
                env.name.clone(),
                true,
            ));
            true
        }
        KeyCode::Char('t') => {
            if let TestDecision::NotProvider(message) = test_decision(&env) {
                model.status = message;
                return true;
            }
            model.status = format!("testing env '{}'…", env.name);
            let pc = cfg.env.get(&env.name).map(|e| e.provider.clone());
            let name = env.name.clone();
            let toast = refresh_tx.clone();
            let wk = waker.clone();
            // Off-loop: a provider `list()` can take seconds; never block the frame.
            std::thread::spawn(move || {
                let Some(pc) = pc else { return };
                use thegn_core::notification::Priority;
                // The outcome pops as a toast where the user is looking; the
                // log line stays for history (System ▸ Logs).
                let (message, priority) =
                    match crate::provider_factory::provider_for_named(&pc, &name) {
                        Some(p) => {
                            match crate::agent::block_on_provider(|| async { p.list().await }) {
                                Ok(list) => {
                                    let m = format!(
                                        "env {name}: reachable — {} managed sandbox(es)",
                                        list.len()
                                    );
                                    thegn_core::msg::info(&m);
                                    (m, Priority::Info)
                                }
                                Err(e) => {
                                    let m = format!("env {name}: test failed: {e}");
                                    thegn_core::msg::warn(&m);
                                    (m, Priority::Alert)
                                }
                            }
                        }
                        None => {
                            let m = format!("env {name}: no API provider (or token unresolved)");
                            thegn_core::msg::warn(&m);
                            (m, Priority::Alert)
                        }
                    };
                let _ = toast.send(crate::hydrate::RefreshKind::Toast { message, priority }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                let _ = wk.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
            });
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_env(
        provider: &str,
        api_key_env: &str,
        region: &str,
    ) -> thegn_core::config::EnvConfig {
        thegn_core::config::EnvConfig {
            placement: PlacementMode::Provider,
            provider: EnvProviderConfig {
                provider: provider.into(),
                api_key_env: api_key_env.into(),
                region: region.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn snapshot(name: &str, placement: &str, bound: bool) -> EnvSnapshot {
        EnvSnapshot {
            name: name.to_string(),
            kind: placement.to_string(),
            placement: placement.to_string(),
            region: String::new(),
            size: String::new(),
            bound,
            token: None,
            detail: Vec::new(),
        }
    }

    #[test]
    fn placement_change_detects_local_to_provider_and_ignores_same_placement() {
        let provider = snapshot("provider", "provider", false);
        assert_eq!(
            placement_change(&[], &provider),
            Some(("local".into(), "provider".into()))
        );
        let envs = [
            snapshot("bound", "local", true),
            snapshot("local", "local", false),
        ];
        assert_eq!(placement_change(&envs, &envs[1]), None);
    }

    #[test]
    fn enter_on_placement_changing_env_opens_bind_confirm() {
        let envs = [
            snapshot("a", "local", true),
            snapshot("b", "provider", false),
        ];
        assert_eq!(
            bind_action(&envs, 1, "/worktree"),
            BindAction::Confirm {
                name: "b".into(),
                from: "local".into(),
                to: "provider".into(),
            }
        );
    }

    #[test]
    fn confirm_bind_refuses_a_vanished_env_or_switched_worktree() {
        let envs = [snapshot("a", "local", false)];
        assert!(matches!(
            confirm_bind_decision("/worktree\u{1f}gone", &envs, "/worktree"),
            ConfirmBindDecision::Status(message) if message.contains("no longer configured")
        ));
        assert!(matches!(
            confirm_bind_decision("/old\u{1f}a", &envs, "/new"),
            ConfirmBindDecision::Status(message) if message.contains("worktree changed")
        ));
    }

    #[test]
    fn snapshots_sorted_and_classify_kind_and_token() {
        let mut cfg = Config::default();
        // A provider env with a token env var set (present).
        // SAFETY: single-threaded test, unique name.
        unsafe { std::env::set_var("TG_ENVUI_TOK", "x") };
        cfg.env
            .insert("z-fly".into(), provider_env("fly", "TG_ENVUI_TOK", "iad"));
        // A local env (no token concept).
        cfg.env
            .insert("a-local".into(), thegn_core::config::EnvConfig::default());

        let snaps = env_snapshots(&cfg, None);
        // Sorted by name.
        assert_eq!(snaps[0].name, "a-local");
        assert_eq!(snaps[0].placement, "local");
        assert_eq!(snaps[0].token, None, "local needs no token");
        assert_eq!(snaps[1].name, "z-fly");
        assert_eq!(snaps[1].kind, "fly");
        assert_eq!(snaps[1].region, "iad");
        assert_eq!(snaps[1].token, Some(true), "token env var is set");
        unsafe { std::env::remove_var("TG_ENVUI_TOK") };
    }

    #[test]
    fn provider_env_without_token_reports_false() {
        let mut cfg = Config::default();
        // api_key_env points at an unset var → token missing.
        cfg.env.insert(
            "h".into(),
            provider_env("hetzner", "TG_ENVUI_DEFINITELY_UNSET", ""),
        );
        assert_eq!(env_snapshots(&cfg, None)[0].token, Some(false));
    }

    #[test]
    fn snapshots_mark_the_bound_env() {
        let mut cfg = Config::default();
        cfg.env.insert(
            "a".into(),
            thegn_core::config::EnvConfig {
                placement: PlacementMode::Local,
                ..Default::default()
            },
        );
        cfg.env.insert("b".into(), provider_env("fly", "", ""));

        let snaps = env_snapshots(&cfg, Some("b"));
        assert!(!snaps[0].bound);
        assert!(snaps[1].bound);
    }

    #[test]
    fn env_snapshot_detail_names_target_and_token_env_never_the_value() {
        let mut cfg = Config::default();
        cfg.env.insert(
            "ssh".into(),
            thegn_core::config::EnvConfig {
                placement: PlacementMode::Ssh,
                ssh: thegn_core::config::EnvSshConfig {
                    host: "box".into(),
                    port: 2222,
                    jump_host: "bastion".into(),
                    identity: "/home/u/.ssh/id_x".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        cfg.env.insert(
            "provider".into(),
            provider_env("fly", "THEGN_ENV_DETAIL_TEST_TOKEN_810", "west"),
        );
        cfg.env.insert("local".into(), Default::default());

        let snapshots = env_snapshots(&cfg, None);
        let ssh = snapshots.iter().find(|env| env.name == "ssh").unwrap();
        assert!(
            ssh.detail
                .iter()
                .any(|row| row == &("target".into(), "box:2222 via bastion".into()))
        );
        let provider = snapshots.iter().find(|env| env.name == "provider").unwrap();
        let token = provider
            .detail
            .iter()
            .find(|(label, _)| label == "token")
            .unwrap();
        assert!(matches!(
            token.1.as_str(),
            "THEGN_ENV_DETAIL_TEST_TOKEN_810 missing" | "THEGN_ENV_DETAIL_TEST_TOKEN_810 present"
        ));
        assert!(
            !ssh.detail
                .iter()
                .any(|(_, value)| value.contains("/home/u/.ssh/id_x"))
        );
        let local = snapshots.iter().find(|env| env.name == "local").unwrap();
        assert!(!local.detail.iter().any(|(label, _)| label == "target"));
    }

    #[test]
    fn bind_decision_skips_the_already_bound_env() {
        let env = EnvSnapshot {
            name: "dev".into(),
            kind: "local".into(),
            placement: "local".into(),
            region: String::new(),
            size: String::new(),
            bound: true,
            token: None,
            detail: Vec::new(),
        };
        assert_eq!(bind_decision(&env, "/wt"), BindDecision::AlreadyBound);
        assert_eq!(bind_decision(&env, ""), BindDecision::NoWorktree);
    }

    #[test]
    fn already_bound_status_describes_the_effective_env() {
        let env = EnvSnapshot {
            name: "dev".into(),
            kind: "local".into(),
            placement: "local".into(),
            region: String::new(),
            size: String::new(),
            bound: true,
            token: None,
            detail: Vec::new(),
        };

        assert_eq!(
            already_bound_status(&env),
            "env 'dev' is already this worktree's effective env"
        );
    }

    #[test]
    fn bind_env_blocking_writes_the_pin() {
        use thegn_core::store::WorkspaceStore;
        let db = thegn_core::db::Db::open_memory().unwrap();

        let (status, priority) = bind_env_blocking(&db, "/wt/x", "dev");
        assert!(status.contains("bound env 'dev'"));
        assert_eq!(priority, thegn_core::notification::Priority::Info);
        assert_eq!(db.worktree_env("/wt/x").unwrap().as_deref(), Some("dev"));
    }

    #[test]
    fn test_decision_only_offers_provider_tests() {
        let mut cfg = Config::default();
        cfg.env.insert(
            "local".into(),
            thegn_core::config::EnvConfig {
                placement: PlacementMode::Local,
                ..Default::default()
            },
        );
        cfg.env
            .insert("provider".into(), provider_env("fly", "", ""));
        let snaps = env_snapshots(&cfg, None);
        let local = snaps.iter().find(|env| env.name == "local").unwrap();
        let provider = snaps.iter().find(|env| env.name == "provider").unwrap();

        assert!(matches!(
            test_decision(local),
            TestDecision::NotProvider(message) if message.contains("provider tokens only")
        ));
        assert_eq!(test_decision(provider), TestDecision::Provider);
    }
}
