//! Native devcontainer [features](https://containers.dev/implementors/features/)
//! resolver.
//!
//! Features are OCI artifacts (`ghcr.io/devcontainers/features/node:1`) that
//! ship a `devcontainer-feature.json` + an `install.sh`. This module is the
//! **pure planner**: it parses feature refs, resolves the install order
//! (`overrideFeatureInstallOrder` first, then declaration order), maps each
//! feature's options to the uppercased env vars `install.sh` expects, and emits
//! a self-contained install script per feature. The scripts run in the target
//! container through the ordinary [`StepKind::Exec`](crate::envplan::StepKind)
//! provisioning machinery — so the *fetch + install* happens in-container, and
//! everything here (ref parsing, ordering, env mapping, script text) is unit
//! tested without a network or a runtime.
//!
//! Fetch strategy inside the generated script: prefer `oras` (the canonical OCI
//! artifact tool) when present, else a dependency-free `curl` pull against the
//! registry's token/manifest/blob API (works out-of-the-box for `ghcr.io` and
//! other Bearer-token registries).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::devcontainer::DevContainer;
use crate::envplan::{ProvisionStep, StepKind};
use crate::util::sh_quote;

/// One feature to install: its OCI ref + resolved string options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feature {
    /// The full ref as written in devcontainer.json (e.g.
    /// `ghcr.io/devcontainers/features/node:1`).
    pub id: String,
    /// Option id → value (scalars stringified).
    pub options: BTreeMap<String, String>,
}

/// A parsed OCI feature reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRef {
    /// Registry host (e.g. `ghcr.io`).
    pub registry: String,
    /// Repository path (e.g. `devcontainers/features/node`).
    pub repository: String,
    /// Tag (defaults to `latest`).
    pub tag: String,
}

impl FeatureRef {
    /// The last path segment — the feature's short name (`node`).
    pub fn short_name(&self) -> &str {
        self.repository
            .rsplit('/')
            .next()
            .unwrap_or(&self.repository)
    }

    /// A loose identity for `overrideFeatureInstallOrder` matching: the ref
    /// without its tag. So `ghcr.io/x/node:1` and `ghcr.io/x/node:2` and
    /// `ghcr.io/x/node` all match the override entry `ghcr.io/x/node`.
    pub fn untagged(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }
}

/// Parse an OCI feature ref into (registry, repository, tag). A ref whose first
/// path segment has no `.`/`:` (i.e. no registry host) defaults to `ghcr.io`.
pub fn parse_ref(id: &str) -> FeatureRef {
    // Strip a `@sha256:…` digest if present (pin by digest → treat as the tag).
    let (body, digest) = match id.split_once('@') {
        Some((b, d)) => (b, Some(d.to_string())),
        None => (id, None),
    };
    let first_slash = body.find('/');
    let has_registry = first_slash
        .map(|i| {
            let head = &body[..i];
            head.contains('.') || head.contains(':')
        })
        .unwrap_or(false);
    let (registry, rest) = if has_registry {
        let i = first_slash.unwrap();
        (body[..i].to_string(), &body[i + 1..])
    } else {
        ("ghcr.io".to_string(), body)
    };
    // A `:tag` on the LAST path segment (a `:` before the last `/` would be part
    // of a registry port, already split off above).
    let (repository, tag) = match rest.rsplit_once(':') {
        Some((repo, tag)) if !repo.is_empty() => (repo.to_string(), tag.to_string()),
        _ => (rest.to_string(), "latest".to_string()),
    };
    FeatureRef {
        registry,
        repository,
        tag: digest.unwrap_or(tag),
    }
}

/// Collect installable [`Feature`]s from a devcontainer. A feature whose value
/// is `false` is disabled and skipped; an object value supplies options; any
/// other value means "enable, no options".
pub fn features_of(dc: &DevContainer) -> Vec<Feature> {
    dc.features
        .iter()
        .filter_map(|(id, v)| {
            if matches!(v, Value::Bool(false)) {
                return None;
            }
            let options = match v {
                Value::Object(m) => m
                    .iter()
                    .filter_map(|(k, val)| scalar(val).map(|s| (k.clone(), s)))
                    .collect(),
                _ => BTreeMap::new(),
            };
            Some(Feature {
                id: id.clone(),
                options,
            })
        })
        .collect()
}

/// Order features for install: those named in `override_order` first (loose,
/// tag-insensitive match), in that order, then the rest in declaration order.
pub fn install_order(features: &[Feature], override_order: &[String]) -> Vec<Feature> {
    let key = |f: &Feature| parse_ref(&f.id).untagged();
    let want: Vec<String> = override_order
        .iter()
        .map(|s| parse_ref(s).untagged())
        .collect();

    let mut ordered: Vec<Feature> = Vec::new();
    let mut used = vec![false; features.len()];
    for w in &want {
        for (i, f) in features.iter().enumerate() {
            if !used[i] && &key(f) == w {
                ordered.push(f.clone());
                used[i] = true;
            }
        }
    }
    for (i, f) in features.iter().enumerate() {
        if !used[i] {
            ordered.push(f.clone());
        }
    }
    ordered
}

/// The env-var name `install.sh` expects for an option: uppercased, with every
/// non-alphanumeric character replaced by `_`. `version` → `VERSION`,
/// `install-zsh` → `INSTALL_ZSH`.
pub fn option_env_name(opt: &str) -> String {
    opt.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The self-contained install script for one feature (fetch → extract → run
/// `install.sh` with options + standard env). `remote_user` seeds
/// `_REMOTE_USER`/`_CONTAINER_USER` (features personalize for it).
pub fn install_script(f: &Feature, remote_user: &str) -> String {
    let r = parse_ref(&f.id);
    let ref_full = format!("{}/{}:{}", r.registry, r.repository, r.tag);

    // Header defines the shell vars + option/standard env; the fixed body (a
    // plain string, so its literal `{}` regex braces need no escaping) reads
    // them. `oras` if present, else a dependency-free curl token/manifest/blob
    // pull (ghcr-style Bearer auth); the feature tarball is the blob layer.
    let mut s = String::new();
    s.push_str("set -e\n");
    s.push_str(&format!("_sz_ref={}\n", sh_quote(&ref_full)));
    s.push_str(&format!("_sz_reg={}\n", sh_quote(&r.registry)));
    s.push_str(&format!("_sz_repo={}\n", sh_quote(&r.repository)));
    s.push_str(&format!("_sz_tag={}\n", sh_quote(&r.tag)));
    s.push_str(&format!("_sz_name={}\n", sh_quote(r.short_name())));
    for (k, v) in &f.options {
        s.push_str(&format!("export {}={}\n", option_env_name(k), sh_quote(v)));
    }
    s.push_str(&format!("export _REMOTE_USER={}\n", sh_quote(remote_user)));
    s.push_str(&format!(
        "export _CONTAINER_USER={}\n",
        sh_quote(remote_user)
    ));
    s.push_str(FEATURE_INSTALL_BODY);
    s
}

/// The fixed install body (references `$_sz_ref`/`$_sz_reg`/… set by the
/// header). Kept as a plain literal so its embedded regex/glob braces are not
/// treated as format placeholders.
const FEATURE_INSTALL_BODY: &str = r#"_sz_d=$(mktemp -d)
cd "$_sz_d"
if command -v oras >/dev/null 2>&1; then
  oras pull "$_sz_ref" >/dev/null
else
  _sz_tok=$(curl -sSL "https://$_sz_reg/token?scope=repository:$_sz_repo:pull" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
  _sz_auth=""
  [ -n "$_sz_tok" ] && _sz_auth="Authorization: Bearer $_sz_tok"
  _sz_man=$(curl -sSL -H "$_sz_auth" -H "Accept: application/vnd.oci.image.manifest.v1+json" "https://$_sz_reg/v2/$_sz_repo/manifests/$_sz_tag")
  _sz_dig=$(printf '%s' "$_sz_man" | grep -o 'sha256:[a-f0-9]\{64\}' | tail -1)
  curl -sSL -H "$_sz_auth" "https://$_sz_reg/v2/$_sz_repo/blobs/$_sz_dig" -o feature.tgz
fi
tar xzf ./*.tgz 2>/dev/null || tar xzf ./*.tar 2>/dev/null || true
chmod +x ./install.sh 2>/dev/null || true
if [ -f ./install.sh ]; then ./install.sh; else echo "devcontainer feature $_sz_name: no install.sh" >&2; fi
rm -rf "$_sz_d"
"#;

/// The ordered feature-install [`ProvisionStep`]s for a devcontainer, to be
/// appended to the plan (after the toolchain, before the lifecycle commands).
/// Empty when there are no features. `remote_user` seeds the feature env.
pub fn feature_steps(dc: &DevContainer, remote_user: &str) -> Vec<ProvisionStep> {
    let ordered = install_order(&features_of(dc), &dc.override_feature_install_order);
    ordered
        .iter()
        .map(|f| {
            let name = parse_ref(&f.id).short_name().to_string();
            ProvisionStep {
                id: format!("devcontainer.feature.{name}"),
                label: format!("devcontainer feature: {name}"),
                kind: StepKind::Exec(install_script(f, remote_user)),
            }
        })
        .collect()
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::parse;

    #[test]
    fn parse_ref_ghcr_with_tag() {
        let r = parse_ref("ghcr.io/devcontainers/features/node:1");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "devcontainers/features/node");
        assert_eq!(r.tag, "1");
        assert_eq!(r.short_name(), "node");
        assert_eq!(r.untagged(), "ghcr.io/devcontainers/features/node");
    }

    #[test]
    fn parse_ref_defaults_registry_and_tag() {
        // No registry host (first segment has no dot/colon) → ghcr.io, latest.
        let r = parse_ref("devcontainers/features/go");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "devcontainers/features/go");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_ref_registry_with_port() {
        let r = parse_ref("localhost:5000/team/feat:2");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "team/feat");
        assert_eq!(r.tag, "2");
    }

    #[test]
    fn parse_ref_digest_pin() {
        let r = parse_ref("ghcr.io/x/y@sha256:abc");
        assert_eq!(r.repository, "x/y");
        assert_eq!(r.tag, "sha256:abc");
    }

    #[test]
    fn features_of_skips_disabled_and_reads_options() {
        let dc = parse(
            r#"{ "image": "x", "features": {
                "ghcr.io/devcontainers/features/node:1": { "version": "20" },
                "ghcr.io/devcontainers/features/git:1": true,
                "ghcr.io/devcontainers/features/go:1": false
            } }"#,
        )
        .unwrap();
        let feats = features_of(&dc);
        // git (enabled, no opts) + node (version=20); go is disabled.
        assert_eq!(feats.len(), 2);
        let node = feats.iter().find(|f| f.id.contains("node")).unwrap();
        assert_eq!(node.options.get("version").map(String::as_str), Some("20"));
        assert!(feats.iter().all(|f| !f.id.contains("go")));
    }

    #[test]
    fn install_order_honors_override_then_declaration() {
        let feats = vec![
            Feature {
                id: "ghcr.io/x/a:1".into(),
                options: BTreeMap::new(),
            },
            Feature {
                id: "ghcr.io/x/b:1".into(),
                options: BTreeMap::new(),
            },
            Feature {
                id: "ghcr.io/x/c:1".into(),
                options: BTreeMap::new(),
            },
        ];
        // Override puts c first (by untagged id), b explicitly; a is unlisted → last.
        let order = install_order(&feats, &["ghcr.io/x/c".into(), "ghcr.io/x/b".into()]);
        let ids: Vec<_> = order.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["ghcr.io/x/c:1", "ghcr.io/x/b:1", "ghcr.io/x/a:1"]);
    }

    #[test]
    fn install_order_empty_override_is_declaration_order() {
        let feats = vec![
            Feature {
                id: "ghcr.io/x/a:1".into(),
                options: BTreeMap::new(),
            },
            Feature {
                id: "ghcr.io/x/b:1".into(),
                options: BTreeMap::new(),
            },
        ];
        let order = install_order(&feats, &[]);
        assert_eq!(order, feats);
    }

    #[test]
    fn option_env_name_uppercases_and_sanitizes() {
        assert_eq!(option_env_name("version"), "VERSION");
        assert_eq!(option_env_name("install-zsh"), "INSTALL_ZSH");
        assert_eq!(option_env_name("a.b/c"), "A_B_C");
    }

    #[test]
    fn install_script_has_ref_options_and_install_invocation() {
        let f = Feature {
            id: "ghcr.io/devcontainers/features/node:1".into(),
            options: BTreeMap::from([("version".into(), "20".into())]),
        };
        let s = install_script(&f, "vscode");
        assert!(s.contains("ghcr.io/devcontainers/features/node:1"));
        assert!(s.contains("export VERSION=20"));
        assert!(s.contains("export _REMOTE_USER=vscode"));
        assert!(s.contains("./install.sh"));
        // Both fetch strategies present.
        assert!(s.contains("oras pull"));
        assert!(s.contains("/token?scope=repository:"));
        assert!(s.contains("/blobs/"));
    }

    #[test]
    fn feature_steps_are_ordered_and_labeled() {
        let dc = parse(
            r#"{ "image": "x",
                "features": {
                    "ghcr.io/devcontainers/features/node:1": {},
                    "ghcr.io/devcontainers/features/git:1": {}
                },
                "overrideFeatureInstallOrder": ["ghcr.io/devcontainers/features/git"] }"#,
        )
        .unwrap();
        let steps = feature_steps(&dc, "root");
        assert_eq!(steps.len(), 2);
        // git first (override), then node.
        assert_eq!(steps[0].id, "devcontainer.feature.git");
        assert_eq!(steps[1].id, "devcontainer.feature.node");
        assert!(matches!(&steps[0].kind, StepKind::Exec(s) if s.contains("features/git:1")));
    }

    #[test]
    fn no_features_no_steps() {
        let dc = parse(r#"{ "image": "x" }"#).unwrap();
        assert!(feature_steps(&dc, "root").is_empty());
    }

    /// The generated install script must be syntactically valid POSIX sh —
    /// `sh -n` parses without executing (no network/runtime needed), so a
    /// quoting/brace bug in the generator fails here in CI.
    #[test]
    fn install_script_is_valid_shell() {
        let f = Feature {
            id: "ghcr.io/devcontainers/features/node:1".into(),
            options: BTreeMap::from([
                ("version".into(), "20".into()),
                // A value with shell metacharacters must survive quoting.
                ("weird".into(), "a b'c\"$d".into()),
            ]),
        };
        let script = install_script(&f, "vs code");
        let status = std::process::Command::new("sh")
            .arg("-n")
            .arg("-c")
            .arg(&script)
            .status()
            .expect("run sh -n");
        assert!(
            status.success(),
            "generated install script is not valid sh:\n{script}"
        );
    }
}
