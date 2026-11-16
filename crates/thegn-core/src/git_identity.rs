//! The host's git commit identity (`user.name` / `user.email`), resolved on the
//! host and handed to a sandbox as `GIT_AUTHOR_*` / `GIT_COMMITTER_*`.
//!
//! A sandboxed shell often cannot see the identity the host uses: the shared
//! `.git/config` is read-only (commit-only `[sandbox.git]` policy) and the
//! user's GLOBAL gitconfig is unreachable when the sandbox `$HOME` differs. The
//! identity is not a credential, so it rides the env. An explicit host value
//! (`GIT_AUTHOR_NAME` already exported, or already in the sandbox env) wins.
//!
//! Pure core: the git reader and the host env are injected.

/// The effective `user.name` / `user.email` as the host's git sees them
/// (repository config first, then global).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl GitIdentity {
    /// Neither half is known.
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.email.is_none()
    }
}

/// Longest identity value passed through; anything longer is dropped.
pub const MAX_VALUE_LEN: usize = 256;

/// A value is passed through only when it is non-empty, at most
/// [`MAX_VALUE_LEN`] bytes and free of control characters (newlines included).
pub fn valid_value(v: &str) -> bool {
    !v.is_empty() && v.len() <= MAX_VALUE_LEN && !v.chars().any(char::is_control)
}

/// Resolve the identity through `read`, which runs `git <args>` and returns
/// its trimmed stdout (`None` on failure or no value). Invalid values are
/// dropped here, so every consumer gets the validated identity.
pub fn resolve(read: &dyn Fn(&[&str]) -> Option<String>) -> GitIdentity {
    let get = |key: &str| {
        read(&["config", "--get", key])
            .map(|v| v.trim().to_string())
            .filter(|v| valid_value(v))
    };
    GitIdentity {
        name: get("user.name"),
        email: get("user.email"),
    }
}

/// The env pairs that carry `id` into a sandbox: `GIT_AUTHOR_NAME`,
/// `GIT_COMMITTER_NAME` (from the name) and `GIT_AUTHOR_EMAIL`,
/// `GIT_COMMITTER_EMAIL` (from the email). A key already present in `existing`
/// or set in the host env is skipped (an explicit value wins), as is an invalid
/// value.
pub fn identity_env(
    id: &GitIdentity,
    existing: &[(String, String)],
    host_env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (value, keys) in [
        (&id.name, ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"]),
        (&id.email, ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"]),
    ] {
        let Some(value) = value.as_deref().filter(|v| valid_value(v)) else {
            continue;
        };
        for key in keys {
            let explicit = existing.iter().any(|(k, _)| k == key)
                || host_env(key).is_some_and(|v| !v.is_empty());
            if !explicit {
                out.push((key.to_string(), value.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(name: Option<&str>, email: Option<&str>) -> impl Fn(&[&str]) -> Option<String> {
        let name = name.map(str::to_string);
        let email = email.map(str::to_string);
        move |args: &[&str]| match args {
            ["config", "--get", "user.name"] => name.clone(),
            ["config", "--get", "user.email"] => email.clone(),
            _ => None,
        }
    }

    fn no_host(_: &str) -> Option<String> {
        None
    }

    fn keys(env: &[(String, String)]) -> Vec<&str> {
        env.iter().map(|(k, _)| k.as_str()).collect()
    }

    #[test]
    fn both_set_yields_all_four_vars() {
        let id = resolve(&reader(Some("Ada Lovelace"), Some("ada@example.org")));
        assert_eq!(id.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(id.email.as_deref(), Some("ada@example.org"));
        assert!(!id.is_empty());
        let env = identity_env(&id, &[], &no_host);
        assert_eq!(
            env,
            vec![
                ("GIT_AUTHOR_NAME".into(), "Ada Lovelace".into()),
                ("GIT_COMMITTER_NAME".into(), "Ada Lovelace".into()),
                ("GIT_AUTHOR_EMAIL".into(), "ada@example.org".into()),
                ("GIT_COMMITTER_EMAIL".into(), "ada@example.org".into()),
            ]
        );
    }

    #[test]
    fn one_missing_yields_only_the_other_half() {
        let id = resolve(&reader(None, Some("ada@example.org")));
        assert_eq!(id.name, None);
        let env = identity_env(&id, &[], &no_host);
        assert_eq!(keys(&env), vec!["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"]);

        let none = resolve(&reader(None, None));
        assert!(none.is_empty());
        assert!(identity_env(&none, &[], &no_host).is_empty());
    }

    #[test]
    fn invalid_value_is_dropped() {
        let long = "x".repeat(MAX_VALUE_LEN + 1);
        for bad in [
            "Ada\nGIT_X=1",
            "Ada\u{7}",
            "Ada\rLovelace",
            "",
            long.as_str(),
        ] {
            let id = resolve(&reader(Some(bad), Some("ada@example.org")));
            assert_eq!(id.name, None, "{bad:?} must be dropped");
            assert_eq!(id.email.as_deref(), Some("ada@example.org"));
        }
        assert!(valid_value(&"x".repeat(MAX_VALUE_LEN)));
        // A hand-built identity is validated again at emission.
        let id = GitIdentity {
            name: Some("bad\nname".into()),
            email: None,
        };
        assert!(identity_env(&id, &[], &no_host).is_empty());
    }

    #[test]
    fn resolve_trims_whitespace() {
        let id = resolve(&reader(Some("  Ada  "), None));
        assert_eq!(id.name.as_deref(), Some("Ada"));
    }

    #[test]
    fn host_env_value_wins() {
        let id = resolve(&reader(Some("Ada"), Some("ada@example.org")));
        let host = |k: &str| (k == "GIT_AUTHOR_NAME").then(|| "Explicit".to_string());
        let env = identity_env(&id, &[], &host);
        assert_eq!(
            keys(&env),
            vec![
                "GIT_COMMITTER_NAME",
                "GIT_AUTHOR_EMAIL",
                "GIT_COMMITTER_EMAIL"
            ]
        );
        // An empty host value is not an explicit choice.
        let empty = |k: &str| (k == "GIT_AUTHOR_NAME").then(String::new);
        assert_eq!(identity_env(&id, &[], &empty).len(), 4);
    }

    #[test]
    fn existing_env_value_wins() {
        let id = resolve(&reader(Some("Ada"), Some("ada@example.org")));
        let existing = vec![("GIT_COMMITTER_EMAIL".to_string(), "x@y".to_string())];
        let env = identity_env(&id, &existing, &no_host);
        assert_eq!(
            keys(&env),
            vec!["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME", "GIT_AUTHOR_EMAIL"]
        );
    }
}
