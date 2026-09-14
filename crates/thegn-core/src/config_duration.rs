//! Duration-only admission shared by strict config writes/overlays and the
//! permissive loader's diagnostics. Bounds come from the generated schema;
//! hot environment DTO checks are direct arithmetic and tested against those bounds.
//!
//! The base-file loader must retain successfully parsed security settings on a
//! duration error. Consumers therefore enforce safe arithmetic independently;
//! this module never turns one duration into whole-file default fallback.

use schemars::schema::{RootSchema, Schema, SchemaObject, SingleOrVec};

/// Validate only supported numeric duration ranges, ignoring unrelated legacy
/// enum/type diagnostics. Explicit overlays reject newly introduced errors.
pub fn errors_for_config(cfg: &crate::config::Config) -> Vec<String> {
    match serde_json::to_value(cfg) {
        Ok(value) => {
            static SCHEMA: std::sync::OnceLock<RootSchema> = std::sync::OnceLock::new();
            let root = SCHEMA.get_or_init(|| schemars::schema_for!(crate::config::Config));
            let mut errors = Vec::new();
            walk(&root.schema, root, &value, "", &mut errors);
            errors.sort();
            errors.dedup();
            errors
        }
        Err(error) => vec![format!(
            "duration validation: cannot inspect config: {error}"
        )],
    }
}

/// Cache duration diagnostics for unchanged base source content. This cache
/// only suppresses repeated validation work, never authorizes runtime policy.
/// Bounded fingerprints retain neither complete configs nor source contents.
pub(crate) fn errors_for_base(cfg: &crate::config::Config, body: &str) -> Vec<String> {
    use std::hash::{Hash, Hasher};
    type Cached = std::collections::VecDeque<(u64, usize, Vec<String>)>;
    static CACHE: std::sync::Mutex<Cached> =
        std::sync::Mutex::new(std::collections::VecDeque::new());
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hash);
    let key = hash.finish();
    if let Ok(cache) = CACHE.lock()
        && let Some((_, _, errors)) = cache
            .iter()
            .find(|(fingerprint, len, _)| *fingerprint == key && *len == body.len())
    {
        return errors.clone();
    }
    let errors = errors_for_config(cfg);
    if let Ok(mut cache) = CACHE.lock() {
        if cache.len() >= 32 {
            cache.pop_front();
        }
        cache.push_back((key, body.len(), errors.clone()));
    }
    errors
}

/// Validate a small explicit patch; do not reserialize/walk the whole base
/// Config twice when only one option changes.
pub(crate) fn validate_override(key: &str, value: &serde_json::Value) -> Result<(), String> {
    let mut patch = value.clone();
    for part in key.split('.').rev() {
        patch = serde_json::Value::Object(serde_json::Map::from_iter([(part.to_string(), patch)]));
    }
    introduced(&[], errors_for_value::<crate::config::Config>(&patch))
}

/// Environment DTO duration checks are direct arithmetic. The usual hydration
/// load adds no Config clone, JSON serialization, or schema walk for this layer.
pub(crate) fn retain_valid_env_durations(
    overlay: &mut crate::config::ConfigOverlay,
) -> Vec<String> {
    let mut errors = Vec::new();
    macro_rules! integer {
        ($field:expr, $name:literal, $max:expr) => {
            if let Some(value) = $field
                && u128::from(value) > u128::from($max)
            {
                errors.push(format!(
                    "environment.{}: duration exceeds supported range (got {value}); override ignored",
                    $name
                ));
                $field = None;
            }
        };
    }
    macro_rules! floating {
        ($field:expr, $name:literal, $max:expr) => {
            if let Some(value) = $field
                && (!value.is_finite() || value < 0.0 || value > $max as f64)
            {
                errors.push(format!(
                    "environment.{}: duration exceeds supported range (got {value}); override ignored",
                    $name
                ));
                $field = None;
            }
        };
    }
    use crate::time_policy::{
        MAX_CADENCE_SECS, MAX_DURATION_DAYS, MAX_DURATION_MILLIS, MAX_DURATION_SECS,
    };
    integer!(overlay.pr_ttl_secs, "pr_ttl_secs", MAX_DURATION_SECS);
    integer!(
        overlay.watch_pr_interval_secs,
        "watch_pr_interval_secs",
        MAX_CADENCE_SECS
    );
    floating!(
        overlay.metrics_interval_secs,
        "metrics_interval_secs",
        MAX_CADENCE_SECS
    );
    integer!(
        overlay.metrics_timeout_ms,
        "metrics_timeout_ms",
        MAX_DURATION_MILLIS
    );
    floating!(
        overlay.activity_runaway_secs,
        "activity_runaway_secs",
        MAX_DURATION_SECS
    );
    integer!(
        overlay.disk_scan_interval_secs,
        "disk_scan_interval_secs",
        MAX_CADENCE_SECS
    );
    integer!(
        overlay.disk_idle_clean_days,
        "disk_idle_clean_days",
        MAX_DURATION_DAYS
    );
    integer!(
        overlay.loc_scan_interval_secs,
        "loc_scan_interval_secs",
        MAX_CADENCE_SECS
    );
    integer!(
        overlay.loc_watch_invalidate_secs,
        "loc_watch_invalidate_secs",
        MAX_DURATION_SECS
    );
    integer!(
        overlay.preview.fetch_timeout_ms,
        "preview.fetch_timeout_ms",
        MAX_DURATION_MILLIS
    );
    errors
}

#[cfg(test)]
fn errors_for_env_overlay(overlay: &crate::config::ConfigOverlay) -> Vec<String> {
    retain_valid_env_durations(&mut overlay.clone())
}

/// Strict raw-document boundary, used before any config write becomes visible.
pub fn errors_for_str(body: &str) -> Vec<String> {
    let normalized = match crate::config_compat::normalize(body) {
        Ok(value) => value,
        Err(_) => return Vec::new(), // existing syntax validation owns this error
    };
    match toml::from_str::<serde_json::Value>(&normalized.body) {
        Ok(value) => errors_for_value::<crate::config::Config>(&value),
        Err(_) => Vec::new(), // existing parse/type validation owns this error
    }
}

pub(crate) fn errors_for_value<T: schemars::JsonSchema>(value: &serde_json::Value) -> Vec<String> {
    let root = schemars::schema_for!(T);
    let mut errors = Vec::new();
    walk(&root.schema, &root, value, "", &mut errors);
    errors.sort();
    errors.dedup();
    errors
}

/// Newly invalid durations are rejected atomically; pre-existing bad settings
/// must not prevent an unrelated repair. Runtime consumers remain fail-safe.
pub fn introduced(before: &[String], after: Vec<String>) -> Result<(), String> {
    let new: Vec<_> = after
        .into_iter()
        .filter(|error| !before.contains(error))
        .collect();
    if new.is_empty() {
        Ok(())
    } else {
        Err(new.join("; "))
    }
}

pub(crate) fn check_number(
    obj: &SchemaObject,
    value: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    let Some(maximum) = obj.number.as_ref().and_then(|number| number.maximum) else {
        return;
    };
    if let Some(number) = value.as_f64() {
        if !number.is_finite() || number < 0.0 || number > maximum {
            errors.push(format!("{path}: duration must be in 0..={maximum} in the field's documented units (got {value})"));
        }
    } else if value.is_null()
        && obj
            .instance_type
            .as_ref()
            .is_some_and(|types| !types.contains(&schemars::schema::InstanceType::Null))
    {
        // serde_json represents a parsed floating NaN/Infinity as null. A
        // genuine optional duration includes Null in its schema and is valid.
        errors.push(format!("{path}: duration must be finite"));
    }
}

// These are the two serde field aliases on numeric durations. Keep raw strict
// admission aligned without deserializing a whole permissive Config (which
// could fail for an unrelated legacy setting). Diagnostic paths retain input
// spelling; canonical schema and runtime serialization use underscores.
fn duration_property_key<'a>(path: &str, key: &'a str) -> &'a str {
    match (path, key) {
        ("metrics", "interval-secs") => "interval_secs",
        ("metrics", "timeout-ms") => "timeout_ms",
        _ => key,
    }
}

fn walk_schema(
    schema: &Schema,
    root: &RootSchema,
    value: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    if let Schema::Object(obj) = schema {
        walk(obj, root, value, path, errors);
    }
}

fn walk(
    obj: &SchemaObject,
    root: &RootSchema,
    value: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    if let Some(reference) = &obj.reference {
        if let Some(schema) = reference
            .rsplit('/')
            .next()
            .and_then(|name| root.definitions.get(name))
        {
            walk_schema(schema, root, value, path, errors);
        }
        return;
    }
    check_number(obj, value, path, errors);
    if let Some(sub) = &obj.subschemas {
        for schema in sub
            .all_of
            .iter()
            .chain(sub.any_of.iter())
            .chain(sub.one_of.iter())
            .flatten()
        {
            walk_schema(schema, root, value, path, errors);
        }
    }
    match value {
        serde_json::Value::Object(fields) => {
            if let Some(schema) = &obj.object {
                for (key, child) in fields {
                    if let Some(property) = schema
                        .properties
                        .get(duration_property_key(path, key))
                        .or(schema.additional_properties.as_deref())
                    {
                        let next = if path.is_empty() {
                            key.clone()
                        } else {
                            format!("{path}.{key}")
                        };
                        walk_schema(property, root, child, &next, errors);
                    }
                }
            }
        }
        serde_json::Value::Array(items) => {
            if let Some(schema) = &obj.array
                && let Some(SingleOrVec::Single(item)) = &schema.items
            {
                for (index, child) in items.iter().enumerate() {
                    walk_schema(item, root, child, &format!("{path}[{index}]"), errors);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, MapEnv};
    use crate::time_policy::{MAX_CADENCE_SECS, MAX_DURATION_SECS};

    #[test]
    fn strict_duration_aliases_cannot_bypass_numeric_admission() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = "[sandbox]\nisolation_floor='shared-kernel'\n";
        std::fs::write(&path, original).unwrap();
        for (alias, max) in [
            ("interval-secs", MAX_CADENCE_SECS),
            ("timeout-ms", crate::time_policy::MAX_DURATION_MILLIS),
        ] {
            let good = format!("[metrics]\n{alias}={max}\n");
            let bad = format!("[metrics]\n{alias}={}\n", max + 1);
            assert!(errors_for_str(&good).is_empty());
            assert!(!errors_for_str(&bad).is_empty());
            assert!(
                crate::config_write::set_key(
                    &path,
                    &format!("metrics.{alias}"),
                    &(max + 1).to_string()
                )
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            let mut cfg = Config::default();
            assert!(Config::apply_toml_overlay(&mut cfg, &bad).is_err());
        }
    }

    #[test]
    fn cached_base_diagnostics_follow_source_changes() {
        let good = "[ci]\npoll_interval_secs=90\n";
        let bad = "[ci]\npoll_interval_secs=2305843009213693952\n";
        let good_cfg: Config = toml::from_str(good).unwrap();
        let bad_cfg: Config = toml::from_str(bad).unwrap();
        assert!(errors_for_base(&good_cfg, good).is_empty());
        let expected = errors_for_config(&bad_cfg);
        assert!(!expected.is_empty());
        assert_eq!(errors_for_base(&bad_cfg, bad), expected);
        assert_eq!(errors_for_base(&bad_cfg, bad), expected);
        assert!(errors_for_base(&good_cfg, good).is_empty());
    }

    #[test]
    fn direct_environment_checks_match_complete_schema_policy() {
        use crate::config::ConfigOverlay;
        use crate::time_policy::{MAX_DURATION_DAYS, MAX_DURATION_MILLIS};
        for invalid in [false, true] {
            let extra = u64::from(invalid);
            let mut overlay = ConfigOverlay {
                pr_ttl_secs: Some(MAX_DURATION_SECS + extra),
                watch_pr_interval_secs: Some(MAX_CADENCE_SECS + extra),
                metrics_interval_secs: Some((MAX_CADENCE_SECS + extra) as f64),
                metrics_timeout_ms: Some(MAX_DURATION_MILLIS + extra),
                activity_runaway_secs: Some((MAX_DURATION_SECS + extra) as f64),
                disk_scan_interval_secs: Some(MAX_CADENCE_SECS + extra),
                disk_idle_clean_days: Some((MAX_DURATION_DAYS + extra) as u32),
                loc_scan_interval_secs: Some(MAX_CADENCE_SECS + extra),
                loc_watch_invalidate_secs: Some(MAX_DURATION_SECS + extra),
                ..Default::default()
            };
            overlay.preview.fetch_timeout_ms = Some(MAX_DURATION_MILLIS + extra);
            let direct = errors_for_env_overlay(&overlay);
            let mut cfg = Config::default();
            overlay.apply(&mut cfg);
            let full = errors_for_config(&cfg);
            assert_eq!(direct.len(), if invalid { 10 } else { 0 });
            assert_eq!(direct.len(), full.len());
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, -1.0] {
            let overlay = ConfigOverlay {
                metrics_interval_secs: Some(value),
                activity_runaway_secs: Some(value),
                ..Default::default()
            };
            assert_eq!(errors_for_env_overlay(&overlay).len(), 2);
        }
        assert!(errors_for_env_overlay(&ConfigOverlay::default()).is_empty());
    }

    #[test]
    fn defaults_and_zero_inheritance_have_no_duration_errors() {
        assert!(errors_for_config(&Config::default()).is_empty());
        for body in [
            "",
            "[calendar]\nrefresh_interval_secs=0\n",
            "[[calendar.accounts]]\nname='work'\nrefresh_interval_secs=0\n",
            "[daemon]\nlease_grace_secs=0\n",
            "[model_proxy.budget]\nwindow_secs=0\n",
        ] {
            assert!(errors_for_str(body).is_empty(), "{body}");
        }
    }

    #[test]
    fn every_shared_cadence_range_matches_schema_and_strict_validation() {
        for (table, field) in [
            ("ci", "poll_interval_secs"),
            ("pr_queue", "poll_interval_secs"),
            ("calendar", "refresh_interval_secs"),
            ("usage", "poll_interval_secs"),
            ("weather", "refresh_interval_secs"),
        ] {
            for value in [
                0,
                1,
                5,
                15,
                60,
                120,
                MAX_CADENCE_SECS,
                MAX_CADENCE_SECS + 1,
                1 << 61,
            ] {
                let body = format!("[{table}]\n{field}={value}\n");
                let errors = errors_for_str(&body);
                assert_eq!(
                    errors.is_empty(),
                    value <= MAX_CADENCE_SECS,
                    "{body}: {errors:?}"
                );
                if value > MAX_CADENCE_SECS {
                    assert!(
                        crate::config_validate::validate_str(&body)
                            .iter()
                            .any(|error| error.contains(&format!("{table}.{field}")))
                    );
                }
            }
        }
        let body = format!(
            "[[calendar.accounts]]\nname='work'\nrefresh_interval_secs={}\n",
            1u64 << 61
        );
        assert!(
            errors_for_str(&body)
                .iter()
                .any(|error| error.contains("calendar.accounts[0].refresh_interval_secs"))
        );
    }

    #[test]
    fn unsigned_programmatic_extremes_are_diagnosed_without_narrowing() {
        for seconds in [
            i64::MAX as u64 - 1,
            i64::MAX as u64,
            i64::MAX as u64 + 1,
            u64::MAX,
        ] {
            let mut cfg = Config::default();
            cfg.daemon.lease_grace_secs = seconds;
            cfg.ci.ttl_secs = seconds;
            let errors = errors_for_config(&cfg);
            assert!(errors.iter().any(|e| e.contains("daemon.lease_grace_secs")));
            assert!(errors.iter().any(|e| e.contains("ci.ttl_secs")));
        }
    }

    #[test]
    fn permissive_base_duration_error_retains_security_policy() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let body = format!(
            "[sandbox]\nisolation_floor='shared-kernel'\n[ci]\npoll_interval_secs={}\n[env.expensive.provider]\nmax_lifetime_secs={}\n",
            1u64 << 61,
            MAX_DURATION_SECS + 1
        );
        std::fs::write(&path, &body).unwrap();
        let cfg = Config::load_layered(&MapEnv::default(), &[], Some(path));
        assert_eq!(
            cfg.sandbox.isolation_floor,
            crate::config::IsolationFloor::SharedKernel
        );
        assert_eq!(cfg.ci.poll_interval_secs, 1u64 << 61);
        let lifetime = cfg.env["expensive"].provider.max_lifetime_secs;
        assert_eq!(
            crate::time_policy::lifetime_expired(2_000_000_000, Some(1), lifetime),
            None
        );
        assert!(!errors_for_config(&cfg).is_empty());
    }

    #[test]
    fn invalid_explicit_overlay_is_atomic_and_unrelated_repair_is_allowed() {
        let mut cfg = Config::default();
        cfg.sandbox.isolation_floor = crate::config::IsolationFloor::SharedKernel;
        let before = cfg.ci.poll_interval_secs;
        assert!(
            Config::apply_toml_overlay(
                &mut cfg,
                &format!("[ci]\npoll_interval_secs={}\n", 1u64 << 61)
            )
            .is_err()
        );
        assert_eq!(cfg.ci.poll_interval_secs, before);
        assert_eq!(
            cfg.sandbox.isolation_floor,
            crate::config::IsolationFloor::SharedKernel
        );
        assert!(
            Config::apply_override_str(&mut cfg, "ci.poll_interval_secs", &u64::MAX.to_string())
                .is_err()
        );
        cfg.ci.poll_interval_secs = u64::MAX; // permissive base's diagnosed value
        assert!(Config::apply_override_str(&mut cfg, "pr.ttl_secs", "123").is_ok());
        assert_eq!(cfg.pr.ttl_secs, 123);
        assert!(Config::apply_override_str(&mut cfg, "ci.poll_interval_secs", "90").is_ok());
        assert!(errors_for_config(&cfg).is_empty());
    }

    #[test]
    fn invalid_env_duration_cannot_discard_base_security_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            "[sandbox]\nisolation_floor='shared-kernel'\n[pr]\nttl_secs=33\n",
        )
        .unwrap();
        let env = MapEnv(std::collections::BTreeMap::from([(
            "THEGN_PR_TTL".into(),
            u64::MAX.to_string(),
        )]));
        let cfg = Config::load_layered(&env, &[], Some(path));
        assert_eq!(cfg.pr.ttl_secs, 33);
        assert_eq!(
            cfg.sandbox.isolation_floor,
            crate::config::IsolationFloor::SharedKernel
        );
    }

    #[test]
    fn invalid_env_duration_retains_valid_explicit_security_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "[pr]\nttl_secs=33\n").unwrap();
        let env = MapEnv(std::collections::BTreeMap::from([
            ("THEGN_PR_TTL".into(), u64::MAX.to_string()),
            (
                "THEGN_SANDBOX_ISOLATION_FLOOR".into(),
                "shared-kernel".into(),
            ),
            ("THEGN_SANDBOX_NETWORK".into(), "none".into()),
        ]));
        let cfg = Config::load_layered(&env, &[], Some(path));
        assert_eq!(cfg.pr.ttl_secs, 33);
        assert_eq!(
            cfg.sandbox.isolation_floor,
            crate::config::IsolationFloor::SharedKernel
        );
        assert_eq!(cfg.sandbox.network, crate::config::Network::None);
    }

    #[test]
    fn duration_write_rejection_happens_before_existing_or_missing_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let invalid = (1u64 << 61).to_string();
        assert!(crate::config_write::set_key(&path, "ci.poll_interval_secs", &invalid).is_err());
        assert!(!path.exists());
        let original = "# keep my security policy\n[sandbox]\nisolation_floor='shared-kernel'\n";
        std::fs::write(&path, original).unwrap();
        assert!(crate::config_write::set_key(&path, "ci.poll_interval_secs", &invalid).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let spec = crate::config_write::EnvSpec {
            name: "expensive".into(),
            placement: "provider".into(),
            provider: Some("hetzner".into()),
            max_lifetime_secs: Some(MAX_DURATION_SECS as i64 + 1),
            ..Default::default()
        };
        assert!(crate::config_write::upsert_env(&path, &spec).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn floating_duration_is_finite_and_millisecond_bounds_use_milliseconds() {
        let mut cfg = Config::default();
        cfg.stats.refresh_secs = f64::INFINITY;
        assert!(
            errors_for_config(&cfg)
                .iter()
                .any(|error| error.contains("stats.refresh_secs"))
        );
        assert!(errors_for_str("[preview]\nfetch_timeout_ms=315360000000\n").is_empty());
        assert!(!errors_for_str("[preview]\nfetch_timeout_ms=315360000001\n").is_empty());
    }
}
