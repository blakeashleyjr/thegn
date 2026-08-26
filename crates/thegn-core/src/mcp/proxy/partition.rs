//! Partitioning (THE-49): scope-key derivation + placeholder expansion.
//!
//! A `scope = "global" | "workspace" | "worktree"` upstream runs one instance
//! per scope key, and `{workspace}`/`{worktree}`/`{repo_root}`/`{branch}`
//! placeholders in its env/args expand from the connecting shim's worktree
//! context. This is what turns a generic memory MCP server into a per-project
//! memory namespace with no vendor knowledge in thegn.
//!
//! **Partition leakage is a correctness bug, not a degradation**: a connection
//! whose context cannot satisfy a scope (no workspace for a workspace-scoped
//! upstream) or cannot resolve a placeholder has that upstream *withheld* with
//! an inspectable reason — never served a shared instance or launched with a
//! literal `{...}` / an empty expansion.

use super::reconcile::InstanceSpec;
use crate::mcp::config::{McpServerConfig, ProxyScope};
use std::collections::BTreeMap;

/// The connecting shim's worktree identity, resolved from its cwd by the host.
/// Any field may be absent (a shim started outside a registered worktree).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeContext {
    pub workspace: Option<String>,
    pub worktree: Option<String>,
    pub repo_root: Option<String>,
    pub branch: Option<String>,
}

impl WorktreeContext {
    /// The value for a placeholder token (`workspace`, `worktree`, …), or `None`
    /// if this context cannot supply it (⇒ the upstream is withheld).
    fn value(&self, token: &str) -> Option<&str> {
        match token {
            "workspace" => self.workspace.as_deref(),
            "worktree" => self.worktree.as_deref(),
            "repo_root" => self.repo_root.as_deref(),
            "branch" => self.branch.as_deref(),
            _ => None,
        }
    }
}

/// Why an upstream is withheld from a connection — surfaced verbatim in
/// `mcp_proxy.status` and as the reason its tools are absent from `tools/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    pub reason: String,
}

impl Withheld {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Withheld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Derive the partition key for an upstream at `scope` given a connection
/// context. `global` is always `"global"`; the scoped kinds require the
/// corresponding context field or the upstream is withheld.
pub fn partition_key(scope: ProxyScope, ctx: &WorktreeContext) -> Result<String, Withheld> {
    match scope {
        ProxyScope::Global => Ok("global".to_string()),
        ProxyScope::Workspace => match &ctx.workspace {
            Some(w) => Ok(format!("workspace:{w}")),
            None => Err(Withheld::new(
                "workspace-scoped upstream withheld: this connection is outside any \
                 registered workspace (no {workspace} context)",
            )),
        },
        ProxyScope::Worktree => match &ctx.worktree {
            Some(w) => Ok(format!("worktree:{w}")),
            None => Err(Withheld::new(
                "worktree-scoped upstream withheld: this connection is outside any \
                 registered worktree (no {worktree} context)",
            )),
        },
    }
}

/// Expand `{workspace}`/`{worktree}`/`{repo_root}`/`{branch}` in `template`.
///
/// - A recognised placeholder the context can supply is substituted.
/// - A recognised placeholder the context cannot supply ⇒ `Withheld` (never an
///   empty expansion).
/// - An unrecognised `{token}` ⇒ `Withheld` (never a literal brace reaching the
///   upstream — a config typo must not silently launch a mis-templated server).
/// - A literal `{{`/`}}` escapes to a single brace.
pub fn expand_placeholders(template: &str, ctx: &WorktreeContext) -> Result<String, Withheld> {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                out.push('{');
                i += 2;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                out.push('}');
                i += 2;
            }
            b'{' => {
                let Some(end) = template[i + 1..].find('}') else {
                    return Err(Withheld::new(format!(
                        "unterminated placeholder in {template:?}"
                    )));
                };
                let token = &template[i + 1..i + 1 + end];
                match ctx.value(token) {
                    Some(v) => out.push_str(v),
                    None if is_known_placeholder(token) => {
                        return Err(Withheld::new(format!(
                            "placeholder {{{token}}} has no value in this connection's \
                             context — upstream withheld rather than launched empty"
                        )));
                    }
                    None => {
                        return Err(Withheld::new(format!(
                            "unknown placeholder {{{token}}} (known: {{workspace}}, \
                             {{worktree}}, {{repo_root}}, {{branch}})"
                        )));
                    }
                }
                i += 1 + end + 1;
            }
            b'}' => {
                return Err(Withheld::new(format!(
                    "unbalanced }} in {template:?} (use }}}} for a literal brace)"
                )));
            }
            _ => {
                // Copy one UTF-8 char (bytes[i] is a char boundary here).
                let ch_len = utf8_len(bytes[i]);
                out.push_str(&template[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    Ok(out)
}

fn is_known_placeholder(token: &str) -> bool {
    matches!(token, "workspace" | "worktree" | "repo_root" | "branch")
}

/// UTF-8 lead-byte length (1..=4).
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Resolve a declared server into the concrete [`InstanceSpec`] for one
/// connection: derive the partition key, and expand placeholders in every
/// `args` entry and every `env` **value** (env keys are literal). Returns the
/// [`Withheld`] reason on the first unsatisfiable scope/placeholder.
///
/// Note the env values carried here are still secret *refs* (`env:`/`file:`/
/// `keyring:`), not resolved secrets — placeholder expansion operates on the
/// ref text; the host resolves the ref to a value only at spawn.
pub fn expand_spec(
    name: &str,
    srv: &McpServerConfig,
    ctx: &WorktreeContext,
) -> Result<InstanceSpec, Withheld> {
    let exposure = srv.exposure();
    let key = partition_key(exposure.scope, ctx)?;

    let mut argv = Vec::with_capacity(srv.command.len() + srv.args.len());
    for a in srv.command.iter().chain(srv.args.iter()) {
        argv.push(expand_placeholders(a, ctx)?);
    }

    let mut env = BTreeMap::new();
    for (k, v) in &srv.env {
        env.insert(k.clone(), expand_placeholders(v, ctx)?);
    }

    Ok(InstanceSpec {
        upstream: name.to_string(),
        partition_key: key,
        argv,
        env,
        exposure: exposure.tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::McpProxyExposure;

    fn ctx() -> WorktreeContext {
        WorktreeContext {
            workspace: Some("acme".into()),
            worktree: Some("feature-x".into()),
            repo_root: Some("/repos/acme".into()),
            branch: Some("tg/feature-x".into()),
        }
    }

    // -- partition_key -------------------------------------------------------

    #[test]
    fn global_scope_shares_one_key() {
        assert_eq!(
            partition_key(ProxyScope::Global, &WorktreeContext::default()).unwrap(),
            "global"
        );
        assert_eq!(partition_key(ProxyScope::Global, &ctx()).unwrap(), "global");
    }

    #[test]
    fn workspace_scope_keys_by_workspace() {
        assert_eq!(
            partition_key(ProxyScope::Workspace, &ctx()).unwrap(),
            "workspace:acme"
        );
    }

    #[test]
    fn worktree_scope_keys_by_worktree() {
        assert_eq!(
            partition_key(ProxyScope::Worktree, &ctx()).unwrap(),
            "worktree:feature-x"
        );
    }

    #[test]
    fn scoped_without_context_is_withheld() {
        let empty = WorktreeContext::default();
        let w = partition_key(ProxyScope::Workspace, &empty).unwrap_err();
        assert!(w.reason.contains("workspace-scoped"), "{}", w.reason);
        let w = partition_key(ProxyScope::Worktree, &empty).unwrap_err();
        assert!(w.reason.contains("worktree-scoped"), "{}", w.reason);
    }

    // -- expand_placeholders -------------------------------------------------

    #[test]
    fn expands_each_placeholder() {
        let c = ctx();
        assert_eq!(
            expand_placeholders("mem-{workspace}", &c).unwrap(),
            "mem-acme"
        );
        assert_eq!(
            expand_placeholders("{repo_root}/.mem/{branch}.db", &c).unwrap(),
            "/repos/acme/.mem/tg/feature-x.db"
        );
        assert_eq!(
            expand_placeholders("wt={worktree}", &c).unwrap(),
            "wt=feature-x"
        );
    }

    #[test]
    fn no_placeholders_is_identity() {
        assert_eq!(
            expand_placeholders("plain-value", &ctx()).unwrap(),
            "plain-value"
        );
    }

    #[test]
    fn missing_context_value_is_withheld_never_empty() {
        let c = WorktreeContext {
            workspace: None,
            ..ctx()
        };
        let w = expand_placeholders("mem-{workspace}", &c).unwrap_err();
        assert!(w.reason.contains("{workspace}"), "{}", w.reason);
        assert!(w.reason.contains("withheld"), "{}", w.reason);
    }

    #[test]
    fn unknown_placeholder_is_withheld_never_literal() {
        let w = expand_placeholders("x-{bogus}", &ctx()).unwrap_err();
        assert!(w.reason.contains("unknown placeholder"), "{}", w.reason);
        assert!(w.reason.contains("{bogus}"), "{}", w.reason);
    }

    #[test]
    fn escaped_braces_are_literal() {
        assert_eq!(
            expand_placeholders("literal {{workspace}} kept", &ctx()).unwrap(),
            "literal {workspace} kept"
        );
        assert_eq!(expand_placeholders("{{}}", &ctx()).unwrap(), "{}");
    }

    #[test]
    fn unterminated_and_unbalanced_braces_are_withheld() {
        assert!(expand_placeholders("open {workspace", &ctx()).is_err());
        assert!(expand_placeholders("stray } here", &ctx()).is_err());
    }

    #[test]
    fn utf8_is_preserved_around_placeholders() {
        let c = ctx();
        assert_eq!(
            expand_placeholders("café-{workspace}-☕", &c).unwrap(),
            "café-acme-☕"
        );
    }

    // -- expand_spec ---------------------------------------------------------

    fn server(scope: ProxyScope) -> McpServerConfig {
        McpServerConfig {
            command: vec!["memory-server".into()],
            args: vec!["--db".into(), "{repo_root}/.mem.db".into()],
            env: {
                let mut m = BTreeMap::new();
                m.insert("NAMESPACE".into(), "{workspace}".into());
                m.insert("KEY".into(), "keyring:mem-token".into());
                m
            },
            proxy: Some(McpProxyExposure {
                tools: vec!["*".into()],
                scope,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn expand_spec_builds_instance_with_expanded_argv_and_env() {
        let spec = expand_spec("mem", &server(ProxyScope::Workspace), &ctx()).unwrap();
        assert_eq!(spec.upstream, "mem");
        assert_eq!(spec.partition_key, "workspace:acme");
        assert_eq!(spec.argv, ["memory-server", "--db", "/repos/acme/.mem.db"]);
        assert_eq!(spec.env["NAMESPACE"], "acme");
        // Secret refs pass through placeholder expansion untouched (still a ref).
        assert_eq!(spec.env["KEY"], "keyring:mem-token");
        assert_eq!(spec.exposure, ["*"]);
    }

    #[test]
    fn expand_spec_withholds_on_unsatisfiable_scope() {
        let empty = WorktreeContext::default();
        let err = expand_spec("mem", &server(ProxyScope::Workspace), &empty).unwrap_err();
        assert!(err.reason.contains("workspace-scoped"), "{}", err.reason);
    }

    #[test]
    fn expand_spec_withholds_on_unresolvable_placeholder() {
        let c = WorktreeContext {
            repo_root: None,
            ..ctx()
        };
        // Global scope resolves, but the {repo_root} arg cannot expand.
        let err = expand_spec("mem", &server(ProxyScope::Global), &c).unwrap_err();
        assert!(err.reason.contains("{repo_root}"), "{}", err.reason);
    }

    #[test]
    fn withheld_display_is_the_reason() {
        let w = Withheld::new("nope");
        assert_eq!(w.to_string(), "nope");
    }
}
