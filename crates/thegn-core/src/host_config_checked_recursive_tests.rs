use super::*;
use crate::plugin_api::{
    ApiVersion, CadenceHint, Contribution, ContributionId, ExtensionPoint, PluginId,
    PluginManifest, PluginMode, PluginSpec,
};
use crate::sandbox_build::SandboxBuild;

fn nested(depth: usize) -> Value {
    let mut value = Value::Null;
    for _ in 0..depth {
        value = Value::Array(vec![value]);
    }
    value
}

fn provider(cfg: &mut Config, value: Value) {
    let mut provider = crate::config_model_proxy::ProviderEntry::default();
    provider.defaults.insert("value".into(), value);
    cfg.model_proxy.providers.push(provider);
}

fn plugin(cfg: &mut Config, value: Value) {
    cfg.plugins.push(PluginSpec {
        manifest: PluginManifest {
            id: PluginId::new("checked-private-fixture"),
            name: "Checked fixture".into(),
            version: "1.0.0".into(),
            api: ApiVersion::new(0, 1, 0),
            capabilities: Vec::new(),
            contributions: vec![Contribution {
                id: ContributionId::new("caps"),
                extension_point: ExtensionPoint::CiProvider,
                label: "Fixture".into(),
                surface: None,
                cadence: CadenceHint::OnDemand,
                metadata: Default::default(),
                caps: value,
                chord: None,
            }],
        },
        command: vec!["never-run-checked-fixture".into()],
        cwd: String::new(),
        env: Default::default(),
        timeout_secs: 30,
        scopes: Vec::new(),
        mode: PluginMode::OneShot,
        enabled: false,
    });
}

#[test]
fn both_recursive_families_use_actual_flattened_depth_before_serialization() {
    for insert in [provider as fn(&mut Config, Value), plugin] {
        let mut cfg = config();
        insert(&mut cfg, nested(MAX_DEPTH - 5));
        assert!(admit(&cfg).is_ok(), "inclusive full structural admission");
        if cfg.plugins.is_empty() {
            assert!(compose_host_definitions_checked(&cfg, &empty_snapshot()).is_ok());
        } else {
            // Existing ApiVersion serializes a string but derives an object
            // schema. Preserve that legacy schema refusal; this fixture proves
            // plugin depth admission, not a repair of its schema declaration.
            let value = serde_json::to_value(&cfg).unwrap();
            assert!(!config_validate::validate_config_schema_value(&value).is_empty());
            assert_eq!(
                compose_host_definitions_checked(&cfg, &empty_snapshot()).unwrap_err(),
                HostCompositionError::InvalidFinalConfig
            );
        }
        let mut invalid = config();
        insert(&mut invalid, nested(MAX_DEPTH - 5 + 1));
        let (result, events) = semantic_observation::capture(|| {
            compose_host_definitions_checked(&invalid, &empty_snapshot())
        });
        assert_eq!(result.unwrap_err(), HostCompositionError::Bounds);
        assert!(events.is_empty(), "no Config clone or semantic entry");
    }
}

// This fixture owns recursive input beyond the API's admitted depth. Its Drop
// empties those exact chains iteratively even if the checked call/assertion
// unwinds; no leak or recursive Config destruction is used as cleanup.
struct DeepFixture(Config);

fn drain_chain(mut value: Value) -> usize {
    let mut depth = 0;
    while let Value::Array(mut entries) = value {
        let Some(next) = entries.pop() else { break };
        value = next;
        depth += 1;
    }
    depth
}

impl Drop for DeepFixture {
    fn drop(&mut self) {
        for provider in &mut self.0.model_proxy.providers {
            for value in provider.defaults.values_mut() {
                drain_chain(std::mem::replace(value, Value::Null));
            }
        }
        for plugin in &mut self.0.plugins {
            for contribution in &mut plugin.manifest.contributions {
                drain_chain(std::mem::replace(&mut contribution.caps, Value::Null));
            }
        }
    }
}

#[test]
fn deep_rejected_input_stays_borrowed_and_is_disposed_by_owned_fixture() {
    for insert in [provider as fn(&mut Config, Value), plugin] {
        let mut cfg = config();
        insert(&mut cfg, nested(4096));
        let mut fixture = DeepFixture(cfg);
        let cfg = &mut fixture.0;
        let original = if cfg.plugins.is_empty() {
            &cfg.model_proxy.providers[0].defaults["value"]
        } else {
            &cfg.plugins[0].manifest.contributions[0].caps
        } as *const Value;
        let (result, events) = semantic_observation::capture(|| {
            compose_host_definitions_checked(cfg, &empty_snapshot())
        });
        // Remove and iteratively dispose of the exact caller-owned chain BEFORE
        // asserting. A failed assertion must not recursively drop this fixture.
        let same_address = if cfg.plugins.is_empty() {
            std::ptr::eq(original, &cfg.model_proxy.providers[0].defaults["value"])
        } else {
            std::ptr::eq(original, &cfg.plugins[0].manifest.contributions[0].caps)
        };
        let value = if cfg.plugins.is_empty() {
            cfg.model_proxy.providers[0]
                .defaults
                .remove("value")
                .unwrap()
        } else {
            std::mem::replace(
                &mut cfg.plugins[0].manifest.contributions[0].caps,
                Value::Null,
            )
        };
        let depth = drain_chain(value);
        assert!(same_address);
        assert_eq!(depth, 4096);
        assert_eq!(result.unwrap_err(), HostCompositionError::Bounds);
        assert!(events.is_empty());
    }
}

#[test]
fn recursive_node_budget_is_shared_across_both_config_families() {
    let part = Value::Array(vec![Value::Array(vec![Value::Null; 1023]); 32]);
    let mut cfg = config();
    provider(&mut cfg, part.clone());
    assert!(preflight_recursive_values(&cfg).is_ok());
    plugin(&mut cfg, part);
    assert_eq!(
        preflight_recursive_values(&cfg),
        Err(HostCompositionError::Bounds)
    );
    let (result, events) =
        semantic_observation::capture(|| compose_host_definitions_checked(&cfg, &empty_snapshot()));
    assert_eq!(result.unwrap_err(), HostCompositionError::Bounds);
    assert!(events.is_empty());
}

#[test]
fn null_plugin_caps_also_consume_preflight_work() {
    let mut cfg = config();
    plugin(&mut cfg, Value::Null);
    let contribution = cfg.plugins[0].manifest.contributions[0].clone();
    cfg.plugins[0].manifest.contributions = vec![contribution; MAX_CONTAINER];
    let entry = cfg.plugins[0].clone();
    cfg.plugins = vec![entry.clone(); MAX_NODES / MAX_CONTAINER];
    assert!(preflight_recursive_values(&cfg).is_ok());
    // The ordinary full-tree/byte gate would reject sooner; this control proves
    // the initial borrowed scan itself cannot visit uncharged null caps forever.
    cfg.plugins.push(entry);
    assert_eq!(
        preflight_recursive_values(&cfg),
        Err(HostCompositionError::Bounds)
    );
    let (result, events) =
        semantic_observation::capture(|| compose_host_definitions_checked(&cfg, &empty_snapshot()));
    assert_eq!(result.unwrap_err(), HostCompositionError::Bounds);
    assert!(events.is_empty());
}

#[test]
fn skipped_build_and_issue_restriction_survive_and_share_the_byte_budget() {
    let build = SandboxBuild {
        dockerfile: "/private-fixture/Dockerfile".into(),
        context: "/private-fixture/context".into(),
        target: Some("fixture".into()),
        args: [("FIXTURE".into(), "literal".into())].into(),
    };
    let mut cfg = config();
    cfg.sandbox.build = Some(build.clone());
    cfg.issues.accounts_restricted = true;
    let result = compose_host_definitions_checked(&cfg, &empty_snapshot()).unwrap();
    assert!(result.config().sandbox.build.as_ref() == Some(&build));
    assert!(result.config().issues.accounts_restricted);
    assert!(cfg.sandbox.build.as_ref() == Some(&build));

    let bytes = serde_json::to_vec(&build).unwrap().len();
    let mut at_limit = super::bounds::config_bytes(MAX_CONFIG_BYTES - bytes);
    at_limit.sandbox.build = Some(build.clone());
    assert!(compose_host_definitions_checked(&at_limit, &empty_snapshot()).is_ok());
    let mut over = super::bounds::config_bytes(MAX_CONFIG_BYTES - bytes + 1);
    over.sandbox.build = Some(build);
    assert_bounds_before_semantics(over);
}

#[test]
fn skipped_build_structural_refusal_does_not_clone_or_discard_input() {
    let mut cfg = config();
    cfg.sandbox.build = Some(SandboxBuild {
        dockerfile: "x".repeat(MAX_STRING_BYTES),
        ..Default::default()
    });
    assert!(compose_host_definitions_checked(&cfg, &empty_snapshot()).is_ok());
    cfg.sandbox.build.as_mut().unwrap().dockerfile.push('x');
    let (result, events) =
        semantic_observation::capture(|| compose_host_definitions_checked(&cfg, &empty_snapshot()));
    assert_eq!(result.unwrap_err(), HostCompositionError::Bounds);
    assert!(events.is_empty());
    assert_eq!(
        cfg.sandbox.build.as_ref().unwrap().dockerfile.len(),
        MAX_STRING_BYTES + 1
    );
}
