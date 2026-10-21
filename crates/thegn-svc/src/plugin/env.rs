//! Resolve environment values for a plugin immediately before it is started.
//!
//! Config-declared plugins may use the same typed secret references as other
//! host consumers. A plugin directory manifest cannot access the user's
//! secret broker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use thegn_core::secretref::{BareAs, SecretRef};

use super::{LoadedPlugin, SpecProblem};

/// Host-owned location for persistent plugin state.
pub const PLUGIN_STATE_DIR_ENV: &str = "THEGN_PLUGIN_STATE_DIR";

/// Ambient names passed to plugins that are not trusted native code.
///
/// This excludes credentials, agent sockets, XDG runtime paths, thegn control
/// endpoints, and proxy settings. A trusted-native config entry receives the
/// full ambient environment through [`resolve_env`].
/// `LLVM_PROFILE_FILE` is a coverage-output path present only in instrumented builds, not a credential.
pub(crate) const AMBIENT_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "TZ",
    "TERM",
    "TMPDIR",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "SYSTEMDRIVE",
    "LLVM_PROFILE_FILE",
];

pub(crate) fn ambient_allowlisted() -> Vec<(String, String)> {
    AMBIENT_ALLOWLIST
        .iter()
        .filter_map(|name| {
            std::env::var_os(name).and_then(|value| {
                value
                    .into_string()
                    .ok()
                    .map(|value| ((*name).to_string(), value))
            })
        })
        .collect()
}

pub(crate) fn apply_plugin_env(cmd: &mut Command, env: &BTreeMap<String, String>) {
    cmd.env_clear().envs(ambient_allowlisted()).envs(env);
}

/// Resolve the complete plugin environment beyond the fixed ambient allowlist.
/// Trusted-native plugins receive a snapshot of the full ambient environment
/// when this function runs (on plugin load or reload), rather than a live view
/// at spawn time. In practice the two are equivalent because thegn does not
/// mutate its own environment after startup.
pub fn resolve_env(plugin: &LoadedPlugin) -> Result<BTreeMap<String, String>, SpecProblem> {
    resolve_env_with(plugin, crate::secret::resolve_ref)
}

/// Create and resolve the private state directory used by one plugin.
pub fn ensure_plugin_state_dir(anchor: &Path, plugin_id: &str) -> Result<PathBuf, SpecProblem> {
    let path = anchor.join("plugins").join(plugin_id);
    if plugin_id.is_empty()
        || plugin_id == "."
        || plugin_id == ".."
        || plugin_id.contains('/')
        || plugin_id.contains('\\')
    {
        return Err(private_state_refusal(plugin_id, &path, "invalid plugin id"));
    }
    std::fs::create_dir_all(anchor)
        .map_err(|error| private_state_refusal(plugin_id, &path, &error.to_string()))?;
    let relative = Path::new("plugins").join(plugin_id);
    let private = thegn_core::fsperm::ensure_private_dir_beneath(anchor, &relative)
        .map_err(|error| private_state_refusal(plugin_id, &path, &error.to_string()))?;
    Ok(private.path().to_path_buf())
}

fn private_state_refusal(plugin: &str, path: &Path, error: &str) -> SpecProblem {
    SpecProblem {
        plugin: plugin.to_string(),
        problem: format!(
            "refused: private state directory {}: {error}; plugin not started",
            path.display()
        ),
    }
}

/// Resolve a plugin's launch environment and add its private persistent state directory.
pub fn resolve_launch_env(
    plugin: &LoadedPlugin,
    state_anchor: &Path,
) -> Result<BTreeMap<String, String>, SpecProblem> {
    resolve_launch_env_with(plugin, state_anchor, crate::secret::resolve_ref)
}

fn resolve_launch_env_with(
    plugin: &LoadedPlugin,
    state_anchor: &Path,
    resolve: impl FnMut(&SecretRef, &str) -> Option<String>,
) -> Result<BTreeMap<String, String>, SpecProblem> {
    let mut env = resolve_env_with(plugin, resolve)?;
    let path = ensure_plugin_state_dir(state_anchor, plugin.spec.manifest.id.as_str())?;
    let path = path.to_str().ok_or_else(|| {
        private_state_refusal(
            plugin.spec.manifest.id.as_str(),
            &path,
            "path is not valid UTF-8",
        )
    })?;
    env.insert(PLUGIN_STATE_DIR_ENV.to_string(), path.to_string());
    Ok(env)
}

fn resolve_env_with(
    plugin: &LoadedPlugin,
    mut resolve: impl FnMut(&SecretRef, &str) -> Option<String>,
) -> Result<BTreeMap<String, String>, SpecProblem> {
    let id = plugin.spec.manifest.id.as_str();
    let consumer = format!("plugin:{id}");
    let mut env = if plugin.spec.trusted_native {
        std::env::vars_os()
            .filter_map(
                |(key, value)| match (key.into_string(), value.into_string()) {
                    (Ok(key), Ok(value)) => Some((key, value)),
                    _ => None,
                },
            )
            .collect()
    } else {
        BTreeMap::new()
    };
    for (key, raw) in &plugin.spec.env {
        let reference = SecretRef::parse(raw, BareAs::Literal);
        if reference.is_literal() {
            env.insert(key.clone(), raw.clone());
            continue;
        }
        if plugin.dir.is_some() {
            return Err(directory_refusal(id, key, &reference));
        }
        if !reference.is_configured() {
            return Err(empty_refusal(id, key, &reference));
        }
        match resolve(&reference, &consumer) {
            Some(value) => {
                env.insert(key.clone(), value);
            }
            None => return Err(unresolved_refusal(id, key, &reference)),
        }
    }
    Ok(env)
}

fn directory_refusal(plugin: &str, key: &str, reference: &SecretRef) -> SpecProblem {
    SpecProblem {
        plugin: plugin.to_string(),
        problem: format!(
            "refused: env {key}: secret reference {} in plugin.toml is not allowed; only a [[plugins]] entry in your config may use keyring:/env:/file: references. Move the env value to a [[plugins]] entry; plugin not started",
            reference.audit_name()
        ),
    }
}

fn empty_refusal(plugin: &str, key: &str, reference: &SecretRef) -> SpecProblem {
    SpecProblem {
        plugin: plugin.to_string(),
        problem: format!(
            "refused: env {key}: secret reference {} has an empty name; set the variable/account/path or remove the entry; plugin not started",
            reference.audit_name()
        ),
    }
}

fn unresolved_refusal(plugin: &str, key: &str, reference: &SecretRef) -> SpecProblem {
    SpecProblem {
        plugin: plugin.to_string(),
        problem: format!(
            "refused: env {key}: secret reference {} did not resolve (missing, empty, or keyring unavailable); plugin not started",
            reference.audit_name()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::plugin_api::PluginSpec;

    fn make_plugin(env: &[(&str, &str)], dir: Option<std::path::PathBuf>) -> LoadedPlugin {
        let mut spec: PluginSpec = toml::from_str(include_str!(
            "../../../../examples/plugins/hello/plugin.toml"
        ))
        .unwrap();
        spec.env = env
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        LoadedPlugin {
            spec,
            dir,
            trusted_native_ignored: false,
        }
    }

    #[test]
    fn literals_are_passed_verbatim_and_refs_use_the_plugin_consumer() {
        let plugin = make_plugin(
            &[
                ("RUST_LOG", "info"),
                ("EMPTY", ""),
                ("SPACED", "  spaced  "),
                ("X", "env:FOO"),
            ],
            None,
        );
        let mut seen = None;
        let resolved = resolve_env_with(&plugin, |reference, consumer| {
            seen = Some((reference.audit_name(), consumer.to_string()));
            Some("resolved-value".into())
        })
        .unwrap();
        assert_eq!(resolved["RUST_LOG"], "info");
        assert_eq!(resolved["EMPTY"], "");
        assert_eq!(resolved["SPACED"], "  spaced  ");
        assert_eq!(resolved["X"], "resolved-value");
        assert_eq!(seen, Some(("env:FOO".into(), "plugin:hello".into())));
    }

    #[test]
    fn failed_and_empty_references_refuse_without_disclosing_values() {
        let sentinel = "PLAINTEXT-SENTINEL";
        let plugin = make_plugin(&[("API_TOKEN", "env:FOO"), ("OTHER", sentinel)], None);
        let result = resolve_env_with(&plugin, |_, _| None);
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("unresolved reference was accepted"),
        };
        assert_eq!(error.plugin, "hello");
        assert!(error.problem.contains("API_TOKEN"));
        assert!(error.problem.contains("env:FOO"));
        assert!(!error.problem.contains(sentinel));

        let empty = make_plugin(&[("API_TOKEN", "env:")], None);
        let mut called = false;
        let empty_result = resolve_env_with(&empty, |_, _| {
            called = true;
            Some("unexpected".into())
        });
        assert!(!called);
        let Err(empty_problem) = empty_result else {
            panic!("empty reference operand was accepted")
        };
        assert!(empty_problem.problem.contains("API_TOKEN"));
        assert!(empty_problem.problem.contains("env:"));
        assert!(empty_problem.problem.contains("empty"));
    }

    #[test]
    fn directory_manifest_references_refuse_without_calling_resolver() {
        let plugin = make_plugin(&[("TOKEN", "keyring:work")], Some("plugin-dir".into()));
        let mut called = false;
        let result = resolve_env_with(&plugin, |_, _| {
            called = true;
            Some("unexpected".into())
        });
        assert!(result.is_err());
        assert!(!called);
        let Err(problem) = result else {
            unreachable!("directory manifest reference must be refused")
        };
        assert_eq!(problem.plugin, "hello");
        assert!(problem.problem.contains("TOKEN"));
        assert!(problem.problem.contains("keyring:work"));
        assert!(problem.problem.contains("plugin.toml"));
        assert!(problem.problem.contains("[[plugins]]"));
    }

    #[test]
    fn real_resolver_reads_environment_and_keyring_fails_closed() {
        let _env = thegn_core::testenv::EnvGuard::set(&[("THEGN_B42_PLUGIN_ENV_FIXTURE", "v1")]);
        let plugin = make_plugin(&[("TOKEN", "env:THEGN_B42_PLUGIN_ENV_FIXTURE")], None);
        assert_eq!(resolve_env(&plugin).unwrap()["TOKEN"], "v1");

        let keyring = make_plugin(&[("TOKEN", "keyring:thegn-b42-no-resolver")], None);
        assert!(resolve_env(&keyring).is_err());
    }

    #[test]
    fn trusted_native_plugin_inherits_the_ambient_environment() {
        let _env = thegn_core::testenv::EnvGuard::set(&[
            ("THEGN_B53_PLUGIN_SECRET", "ambient"),
            ("THEGN_B53_PLUGIN_OVERRIDE", "ambient"),
        ]);
        let mut plugin = make_plugin(&[("THEGN_B53_PLUGIN_OVERRIDE", "configured")], None);
        plugin.spec.trusted_native = true;

        let resolved = resolve_env(&plugin).unwrap();
        assert_eq!(resolved["THEGN_B53_PLUGIN_SECRET"], "ambient");
        assert_eq!(resolved["THEGN_B53_PLUGIN_OVERRIDE"], "configured");
    }

    #[test]
    fn untrusted_plugin_map_carries_only_configured_entries() {
        let _env = thegn_core::testenv::EnvGuard::set(&[("THEGN_B53_PLUGIN_SECRET", "ambient")]);
        let plugin = make_plugin(&[("PLUGIN_SETTING", "configured")], None);

        let resolved = resolve_env(&plugin).unwrap();
        assert_eq!(
            resolved,
            BTreeMap::from([("PLUGIN_SETTING".into(), "configured".into())])
        );
        assert!(!resolved.contains_key("THEGN_B53_PLUGIN_SECRET"));
    }

    #[test]
    fn state_dir_is_private_and_per_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path().join("state");

        let alpha = ensure_plugin_state_dir(&anchor, "alpha").unwrap();
        let beta = ensure_plugin_state_dir(&anchor, "beta").unwrap();
        let alpha_again = ensure_plugin_state_dir(&anchor, "alpha").unwrap();

        assert_eq!(alpha, anchor.join("plugins/alpha"));
        assert_eq!(beta, anchor.join("plugins/beta"));
        assert_ne!(alpha, beta);
        assert!(alpha.is_dir());
        assert!(beta.is_dir());
        assert_eq!(alpha_again, alpha);
    }

    #[test]
    fn state_dir_refuses_dot_ids_without_touching_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path().join("state");

        for id in ["", ".", ".."] {
            let Err(problem) = ensure_plugin_state_dir(&anchor, id) else {
                panic!("invalid plugin id {id:?} was accepted")
            };
            assert!(problem.problem.contains("private state directory"));
            assert_eq!(problem.plugin, id);
            assert!(!anchor.exists());
        }
    }

    #[test]
    fn state_dir_refuses_a_symlinked_leaf() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path().join("state");
        let plugins = anchor.join("plugins");
        let other_dir = tmp.path().join("other");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::create_dir(&other_dir).unwrap();
        let link = plugins.join("gamma");
        if let Err(error) = thegn_core::fsperm::create_symlink(&other_dir, &link, true) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("could not create symlink fixture: {error}");
        }

        let Err(problem) = ensure_plugin_state_dir(&anchor, "gamma") else {
            panic!("symlinked plugin state directory was accepted")
        };

        assert!(problem.problem.contains("private state directory"));
        assert!(std::fs::read_dir(&other_dir).unwrap().next().is_none());
    }

    #[test]
    fn launch_env_sets_the_state_dir_and_overrides_a_configured_value() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = tmp.path().join("state");
        let plugin = make_plugin(
            &[
                (PLUGIN_STATE_DIR_ENV, "/elsewhere"),
                ("PLUGIN_SETTING", "configured"),
            ],
            None,
        );

        let resolved = resolve_launch_env(&plugin, &anchor).unwrap();

        assert_eq!(resolved["PLUGIN_SETTING"], "configured");
        assert_eq!(
            resolved[PLUGIN_STATE_DIR_ENV],
            anchor.join("plugins/hello").to_str().unwrap()
        );
    }
}
