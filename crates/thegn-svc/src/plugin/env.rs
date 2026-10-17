//! Resolve environment values for a plugin immediately before it is started.
//!
//! Config-declared plugins may use the same typed secret references as other
//! host consumers. A plugin directory manifest cannot access the user's
//! secret broker.

use std::collections::BTreeMap;

use thegn_core::secretref::{BareAs, SecretRef};

use super::{LoadedPlugin, SpecProblem};

/// Resolve a config-declared plugin's environment using the installed broker.
pub fn resolve_env(plugin: &LoadedPlugin) -> Result<BTreeMap<String, String>, SpecProblem> {
    resolve_env_with(plugin, crate::secret::resolve_ref)
}

fn resolve_env_with(
    plugin: &LoadedPlugin,
    mut resolve: impl FnMut(&SecretRef, &str) -> Option<String>,
) -> Result<BTreeMap<String, String>, SpecProblem> {
    let id = plugin.spec.manifest.id.as_str();
    let consumer = format!("plugin:{id}");
    let mut env = BTreeMap::new();
    for (key, raw) in &plugin.spec.env {
        let reference = SecretRef::parse(raw, BareAs::Literal);
        if reference.is_literal() {
            env.insert(key.clone(), raw.clone());
            continue;
        }
        if plugin.dir.is_some() {
            return Err(refusal(id, key, &reference));
        }
        if !reference.is_configured() {
            return Err(refusal(id, key, &reference));
        }
        match resolve(&reference, &consumer) {
            Some(value) => {
                env.insert(key.clone(), value);
            }
            None => return Err(refusal(id, key, &reference)),
        }
    }
    Ok(env)
}

fn refusal(plugin: &str, key: &str, reference: &SecretRef) -> SpecProblem {
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
    }

    #[test]
    fn real_resolver_reads_environment_and_keyring_fails_closed() {
        let _env = thegn_core::testenv::EnvGuard::set(&[("THEGN_B42_PLUGIN_ENV_FIXTURE", "v1")]);
        let plugin = make_plugin(&[("TOKEN", "env:THEGN_B42_PLUGIN_ENV_FIXTURE")], None);
        assert_eq!(resolve_env(&plugin).unwrap()["TOKEN"], "v1");

        let keyring = make_plugin(&[("TOKEN", "keyring:thegn-b42-no-resolver")], None);
        assert!(resolve_env(&keyring).is_err());
    }
}
