//! Display snapshots for the System ▸ Environments panel section — one row per
//! configured `[env.<name>]` with its placement kind, region/size, and (for
//! managed-provider envs) whether a token resolves. Pure over the config +
//! secret backend; built off-loop by hydration into `model.panel.environments`
//! (mirroring `host_ui::host_snapshots` → `model.panel.hosts`), so the render
//! path just reads the precomputed list.

use superzej_core::config::{Config, EnvProviderConfig, PlacementMode};

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
    /// `Some(true/false)` whether a token resolves for a provider env; `None` for
    /// non-provider placements (which need no token).
    pub token: Option<bool>,
}

/// Build the display list for every configured env (sorted by name). Cheap: a
/// map walk + one secret probe per provider env.
pub fn env_snapshots(cfg: &Config) -> Vec<EnvSnapshot> {
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
                !key.is_empty() && crate::secret::resolve(&key).is_some()
            });
            EnvSnapshot {
                name: name.clone(),
                kind,
                placement,
                region: e.provider.region.clone(),
                size: e.provider.size.clone(),
                token,
            }
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// The SecretRef (or bare env-var name) a provider env's token resolves from:
/// the explicit `api_key_env`, else the provider's built-in default.
fn effective_token_env(pc: &EnvProviderConfig) -> String {
    let k = pc.api_key_env.trim();
    if !k.is_empty() {
        return k.to_string();
    }
    match pc.provider.trim() {
        "sprites" => "SPRITES_TOKEN".to_string(),
        "fly" => "FLY_API_TOKEN".to_string(),
        p => superzej_svc::vps::VpsKind::parse(p)
            .map(|k| k.token_env_default().to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_env(
        provider: &str,
        api_key_env: &str,
        region: &str,
    ) -> superzej_core::config::EnvConfig {
        superzej_core::config::EnvConfig {
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
        unsafe { std::env::set_var("SZ_ENVUI_TOK", "x") };
        cfg.env
            .insert("z-fly".into(), provider_env("fly", "SZ_ENVUI_TOK", "iad"));
        // A local env (no token concept).
        cfg.env.insert(
            "a-local".into(),
            superzej_core::config::EnvConfig::default(),
        );

        let snaps = env_snapshots(&cfg);
        // Sorted by name.
        assert_eq!(snaps[0].name, "a-local");
        assert_eq!(snaps[0].placement, "local");
        assert_eq!(snaps[0].token, None, "local needs no token");
        assert_eq!(snaps[1].name, "z-fly");
        assert_eq!(snaps[1].kind, "fly");
        assert_eq!(snaps[1].region, "iad");
        assert_eq!(snaps[1].token, Some(true), "token env var is set");
        unsafe { std::env::remove_var("SZ_ENVUI_TOK") };
    }

    #[test]
    fn provider_env_without_token_reports_false() {
        let mut cfg = Config::default();
        // api_key_env points at an unset var → token missing.
        cfg.env.insert(
            "h".into(),
            provider_env("hetzner", "SZ_ENVUI_DEFINITELY_UNSET", ""),
        );
        assert_eq!(env_snapshots(&cfg)[0].token, Some(false));
    }
}
