use crate::{Arch, Cache, Env, Environment, Feature, JobSpec, RetryLimit, Spot};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use std::num::NonZeroU32;

impl JobSpec {
    pub fn parse_yaml(input: &str) -> Result<Self> {
        let spec: Self = serde_saphyr::with_deserializer_from_str(input, |de| {
            serde_path_to_error::deserialize(de).map_err(serde::de::Error::custom)
        })?;
        spec.validate()?;
        Ok(spec)
    }

    /// Checks semantic constraints after structural deserialization. Daemons
    /// accepting a structured request must also validate it before resolving it.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.metadata.name.trim().is_empty(),
            "metadata.name is empty"
        );
        if self.recipe.is_some() {
            ensure!(
                self.runner
                    .as_ref()
                    .is_some_and(|runner| valid_runner_alias(runner)),
                "typed recipe requires a valid explicitly selected runner"
            );
            ensure!(
                self.steps.is_empty(),
                "typed recipes cannot be combined with command steps"
            );
            ensure!(
                matches!(&self.source.snapshot, crate::Snapshot::Pinned(_)),
                "typed recipe requires a pinned source digest"
            );
            if let Some(field) = self.unhonoured_recipe_field() {
                bail!("typed recipes do not honour {field}");
            }
        } else {
            ensure!(
                self.runner.is_none(),
                "runner selection requires a typed recipe"
            );
            ensure!(!self.steps.is_empty(), "steps must contain a command");
        }
        match &self.environment {
            Environment::Image { image } => {
                ensure!(!image.trim().is_empty(), "environment.image is empty")
            }
            Environment::Nix { nix } => ensure!(
                !nix.flake.trim().is_empty(),
                "environment.nix.flake is empty"
            ),
        }
        for (name, value) in [
            ("memory", &self.resources.memory),
            ("disk", &self.resources.disk),
            ("shm", &self.resources.shm),
        ] {
            if let Some(value) = value {
                byte_quantity(value).with_context(|| format!("resources.{name}"))?;
            }
        }
        duration_ms(&self.timeout).context("timeout")?;
        for (i, step) in self.steps.iter().enumerate() {
            ensure!(!step.name.trim().is_empty(), "steps[{i}].name is empty");
            let valid = match (&step.run, &step.argv) {
                (Some(script), None) => !script.trim().is_empty() && !script.contains('\0'),
                (None, Some(argv)) => {
                    argv.first().is_some_and(|a| !a.is_empty())
                        && argv.iter().all(|a| !a.contains('\0'))
                }
                _ => false,
            };
            ensure!(
                valid,
                "steps[{i}] requires exactly one nonempty run or argv"
            );
            if let Some(cwd) = &step.cwd {
                check_path(cwd, false).with_context(|| format!("steps[{i}].cwd"))?;
            }
            if let Some(timeout) = &step.timeout {
                duration_ms(timeout).with_context(|| format!("steps[{i}].timeout"))?;
            }
        }
        check_env(&self.env).context("env")?;
        for secret in &self.secrets {
            ensure!(
                valid_env_name(secret) && !secret.starts_with("THEGN_"),
                "invalid secrets entry {secret:?}"
            );
        }
        let mut names = BTreeSet::new();
        for (i, service) in self.services.iter().enumerate() {
            ensure!(
                !service.name.trim().is_empty() && names.insert(&service.name),
                "services[{i}].name is empty or duplicated"
            );
            ensure!(
                !service.image.trim().is_empty(),
                "services[{i}].image is empty"
            );
            check_env(&service.env).with_context(|| format!("services[{i}].env"))?;
        }
        for (i, cache) in self.caches.iter().enumerate() {
            if let Cache::Keyed {
                key,
                paths,
                restore_keys,
            } = cache
            {
                ensure!(
                    !key.trim().is_empty()
                        && !paths.is_empty()
                        && restore_keys.iter().all(|k| !k.is_empty()),
                    "invalid caches[{i}] key, paths or restoreKeys"
                );
                check_cache_key(key).with_context(|| format!("caches[{i}].key"))?;
                for path in paths {
                    check_path(path, true).with_context(|| format!("caches[{i}].paths"))?;
                }
            }
        }
        for (field, paths) in [
            ("artifacts", &self.artifacts),
            ("source.exclude", &self.source.exclude),
        ] {
            for path in paths {
                check_path(path, false).context(field)?;
            }
        }
        if let Some(glob) = &self.results.junit {
            check_path(glob, false).context("results.junit")?;
        }
        Ok(())
    }

    /// Fields the typed selected-runner route cannot forward. A recipe spec
    /// must leave them at their defaults so a value the user set is never
    /// silently ignored. The deadline (`timeout`) is forwarded to the runner,
    /// so it is deliberately absent.
    fn unhonoured_recipe_field(&self) -> Option<&'static str> {
        if !self.env.is_empty() {
            return Some("env");
        }
        if !self.secrets.is_empty() {
            return Some("secrets");
        }
        if !self.services.is_empty() {
            return Some("services");
        }
        if !self.caches.is_empty() {
            return Some("caches");
        }
        if self.shards != NonZeroU32::MIN {
            return Some("shards");
        }
        let resources = &self.resources;
        if resources.cpu.is_some()
            || resources.memory.is_some()
            || resources.shm.is_some()
            || resources.disk.is_some()
            || resources.arch != Arch::Any
        {
            return Some("resources");
        }
        if self.isolation.is_some() {
            return Some("isolation");
        }
        let placement = &self.placement;
        if placement.providers.is_some()
            || placement.regions.is_some()
            || placement.spot != Spot::Allowed
            || !placement.affinity.is_empty()
        {
            return Some("placement");
        }
        if !self.artifacts.is_empty() {
            return Some("artifacts");
        }
        if self.results.junit.is_some() || self.results.cargo {
            return Some("results");
        }
        if self.retry.escalate.is_some()
            || !matches!(self.retry.on_preempt, RetryLimit::Count { max: 2 })
            || !matches!(self.retry.on_failure, RetryLimit::Count { max: 0 })
        {
            return Some("retry");
        }
        if !self.source.exclude.is_empty() {
            return Some("source.exclude");
        }
        None
    }

    /// Return the unmet feature names for this particular offering. The caller
    /// can include these directly in a rejection or placement trace.
    pub fn missing_features(&self, offered: &BTreeSet<String>) -> Vec<&'static str> {
        self.required_features()
            .into_iter()
            .map(Feature::wire_name)
            .filter(|name| !offered.contains(*name))
            .collect()
    }

    /// Compare this entire set against each offering. The broker additionally
    /// checks arch, shape, effective isolation, location, budget and trust.
    pub fn required_features(&self) -> BTreeSet<Feature> {
        let env = match self.environment {
            Environment::Image { .. } => Feature::Oci,
            Environment::Nix { .. } => Feature::NixEnv,
        };
        let mut features = BTreeSet::from([env]);
        for (required, feature) in [
            (self.resources.shm.is_some(), Feature::Shm),
            (!self.services.is_empty(), Feature::Services),
            (!self.secrets.is_empty(), Feature::Secrets),
            (self.results.junit.is_some(), Feature::Junit),
            (self.results.cargo, Feature::CargoJson),
        ] {
            if required {
                features.insert(feature);
            }
        }
        for cache in &self.caches {
            features.insert(match cache {
                Cache::Keyed { .. } => Feature::CacheVolume,
                Cache::Tool { tool } => Feature::CacheTool(*tool),
            });
        }
        features
    }
}

// Only hash(snapshot-relative-file) is defined for v0 key interpolation.
fn check_cache_key(mut key: &str) -> Result<()> {
    while let Some((_, rest)) = key.split_once("${") {
        let (expression, remaining) = rest
            .split_once('}')
            .context("unterminated key expression")?;
        let file = expression
            .strip_prefix("hash(")
            .and_then(|s| s.strip_suffix(')'))
            .context("expected hash(file) in cache key")?;
        check_path(file, false)?;
        key = remaining;
    }
    Ok(())
}

fn valid_env_name(s: &str) -> bool {
    s.bytes()
        .next()
        .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
        && s.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

fn valid_runner_alias(runner: &str) -> bool {
    !runner.is_empty()
        && runner.len() <= 128
        && runner
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && runner.as_bytes()[0].is_ascii_alphanumeric()
        && runner.as_bytes()[runner.len() - 1].is_ascii_alphanumeric()
}

fn check_env(env: &Env) -> Result<()> {
    for (key, value) in env {
        ensure!(
            valid_env_name(key) && !key.starts_with("THEGN_") && !value.contains('\0'),
            "invalid environment entry {key:?}"
        );
    }
    Ok(())
}

// Lexical check only. The executor separately confines symlink resolution.
fn check_path(path: &str, guest_home: bool) -> Result<()> {
    let path = if guest_home {
        path.strip_prefix("~/").unwrap_or(path)
    } else {
        path
    };
    ensure!(
        !path.is_empty()
            && !path.starts_with(['/', '~'])
            && !path.contains(['\0', '\\', ':'])
            && !path.split('/').any(|part| part == ".."),
        "expected a confined guest path"
    );
    Ok(())
}

fn quantity(input: &str, units: &[(&str, u64)]) -> Result<u64> {
    let pos = input.bytes().take_while(u8::is_ascii_digit).count();
    let number: u64 = input[..pos]
        .parse()
        .context("expected an unsigned integer")?;
    let unit = &input[pos..];
    let (_, multiplier) = units
        .iter()
        .find(|(name, _)| *name == unit)
        .context("unknown unit")?;
    ensure!(number > 0, "quantity must be positive");
    number
        .checked_mul(*multiplier)
        .context("quantity exceeds u64")
}

pub fn byte_quantity(input: &str) -> Result<u64> {
    quantity(
        input,
        &[
            ("", 1),
            ("B", 1),
            ("Ki", 1 << 10),
            ("Mi", 1 << 20),
            ("Gi", 1 << 30),
            ("Ti", 1 << 40),
        ],
    )
}

pub fn duration_ms(input: &str) -> Result<u64> {
    quantity(
        input,
        &[("ms", 1), ("s", 1000), ("m", 60_000), ("h", 3_600_000)],
    )
}
