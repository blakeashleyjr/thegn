//! Binds a claimed pipeline stage to the launch configuration the caller used.
//!
//! THE-268: the prompt template is deliberately not included. The CLI sends
//! the already-rendered prompt, while this digest covers launch authority and
//! stage capacity as resolved from configuration.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::agent_task::effective_agent;
use crate::config::Config;

/// Private request marker carrying the stage configuration digest to the daemon.
pub const STAGE_CONFIG_DIGEST_ENV: &str = "THEGN_STAGE_CONFIG_DIGEST";

const DOMAIN: &[u8] = b"thegn-stage-launch-digest-v2\0";

/// A typed refusal when the claimed stage configuration no longer matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageConfigDrift {
    /// The stage whose configuration changed.
    pub stage: String,
    /// The agent requested for that stage.
    pub agent: String,
    /// Why the launch configuration could not be recomputed, when applicable.
    pub detail: Option<String>,
}

impl std::fmt::Display for StageConfigDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stage launch refused: the configuration for pipeline stage `{}` (agent `{}`) changed between claim and launch",
            self.stage, self.agent
        )?;
        if let Some(detail) = &self.detail {
            write!(f, ": {detail}")?;
        }
        write!(f, "; retry the dispatch")
    }
}

impl std::error::Error for StageConfigDrift {}

/// Hash the stage capacity and its effective launch configuration.
///
/// Every field is encoded as `{byte_len}:{bytes}` after a versioned domain tag,
/// so adjacent values cannot forge boundaries. Environment values remain in
/// their configured `env:`/`file:` form; secrets are not expanded here.
pub fn stage_launch_digest(cfg: &Config, agent: &str, stage: &str) -> Result<String, String> {
    let stage_name = stage.trim();
    let configured_stage = cfg
        .pipeline
        .stage(stage_name)
        .ok_or_else(|| format!("unknown pipeline stage `{stage_name}`"))?;
    let effective = effective_agent(cfg, agent, Some(stage_name))?;

    let mut hash = Sha256::new();
    hash.update(DOMAIN);
    let mut add = |value: &str| {
        hash.update(value.len().to_string().as_bytes());
        hash.update(b":");
        hash.update(value.as_bytes());
    };

    add(&configured_stage.name);
    add(&configured_stage.agent);
    add(&configured_stage.concurrency.to_string());
    add(&effective.name);
    add(&effective.command);
    add(&effective.harness);
    let provider = cfg
        .agents
        .iter()
        .chain(cfg.tools.iter())
        .find(|entry| entry.name == effective.name)
        .and_then(|entry| entry.provider.as_deref())
        .unwrap_or("");
    add(provider);
    add(effective.model.as_deref().unwrap_or(""));
    add(if effective.route_via_proxy { "1" } else { "0" });
    add(&effective.env.len().to_string());
    for (key, value) in &effective.env {
        add(key);
        add(value);
    }
    add(&effective.permissions.len().to_string());
    for permission in &effective.permissions {
        add(permission);
    }

    let mut text = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut text, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(text)
}

/// Accept legacy opens without a marker; otherwise refuse any changed or
/// unresolvable configuration as stage drift.
pub fn check_stage_launch(
    cfg: &Config,
    agent: &str,
    stage: &str,
    presented: Option<&str>,
) -> Result<(), StageConfigDrift> {
    let Some(presented) = presented else {
        return Ok(());
    };
    let detail = match stage_launch_digest(cfg, agent, stage) {
        Ok(expected) if expected == presented => return Ok(()),
        Ok(_) => None,
        Err(error) => Some(error),
    };
    Err(StageConfigDrift {
        stage: stage.trim().to_string(),
        agent: agent.trim().to_string(),
        detail,
    })
}

/// Require the digest for a launch-id-bound pipeline dispatch, while keeping
/// [`check_stage_launch`] available for legacy layered stage opens.
pub fn check_stage_launch_required(
    cfg: &Config,
    agent: &str,
    stage: &str,
    presented: Option<&str>,
) -> Result<(), StageConfigDrift> {
    let Some(presented) = presented else {
        return Err(StageConfigDrift {
            stage: stage.trim().to_string(),
            agent: agent.trim().to_string(),
            detail: Some("no stage configuration digest was presented".into()),
        });
    };
    check_stage_launch(cfg, agent, stage, Some(presented))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NamedCommand;
    use crate::config_pipeline::PipelineStage;

    fn config(stage_name: &str, stage_agent: &str) -> Config {
        let mut cfg = Config::default();
        cfg.agents.push(NamedCommand {
            name: "worker".into(),
            command: "claude".into(),
            hints: Vec::new(),
            provider: None,
            harness: None,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
            resume: false,
            route_via_proxy: false,
            drawer_scope: None,
            drawer_cwd: None,
        });
        cfg.pipeline.stages.push(PipelineStage {
            name: stage_name.into(),
            agent: stage_agent.into(),
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn digest_is_stable_and_hex() {
        let cfg = config("review", "worker");
        let digest = stage_launch_digest(&cfg, "worker", "review").expect("digest");
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_eq!(
            digest,
            stage_launch_digest(&cfg, "worker", "review").expect("same digest")
        );
    }

    #[test]
    fn digest_changes_with_stage_concurrency() {
        let first = config("review", "worker");
        let mut second = first.clone();
        second.pipeline.stages[0].concurrency += 1;
        assert_ne!(
            stage_launch_digest(&first, "worker", "review").expect("first"),
            stage_launch_digest(&second, "worker", "review").expect("second")
        );
    }

    #[test]
    fn digest_changes_with_stage_permissions() {
        let first = config("review", "worker");
        let mut second = first.clone();
        second.pipeline.stages[0].permissions.push("Read".into());
        assert_ne!(
            stage_launch_digest(&first, "worker", "review").expect("first"),
            stage_launch_digest(&second, "worker", "review").expect("second")
        );
    }

    #[test]
    fn digest_changes_with_agent_command_and_model() {
        let first = config("review", "worker");
        let mut command_changed = first.clone();
        command_changed.agents[0].command = "claude --verbose".into();
        let mut model_changed = first.clone();
        model_changed.pipeline.stages[0].model = Some("claude-sonnet-5".into());
        let mut stage_agent_changed = first.clone();
        stage_agent_changed.pipeline.stages[0].agent = "another-worker".into();
        let mut proxy_changed = first.clone();
        proxy_changed.agents[0].route_via_proxy = true;
        let expected = stage_launch_digest(&first, "worker", "review").expect("first");
        assert_ne!(
            expected,
            stage_launch_digest(&command_changed, "worker", "review").expect("command")
        );
        assert_ne!(
            expected,
            stage_launch_digest(&model_changed, "worker", "review").expect("model")
        );
        assert_ne!(
            expected,
            stage_launch_digest(&stage_agent_changed, "worker", "review").expect("stage agent")
        );
        assert_ne!(
            expected,
            stage_launch_digest(&proxy_changed, "worker", "review").expect("proxy route")
        );
    }

    #[test]
    fn digest_changes_with_env_values_without_expanding_them() {
        let first = config("review", "worker");
        let mut second = first.clone();
        second.pipeline.stages[0]
            .env
            .insert("TOKEN".into(), "file:secret".into());
        assert_ne!(
            stage_launch_digest(&first, "worker", "review").expect("first"),
            stage_launch_digest(&second, "worker", "review").expect("second")
        );
        assert_eq!(second.pipeline.stages[0].env["TOKEN"], "file:secret");
    }

    #[test]
    fn digest_fields_are_length_prefixed() {
        let first = config("ab", "c");
        let second = config("a", "bc");
        assert_ne!(
            stage_launch_digest(&first, "worker", "ab").expect("first"),
            stage_launch_digest(&second, "worker", "a").expect("second")
        );

        let mut env_first = config("review", "worker");
        env_first.pipeline.stages[0]
            .env
            .insert("AB".into(), "C".into());
        let mut env_second = config("review", "worker");
        env_second.pipeline.stages[0]
            .env
            .insert("A".into(), "BC".into());
        assert_ne!(
            stage_launch_digest(&env_first, "worker", "review").expect("first env boundary"),
            stage_launch_digest(&env_second, "worker", "review").expect("second env boundary")
        );
    }

    #[test]
    fn unknown_stage_is_an_error() {
        let error = stage_launch_digest(&config("review", "worker"), "worker", "missing")
            .expect_err("unknown stage refuses");
        assert!(error.contains("missing"), "{error}");
        let error = stage_launch_digest(&config("review", "worker"), "missing", "review")
            .expect_err("unknown agent refuses");
        assert!(error.contains("unknown agent"), "{error}");
    }

    #[test]
    fn check_refuses_a_mismatched_or_malformed_digest() {
        let cfg = config("review", "worker");
        let zeros = "0".repeat(64);
        for presented in [Some("not-a-digest"), Some(zeros.as_str())] {
            let error = check_stage_launch(&cfg, "worker", "review", presented)
                .expect_err("mismatch refuses");
            assert!(
                error
                    .to_string()
                    .contains("changed between claim and launch")
            );
        }
        let unknown = check_stage_launch(&cfg, "worker", "missing", Some(zeros.as_str()))
            .expect_err("unresolvable config refuses");
        assert!(unknown.to_string().contains("missing"));
        assert!(
            unknown
                .to_string()
                .contains("unknown pipeline stage `missing`"),
            "recompute reason was dropped: {unknown}"
        );
        let missing_agent = check_stage_launch(&cfg, "missing", "review", Some(zeros.as_str()))
            .expect_err("removed agent refuses");
        assert!(
            missing_agent.to_string().contains("unknown agent"),
            "recompute reason was dropped: {missing_agent}"
        );
    }

    #[test]
    fn check_accepts_an_absent_digest() {
        assert!(check_stage_launch(&Config::default(), "worker", "missing", None).is_ok());
    }

    #[test]
    fn check_stage_launch_required_refuses_a_missing_digest() {
        let cfg = config("review", "worker");
        let error = check_stage_launch_required(&cfg, "worker", "review", None)
            .expect_err("missing digest refuses");
        assert!(error.to_string().contains("no stage configuration digest"));
        let digest = stage_launch_digest(&cfg, "worker", "review").expect("digest");
        assert!(check_stage_launch_required(&cfg, "worker", "review", Some(&digest)).is_ok());
    }

    #[test]
    fn stage_launch_digest_binds_the_agent_provider() {
        let mut first = config("review", "worker");
        first.agents[0].harness = Some("claude".into());
        first.agents[0].provider = Some("account-one".into());
        let mut second = first.clone();
        second.agents[0].provider = Some("account-two".into());
        assert_ne!(
            stage_launch_digest(&first, "worker", "review").expect("first digest"),
            stage_launch_digest(&second, "worker", "review").expect("second digest")
        );
    }
}
