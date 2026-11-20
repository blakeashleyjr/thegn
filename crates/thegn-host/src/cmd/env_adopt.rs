//! `thegn env adopt <sprite> --env <S> [--yes] [--json]`: record an existing
//! sprite as this host's, so thegn may later destroy it (env down, env reap,
//! land --release-sandbox, worktree delete and the opt-in lifetime reaper). It
//! asks for confirmation and never touches the sprite itself: one org-API read
//! (`GET /v1/sprites/{name}`, which never wakes it) and one ledger write.
//!
//! Adoption is the only way a sprite not created by this host becomes provable
//! (see [`thegn_svc::sprites::ledger::adopt`]), so the user must type the
//! sprite's name back on a terminal, or pass `--yes`.

use anyhow::{Context, Result, bail};
use thegn_core::config::{Config, EnvProviderConfig};
use thegn_core::outln;
use thegn_svc::provider::Provider;
use thegn_svc::sprites::inspect::SpriteInfo;
use thegn_svc::sprites::ledger::{self, AdoptOutcome, OwnerScope};

const KIND: &str = "sprites";

/// What adopting gives thegn, shown before the confirmation.
pub(crate) const CONSEQUENCES: &str = "thegn will be able to destroy it on env down, env reap, land --release-sandbox, worktree delete and the opt-in reaper; it counts toward max_instances";

/// `thegn env adopt` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct AdoptArgs {
    /// The sprite's name as the Sprites API lists it.
    pub name: String,
    /// The `[env.<name>]` (a sprites env) whose account owns the sprite.
    #[arg(long, value_name = "ENV")]
    pub env: String,
    /// Skip the typed confirmation (required when stdin is not a tty).
    #[arg(long)]
    pub yes: bool,
    /// Emit one JSON object instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

/// The provider calls adopt makes. Production wraps a scoped
/// [`thegn_svc::provider::SpritesProvider`]; tests use a fake.
pub(crate) trait AdoptApi {
    /// The ownership scope (`None` = no controller id or account fingerprint).
    fn scope(&self) -> Option<OwnerScope>;
    /// `GET /v1/sprites/{name}` (`None` = 404).
    fn inspect(&self, name: &str) -> Result<Option<SpriteInfo>>;
}

struct LiveAdopt(thegn_svc::provider::SpritesProvider);

impl AdoptApi for LiveAdopt {
    fn scope(&self) -> Option<OwnerScope> {
        self.0.scope().cloned()
    }

    fn inspect(&self, name: &str) -> Result<Option<SpriteInfo>> {
        crate::agent::block_on_provider(|| async { self.0.inspect(name).await })
    }
}

/// The typed confirmation prompt; `None` when stdin is not a terminal.
type Ask<'a> = &'a mut dyn FnMut(&str) -> Option<String>;

/// The `--json` shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct AdoptReport {
    pub name: String,
    pub status: String,
    pub created_at: Option<String>,
    /// `adopted` or `already_ours`.
    pub outcome: &'static str,
}

pub fn run(cfg: &Config, args: AdoptArgs) -> Result<()> {
    let pc = sprites_env(cfg, &args.env)?;
    let api = match crate::provider_factory::provider_for_inventory(pc) {
        Ok(Provider::Sprites(provider)) => LiveAdopt(provider),
        Ok(_) => bail!("env adopt: [env.{}] is not a sprites env", args.env),
        Err(reason) => bail!("env adopt: [env.{}]: {reason}", args.env),
    };
    let json = args.json;
    // The block goes to stdout as the human result; with --json it goes to
    // stderr so a tty user still sees what they are confirming.
    let mut show = |text: &str| {
        if json {
            show_stderr(text);
        } else {
            outln!("{text}");
        }
    };
    let mut ask = |prompt: &str| tty_prompt(prompt);
    let report = adopt(
        &api,
        &args.env,
        pc.max_instances,
        &args,
        &mut show,
        &mut ask,
    )?;
    if json {
        crate::cmd::emit_json(&report)?;
    } else if report.outcome == "already_ours" {
        outln!(
            "{} is already recorded as thegn's; nothing written",
            cell(&report.name)
        );
    } else {
        outln!("adopted {}", cell(&report.name));
    }
    Ok(())
}

/// `env`'s provider table when it is a sprites env, else an error listing the
/// sprites envs.
pub(crate) fn sprites_env<'a>(cfg: &'a Config, env: &str) -> Result<&'a EnvProviderConfig> {
    if let Some(envc) = cfg.env.get(env)
        && envc.provider.provider.trim() == KIND
    {
        return Ok(&envc.provider);
    }
    let sprites: Vec<&str> = cfg
        .env
        .iter()
        .filter(|(_, envc)| envc.provider.provider.trim() == KIND)
        .map(|(name, _)| name.as_str())
        .collect();
    let listed = if sprites.is_empty() {
        "none is configured".to_string()
    } else {
        sprites.join(", ")
    };
    bail!(
        "env adopt: --env {} is not a sprites env (sprites envs: {listed})",
        cell(env)
    )
}

/// The adopt pass (testable core: no stdout, no tty, no config). Writes the
/// ledger record only after the confirmation; every refusal writes nothing.
pub(crate) fn adopt(
    api: &dyn AdoptApi,
    env: &str,
    max_instances: u32,
    args: &AdoptArgs,
    show: &mut dyn FnMut(&str),
    ask: Ask<'_>,
) -> Result<AdoptReport> {
    let name = args.name.as_str();
    thegn_svc::vps::registry::checked_name(name)
        .with_context(|| format!("env adopt: {} is not a valid sprite name", cell(name)))?;
    let Some(scope) = api.scope() else {
        bail!(
            "env adopt: [env.{}] has no ownership scope (controller id or account fingerprint unavailable); refusing to adopt",
            cell(env)
        );
    };
    let Some(info) = api.inspect(name)? else {
        bail!("no sprite named {name} in this Sprites account");
    };
    let already = ledger::prove_owned(&scope, name, &[]).is_ok();
    let recorded = ledger::owned(&scope)
        .map(|view| view.recorded.len().to_string())
        .unwrap_or_else(|_| "?".into());
    let cap = if max_instances == 0 {
        "no cap".to_string()
    } else {
        max_instances.to_string()
    };
    let block = [
        format!("sprite:       {}", cell(&info.name)),
        format!("status:       {}", cell(&info.status)),
        format!(
            "created_at:   {}",
            info.created_at.as_deref().map_or("-".into(), cell)
        ),
        format!("account:      {}", cell(&scope.account)),
        format!("controller:   {}", cell(&scope.owner)),
        format!("max_instances: {recorded}/{cap}"),
        CONSEQUENCES.to_string(),
    ]
    .join("\n");
    show(&block);
    if !already && !args.yes {
        let Some(typed) = ask("Type the sprite name to adopt it:") else {
            bail!(
                "env adopt: stdin is not a terminal; pass --yes to adopt {name} without the typed confirmation"
            );
        };
        if typed.trim_end_matches(['\n', '\r']) != name {
            bail!("env adopt: the typed name does not match {name}; nothing adopted");
        }
    }
    let cap = usize::try_from(max_instances).unwrap_or(usize::MAX);
    let outcome = match ledger::adopt(&scope, name, cap)? {
        AdoptOutcome::Adopted => "adopted",
        AdoptOutcome::AlreadyOurs => "already_ours",
    };
    Ok(AdoptReport {
        name: info.name,
        status: info.status,
        created_at: info.created_at,
        outcome,
    })
}

/// A terminal-safe cell: control characters (API text is untrusted) become
/// `?`, capped at 64 characters.
fn cell(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(64)
        .collect()
}

#[allow(clippy::disallowed_macros)] // the confirmation block, beside the prompt
fn show_stderr(text: &str) {
    eprintln!("{text}");
}

/// The typed confirmation; `None` when stdin is not a terminal (piped or absent).
#[allow(clippy::disallowed_macros)] // a raw interactive prompt, not a log line
fn tty_prompt(prompt: &str) -> Option<String> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return None;
    }
    eprint!("{prompt} ");
    std::io::stderr().flush().ok()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_svc::vps::registry;

    struct Fake {
        scope: Option<OwnerScope>,
        sprite: Option<SpriteInfo>,
    }

    impl AdoptApi for Fake {
        fn scope(&self) -> Option<OwnerScope> {
            self.scope.clone()
        }

        fn inspect(&self, name: &str) -> Result<Option<SpriteInfo>> {
            Ok(self.sprite.clone().filter(|s| s.name == name))
        }
    }

    fn scope() -> OwnerScope {
        OwnerScope {
            owner: "ctl-adopt".into(),
            account: "acct1-adopt".into(),
        }
    }

    fn fake() -> Fake {
        Fake {
            scope: Some(scope()),
            sprite: Some(SpriteInfo {
                name: "sx-dash".into(),
                status: "cold\x1b[2J".into(),
                created_at: Some("2026-11-18T10:00:00Z".into()),
                url_auth: None,
            }),
        }
    }

    fn args(yes: bool) -> AdoptArgs {
        AdoptArgs {
            name: "sx-dash".into(),
            env: "s".into(),
            yes,
            json: false,
        }
    }

    fn run_with(
        api: &Fake,
        args: &AdoptArgs,
        cap: u32,
        answer: Option<&str>,
    ) -> (Result<AdoptReport>, Vec<String>, usize) {
        let mut shown = Vec::new();
        let mut asked = 0;
        let result = {
            let mut show = |text: &str| shown.push(text.to_string());
            let mut ask = |_: &str| {
                asked += 1;
                answer.map(str::to_string)
            };
            adopt(api, "s", cap, args, &mut show, &mut ask)
        };
        (result, shown, asked)
    }

    fn records() -> usize {
        registry::list_checked_at(&registry::dir())
            .map(|view| view.records.len())
            .unwrap_or(0)
    }

    #[test]
    fn adopt_non_tty_without_yes_refuses_with_zero_writes() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (result, shown, asked) = run_with(&fake(), &args(false), 0, None);
        let text = format!("{:#}", result.unwrap_err());
        assert!(text.contains("--yes"), "{text}");
        assert_eq!(asked, 1);
        assert_eq!(records(), 0, "a refusal writes nothing");
        assert!(shown[0].contains(CONSEQUENCES), "{shown:?}");
        assert!(
            !shown[0].contains('\x1b'),
            "API text is filtered: {shown:?}"
        );
    }

    #[test]
    fn adopt_wrong_typed_name_refuses() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (result, _, _) = run_with(&fake(), &args(false), 0, Some("sx-das\n"));
        assert!(format!("{:#}", result.unwrap_err()).contains("does not match"));
        assert_eq!(records(), 0);
    }

    #[test]
    fn adopt_typed_name_writes_a_ready_record() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (result, shown, _) = run_with(&fake(), &args(false), 2, Some("sx-dash\n"));
        let report = result.unwrap();
        assert_eq!(report.outcome, "adopted");
        assert!(shown[0].contains("max_instances: 0/2"), "{shown:?}");
        let rec = registry::read_checked_at(&registry::dir(), "sx-dash")
            .unwrap()
            .unwrap();
        assert_eq!(rec.state, "ready");
        assert!(ledger::prove_owned(&scope(), "sx-dash", &[]).is_ok());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["outcome"], "adopted");
        assert_eq!(json["created_at"], "2026-11-18T10:00:00Z");
    }

    #[test]
    fn adopt_yes_skips_the_prompt_and_a_second_adopt_is_a_no_op() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let (result, _, asked) = run_with(&fake(), &args(true), 0, None);
        assert_eq!(result.unwrap().outcome, "adopted");
        assert_eq!(asked, 0);
        let (result, _, asked) = run_with(&fake(), &args(false), 0, None);
        assert_eq!(result.unwrap().outcome, "already_ours");
        assert_eq!(asked, 0, "already ours needs no confirmation");
    }

    #[test]
    fn adopt_refuses_a_missing_sprite_and_no_scope() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let mut gone = fake();
        gone.sprite = None;
        let (result, _, _) = run_with(&gone, &args(true), 0, None);
        assert_eq!(
            format!("{:#}", result.unwrap_err()),
            "no sprite named sx-dash in this Sprites account"
        );
        let mut unscoped = fake();
        unscoped.scope = None;
        let (result, _, _) = run_with(&unscoped, &args(true), 0, None);
        assert!(format!("{:#}", result.unwrap_err()).contains("no ownership scope"));
        assert_eq!(records(), 0);
    }

    #[test]
    fn adopt_refuses_a_non_sprites_env() {
        use thegn_core::config::EnvConfig;
        let mut cfg = Config::default();
        for (name, kind) in [("s1", "sprites"), ("v", "vercel")] {
            cfg.env.insert(
                name.into(),
                EnvConfig {
                    provider: EnvProviderConfig {
                        provider: kind.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
        }
        assert!(sprites_env(&cfg, "s1").is_ok());
        for env in ["v", "missing"] {
            let text = format!("{:#}", sprites_env(&cfg, env).unwrap_err());
            assert!(text.contains("not a sprites env"), "{text}");
            assert!(text.contains("sprites envs: s1"), "{text}");
        }
    }
}
