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
            EnvSnapshot {
                name: name.clone(),
                kind,
                placement,
                region: e.provider.region.clone(),
                size: e.provider.size.clone(),
                bound: bound == Some(name.as_str()),
                token,
            }
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BindDecision {
    NoWorktree,
    AlreadyBound,
    Bind,
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
            match bind_decision(&env, worktree) {
                BindDecision::NoWorktree => {
                    model.status = "no active worktree to bind (open one first)".into();
                }
                BindDecision::AlreadyBound => {
                    model.status = format!("env '{}' is already bound to this worktree", env.name);
                }
                BindDecision::Bind => {
                    model.status = format!("binding env '{}'…", env.name);
                    let wt = worktree.to_string();
                    let name = env.name.clone();
                    let toast = refresh_tx.clone();
                    let refresh = refresh_tx.clone();
                    let wk = waker.clone();
                    drop(tokio::task::spawn_blocking(move || {
                        use thegn_core::notification::Priority;
                        let (message, priority) = match thegn_core::db::Db::open() {
                            Ok(db) => bind_env_blocking(&db, &wt, &name),
                            Err(error) => (format!("bind failed: {error}"), Priority::Alert),
                        };
                        if let Err(error) =
                            toast.send(crate::hydrate::RefreshKind::Toast { message, priority })
                        {
                            tracing::debug!(target: "thegn::env", %error, "bind toast consumer is gone");
                        }
                        if let Err(error) = refresh.send(crate::hydrate::RefreshKind::Model) {
                            tracing::debug!(target: "thegn::env", %error, "bind model refresh consumer is gone");
                        }
                        if let Err(error) = wk.wake() {
                            tracing::debug!(target: "thegn::env", %error, "bind wake failed");
                        }
                    }));
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
    fn bind_decision_skips_the_already_bound_env() {
        let env = EnvSnapshot {
            name: "dev".into(),
            kind: "local".into(),
            placement: "local".into(),
            region: String::new(),
            size: String::new(),
            bound: true,
            token: None,
        };
        assert_eq!(bind_decision(&env, "/wt"), BindDecision::AlreadyBound);
        assert_eq!(bind_decision(&env, ""), BindDecision::NoWorktree);
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
