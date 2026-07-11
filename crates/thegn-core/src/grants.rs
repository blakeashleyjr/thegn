//! Capability grants — a glob-scoped, least-privilege permission model.
//!
//! Borrowed from Zed's extension model: every side-effecting acquisition/launch
//! of a *user-declared* tool (an MCP server, a helper binary) is gated by a
//! capability the user declared for it. A [`Grant`] pairs a [`GrantKind`]
//! (`process:exec`, `download_file`, `npm:install`, `cargo:install`) with a glob
//! **scope** restricting what that capability may touch; [`Grants::allows`]
//! answers whether a concrete [`Action`] is permitted.
//!
//! This is a declarative guardrail at thegn's own boundary — not an OS
//! sandbox (that is the sandbox layer's job) — so it stays pure and unit-tested.
//! First-party tools (the managed pi, the debugger) are implicitly trusted and
//! do not consult grants; grants gate the tools *users* declare.

use serde::{Deserialize, Serialize};

/// The kind of side effect a grant authorizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// Execute a command (scope matches the command).
    Exec,
    /// Download from a host (scope matches the host).
    Download,
    /// `npm install` a package (scope matches the package).
    NpmInstall,
    /// `cargo install` a crate (scope matches the crate).
    CargoInstall,
}

impl GrantKind {
    /// Parse the manifest token (`process:exec`, `download_file`,
    /// `npm:install`, `cargo:install`).
    pub fn parse(s: &str) -> Option<GrantKind> {
        match s {
            "process:exec" => Some(GrantKind::Exec),
            "download_file" => Some(GrantKind::Download),
            "npm:install" => Some(GrantKind::NpmInstall),
            "cargo:install" => Some(GrantKind::CargoInstall),
            _ => None,
        }
    }

    /// The canonical manifest token.
    pub fn token(self) -> &'static str {
        match self {
            GrantKind::Exec => "process:exec",
            GrantKind::Download => "download_file",
            GrantKind::NpmInstall => "npm:install",
            GrantKind::CargoInstall => "cargo:install",
        }
    }
}

/// One declared capability: a kind + a glob scope. Serialized as
/// `{ kind = "npm:install", scope = "@modelcontextprotocol/*" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Grant {
    pub kind: String,
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "**".to_string()
}

/// A concrete side effect to authorize against a [`Grants`] set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action<'a> {
    Exec(&'a str),
    Download(&'a str),
    Npm(&'a str),
    Cargo(&'a str),
}

impl Action<'_> {
    fn kind(&self) -> GrantKind {
        match self {
            Action::Exec(_) => GrantKind::Exec,
            Action::Download(_) => GrantKind::Download,
            Action::Npm(_) => GrantKind::NpmInstall,
            Action::Cargo(_) => GrantKind::CargoInstall,
        }
    }
    fn resource(&self) -> &str {
        match self {
            Action::Exec(r) | Action::Download(r) | Action::Npm(r) | Action::Cargo(r) => r,
        }
    }
}

/// A set of declared grants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants(Vec<Grant>);

impl Grants {
    pub fn new(grants: Vec<Grant>) -> Grants {
        Grants(grants)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `action` is permitted: some declared grant has the action's kind
    /// and a scope glob that matches the action's resource.
    pub fn allows(&self, action: &Action) -> bool {
        let want = action.kind();
        self.0.iter().any(|g| {
            GrantKind::parse(&g.kind) == Some(want) && glob_match(&g.scope, action.resource())
        })
    }

    /// The missing-capability message for a refused action.
    pub fn deny_reason(&self, action: &Action) -> String {
        format!(
            "no `{}` grant covers `{}` — declare one under this server's [[..grants]]",
            action.kind().token(),
            action.resource()
        )
    }
}

/// Glob match with `*` (matches any run of chars within one `/`-delimited
/// segment) and `**` (matches any run, crossing `/`). A bare `**` matches
/// everything. Operates on bytes — patterns/resources here are ASCII
/// (package names, hosts, commands), and `/` (0x2f) never appears inside a
/// UTF-8 multibyte sequence, so segment detection is safe.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    m(pattern.as_bytes(), value.as_bytes())
}

fn m(p: &[u8], v: &[u8]) -> bool {
    if p.is_empty() {
        return v.is_empty();
    }
    if p.starts_with(b"**") {
        let rest = &p[2..];
        // `**` matches zero chars, or one more char (any, incl. `/`) then retry.
        return m(rest, v) || (!v.is_empty() && m(p, &v[1..]));
    }
    if p[0] == b'*' {
        let rest = &p[1..];
        // `*` matches zero chars, or one more char that is not `/`, then retry.
        return m(rest, v) || (!v.is_empty() && v[0] != b'/' && m(p, &v[1..]));
    }
    !v.is_empty() && v[0] == p[0] && m(&p[1..], &v[1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact_and_wildcards() {
        assert!(glob_match("bugstalker", "bugstalker"));
        assert!(!glob_match("bugstalker", "other"));
        // `*` within a segment.
        assert!(glob_match("@scope/*", "@scope/server-foo"));
        assert!(!glob_match("@scope/*", "@scope/foo/bar")); // `*` does not cross `/`
        assert!(glob_match("server-*", "server-foo"));
        assert!(!glob_match("server-*", "client-foo"));
        // `**` crosses segments.
        assert!(glob_match("**", "anything/at/all"));
        assert!(glob_match("github.com/**", "github.com/owner/repo"));
        assert!(!glob_match("github.com/**", "gitlab.com/owner/repo"));
        assert!(glob_match("**/server-foo", "a/b/server-foo"));
        // mixed
        assert!(glob_match("@scope/**", "@scope/deep/pkg"));
    }

    fn g(kind: &str, scope: &str) -> Grant {
        Grant {
            kind: kind.into(),
            scope: scope.into(),
        }
    }

    #[test]
    fn grant_kind_parse_round_trip() {
        for k in [
            GrantKind::Exec,
            GrantKind::Download,
            GrantKind::NpmInstall,
            GrantKind::CargoInstall,
        ] {
            assert_eq!(GrantKind::parse(k.token()), Some(k));
        }
        assert_eq!(GrantKind::parse("bogus"), None);
    }

    #[test]
    fn allows_matches_kind_and_scope() {
        let grants = Grants::new(vec![
            g("npm:install", "@modelcontextprotocol/*"),
            g("cargo:install", "mcp-*"),
        ]);
        // Matching kind + scope.
        assert!(grants.allows(&Action::Npm("@modelcontextprotocol/server-git")));
        assert!(grants.allows(&Action::Cargo("mcp-foo")));
        // Right resource, wrong kind.
        assert!(!grants.allows(&Action::Cargo("@modelcontextprotocol/server-git")));
        // Right kind, scope miss.
        assert!(!grants.allows(&Action::Npm("@evil/pkg")));
        // No download grant at all.
        assert!(!grants.allows(&Action::Download("github.com")));
        // A broad `**` grant covers anything of its kind.
        let broad = Grants::new(vec![g("download_file", "**")]);
        assert!(broad.allows(&Action::Download("example.com/x/y")));
        // Empty grants deny everything; the reason names the capability.
        let none = Grants::default();
        assert!(!none.allows(&Action::Exec("bs")));
        assert!(
            none.deny_reason(&Action::Exec("bs"))
                .contains("process:exec")
        );
    }

    #[test]
    fn grant_deserializes_with_default_scope() {
        let grants: Vec<Grant> = toml::from_str::<std::collections::BTreeMap<String, Vec<Grant>>>(
            "g = [{ kind = \"npm:install\" }]",
        )
        .unwrap()
        .remove("g")
        .unwrap();
        assert_eq!(grants[0].scope, "**"); // omitted scope defaults to broad
    }
}
