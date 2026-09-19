//! The one selector for the trusted `[workspace.<key>]` (`[project.<key>]`)
//! overlay (THE-515).
//!
//! That overlay carries security authority — coding-agent accounts, env
//! bundles / HOME, trusted hooks, ungated sandbox mounts, merge/PR queue gates
//! and targets, CI autofix, autopilot, Git policy — so *which* block applies to
//! a repository must be decided in exactly one place, and an ambiguous choice
//! must be refused rather than resolved by first match.
//!
//! Every production consumer goes through [`resolve`] (via
//! [`crate::config::Config::workspace_overlay`] /
//! [`crate::config::Config::workspace_overlay_for_key`]); nothing reads
//! `Config::workspace` directly. Today the key is still the legacy lossy
//! basename slug; the canonical binding to THE-516's `RepositoryId` (and the
//! admitted-config revision from THE-505) replaces the key input here without
//! touching the consumers. See `.thegn/maintenance/THE-515/plan.md`.
//!
//! What this module refuses now, independent of repository identity:
//!
//! * a table key that is not in normalized slug form (`Foo`, `my_repo`) — it
//!   could only ever be reached by some key derivations and not others, so the
//!   same block would govern different authorities depending on the path;
//! * two or more table keys that normalize to the requested key (`foo` and
//!   `Foo`) — the user's intent is ambiguous;
//! * an empty key.
//!
//! A repository whose name has no slug (empty after normalization) selects no
//! overlay at all: the old `"repo"` fallback made `[workspace.repo]` govern
//! every such repository *and* a repository literally named `repo`.

use crate::config::WorkspaceConfig;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// The trusted overlay table (`Config::workspace`).
pub type OverlayTable = BTreeMap<String, WorkspaceConfig>;

/// Why a trusted overlay was refused. Refusal is fail-closed: no field of any
/// candidate block applies, and callers that would otherwise run automated
/// work under the weaker global policy must not (see
/// [`crate::config::Config::repo_merge_queue`] and friends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayRefusal {
    /// Several table keys normalize to `key`. `aliases` lists every table key
    /// other than `key` itself that does (sorted).
    AliasCollision { key: String, aliases: Vec<String> },
    /// The only candidate block is spelled `key`, which is not in normalized
    /// form; it must be renamed to `canonical` to take effect.
    NonCanonicalKey { key: String, canonical: String },
    /// Several LIVE main checkouts at different canonical locations share the
    /// legacy key, so the block cannot tell them apart until it is bound to a
    /// canonical repository identity. `repositories` is sorted.
    AmbiguousRepositories {
        key: String,
        repositories: Vec<String>,
    },
    /// The registered-repository index could not be read; refuse rather than
    /// guess.
    RegistryUnavailable { key: String },
}

impl fmt::Display for OverlayRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AliasCollision { key, aliases } => write!(
                f,
                "trusted [project.{key}] overlay refused: table keys {} normalize to the same \
                 repository key `{key}`; keep exactly one block named `{key}`",
                aliases
                    .iter()
                    .map(|alias| format!("`{alias}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::NonCanonicalKey { key, canonical } => write!(
                f,
                "trusted [project.{key}] overlay refused: key is not in normalized form; \
                 rename the block to `{canonical}`"
            ),
            Self::AmbiguousRepositories { key, repositories } => write!(
                f,
                "trusted [project.{key}] overlay refused: {} are separate checkouts all named \
                 `{key}`, and the block cannot tell them apart yet. Rename or delete the checkout \
                 the block is NOT for (it stops counting as soon as it is gone)",
                repositories.join(" and ")
            ),
            Self::RegistryUnavailable { key } => write!(
                f,
                "trusted [project.{key}] overlay refused: the repository registry is unavailable"
            ),
        }
    }
}

impl std::error::Error for OverlayRefusal {}

/// The outcome of selecting a repository's trusted overlay.
#[derive(Debug, Clone)]
pub enum WorkspaceOverlay<'a> {
    /// No block applies to this repository.
    Unconfigured,
    /// Exactly one unambiguous block applies.
    Selected {
        key: &'a str,
        overlay: &'a WorkspaceConfig,
    },
    /// A block might apply but the choice is ambiguous; nothing applies.
    Refused(OverlayRefusal),
}

impl<'a> WorkspaceOverlay<'a> {
    /// The selected block, or `None` when unconfigured or refused.
    pub fn overlay(&self) -> Option<&'a WorkspaceConfig> {
        match self {
            Self::Selected { overlay, .. } => Some(overlay),
            Self::Unconfigured | Self::Refused(_) => None,
        }
    }

    /// The refusal, when the selection was refused.
    pub fn refusal(&self) -> Option<&OverlayRefusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            Self::Unconfigured | Self::Selected { .. } => None,
        }
    }

    pub fn is_refused(&self) -> bool {
        matches!(self, Self::Refused(_))
    }

    pub fn is_unconfigured(&self) -> bool {
        matches!(self, Self::Unconfigured)
    }
}

/// Normalize a human repository name to its legacy overlay key. `None` when
/// the name has no slug: such a repository selects no overlay.
pub fn legacy_key_for_name(name: &str) -> Option<String> {
    let key = crate::util::slugify(name);
    (!key.is_empty()).then_some(key)
}

/// Legacy overlay key for a known repository ROOT, derived purely from the
/// path (no Git, no filesystem, safe on the UI loop). Linked-worktree paths
/// must not be passed here: their basename is the worktree directory, not the
/// repository.
pub fn legacy_key_for_root(root: &Path) -> Option<String> {
    legacy_key_for_name(&crate::repo::repo_name_from_path(root))
}

/// Whether a table key is a normalization fixed point.
fn is_canonical(key: &str) -> bool {
    !key.is_empty() && crate::util::slugify(key) == key
}

/// Select the trusted overlay for `key` (already derived by the caller; a
/// non-normalized key is normalized first). This is the only selection
/// algorithm; see the module docs for what it refuses.
pub fn resolve<'a>(table: &'a OverlayTable, key: Option<&str>) -> WorkspaceOverlay<'a> {
    if table.is_empty() {
        return WorkspaceOverlay::Unconfigured;
    }
    let Some(key) = key.and_then(legacy_key_for_name) else {
        return WorkspaceOverlay::Unconfigured;
    };
    if let Some(refusal) = refusal(table, &key) {
        return WorkspaceOverlay::Refused(refusal);
    }
    match table.get_key_value(key.as_str()) {
        Some((key, overlay)) => WorkspaceOverlay::Selected { key, overlay },
        None => WorkspaceOverlay::Unconfigured,
    }
}

/// The refusal [`resolve`] reports for `key` (normalized first), if any.
pub fn refusal(table: &OverlayTable, key: &str) -> Option<OverlayRefusal> {
    let key = legacy_key_for_name(key)?;
    let aliases: Vec<String> = table
        .keys()
        .filter(|candidate| candidate.as_str() != key && crate::util::slugify(candidate) == key)
        .cloned()
        .collect();
    match (aliases.len(), table.contains_key(&key)) {
        (0, _) => None,
        (1, false) => Some(OverlayRefusal::NonCanonicalKey {
            key: aliases[0].clone(),
            canonical: key,
        }),
        _ => Some(OverlayRefusal::AliasCollision { key, aliases }),
    }
}

/// The repository path the registry (`repo_slugs`) records for a tab slug.
pub fn registered_path<'r>(slug: &str, rows: &'r [(String, String)]) -> Option<&'r str> {
    rows.iter()
        .find(|(_, s)| s == slug)
        .map(|(path, _)| path.as_str())
}

/// Other registered repositories that derive the same legacy `key` as `own`
/// AND are live main checkouts at a different canonical location. `live`
/// returns a path's canonical location iff it is a live main checkout (the
/// caller's filesystem probe), so stale rows (removed clones, dir workspaces,
/// linked worktrees) and two spellings of one checkout (symlink, `/tmp` vs
/// `/private/tmp`) never count. Sorted, deduplicated by canonical location.
pub fn live_duplicates(
    key: &str,
    own: &str,
    rows: &[(String, String)],
    live: impl Fn(&str) -> Option<std::path::PathBuf>,
) -> Vec<String> {
    let own_canonical = live(own).unwrap_or_else(|| std::path::PathBuf::from(own));
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for (path, _) in rows {
        if path == own || legacy_key_for_root(Path::new(path)).as_deref() != Some(key) {
            continue;
        }
        let Some(canonical) = live(path) else {
            continue;
        };
        if canonical != own_canonical && seen.insert(canonical) {
            out.push(path.clone());
        }
    }
    out.sort();
    out
}

/// Whether any block that normalizes to `key` carries credential authority
/// (agent accounts or an env bundle / HOME). Used to refuse a launch outright
/// rather than run it with credentials the trusted block did not pin.
pub fn candidates_carry_credentials(table: &OverlayTable, key: &str) -> bool {
    let Some(key) = legacy_key_for_name(key) else {
        return false;
    };
    table
        .iter()
        .filter(|(k, _)| crate::util::slugify(k) == key)
        .any(|(_, ws)| !ws.accounts.is_empty() || ws.env_bundle.is_some())
}

/// Global validation of the overlay table: every key must be a non-empty,
/// normalized slug, and no two keys may normalize to the same slug. One
/// message per offending normalized key (or per empty key), in key order.
pub fn validate(table: &OverlayTable) -> Vec<String> {
    let mut errors = Vec::new();
    let mut groups: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for key in table.keys() {
        if is_canonical(key) {
            groups.entry(key.clone()).or_default().push(key);
            continue;
        }
        match legacy_key_for_name(key) {
            Some(canonical) => groups.entry(canonical).or_default().push(key),
            None => errors.push(format!(
                "project.{key:?}: trusted overlay key has no repository slug; \
                 it can never be selected safely — rename or remove the block"
            )),
        }
    }
    for (canonical, keys) in groups {
        let has_non_canonical = keys.iter().any(|key| *key != canonical);
        if has_non_canonical && let Some(refusal) = refusal(table, &canonical) {
            errors.push(format!("project.{canonical}: {refusal}"));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(keys: &[&str]) -> OverlayTable {
        keys.iter()
            .map(|key| (key.to_string(), WorkspaceConfig::default()))
            .collect()
    }

    #[test]
    fn empty_table_and_underivable_keys_select_nothing() {
        assert!(resolve(&table(&[]), Some("foo")).is_unconfigured());
        let t = table(&["repo"]);
        assert!(resolve(&t, None).is_unconfigured());
        // An unslugifiable name no longer falls back to `[workspace.repo]`.
        assert!(resolve(&t, Some("___")).is_unconfigured());
        assert!(resolve(&t, Some("")).is_unconfigured());
        assert_eq!(legacy_key_for_root(Path::new("/src/.git")), None);
        assert_eq!(legacy_key_for_root(Path::new("/src/___")), None);
        // A repository literally named `repo` still gets its own block.
        assert!(resolve(&t, Some("repo")).overlay().is_some());
    }

    #[test]
    fn exact_canonical_key_is_selected() {
        let t = table(&["widget", "other"]);
        match resolve(&t, Some("widget")) {
            WorkspaceOverlay::Selected { key, .. } => assert_eq!(key, "widget"),
            other => panic!("expected selection, got {other:?}"),
        }
        assert!(resolve(&t, Some("absent")).is_unconfigured());
    }

    #[test]
    fn requested_key_is_normalized_like_a_repository_name() {
        let t = table(&["my-repo"]);
        for raw in ["My.Repo", "my_repo", "MY REPO", "-my-repo-"] {
            assert!(
                resolve(&t, Some(raw)).overlay().is_some(),
                "{raw} normalizes to my-repo"
            );
        }
        assert_eq!(
            legacy_key_for_root(Path::new("/a/My.Repo.git")),
            Some("my-repo".into())
        );
    }

    #[test]
    fn punctuation_and_case_aliases_are_refused_not_first_matched() {
        // Two blocks that both claim `foo`: neither applies.
        let t = table(&["Foo", "foo"]);
        assert!(resolve(&t, Some("foo")).is_refused());
        assert!(resolve(&t, Some("FOO")).is_refused());
        assert_eq!(
            refusal(&t, "foo"),
            Some(OverlayRefusal::AliasCollision {
                key: "foo".into(),
                aliases: vec!["Foo".into()]
            })
        );
        // Two non-canonical spellings with no canonical block: still ambiguous.
        let t = table(&["my_repo", "My.Repo"]);
        assert!(resolve(&t, Some("my-repo")).is_refused());
        assert!(matches!(
            refusal(&t, "my-repo"),
            Some(OverlayRefusal::AliasCollision { .. })
        ));
        // The collision does not bleed into unrelated keys.
        let t = table(&["Foo", "foo", "bar"]);
        assert!(resolve(&t, Some("bar")).overlay().is_some());
    }

    #[test]
    fn a_lone_non_canonical_key_is_refused_with_its_rename() {
        let t = table(&["My_Repo"]);
        assert!(resolve(&t, Some("my-repo")).is_refused());
        assert_eq!(
            refusal(&t, "my-repo"),
            Some(OverlayRefusal::NonCanonicalKey {
                key: "My_Repo".into(),
                canonical: "my-repo".into()
            })
        );
        let message = refusal(&t, "my-repo").unwrap().to_string();
        assert!(
            message.contains("rename the block to `my-repo`"),
            "{message}"
        );
    }

    fn rows(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect()
    }

    #[test]
    fn only_live_distinct_checkouts_count_as_duplicates() {
        let r = rows(&[
            ("/code/foo", "foo-2"),
            ("/tmp/foo", "foo"),
            ("/private/tmp/foo", "foo-3"),
            ("/gone/foo", "foo-4"),
            ("/code/bar", "bar"),
        ]);
        assert_eq!(registered_path("foo-2", &r), Some("/code/foo"));
        assert_eq!(registered_path("nope", &r), None);
        // /tmp/foo and /private/tmp/foo are one checkout; /gone/foo is stale.
        let live = |p: &str| match p {
            "/code/foo" => Some(std::path::PathBuf::from("/code/foo")),
            "/tmp/foo" | "/private/tmp/foo" => Some(std::path::PathBuf::from("/private/tmp/foo")),
            "/code/bar" => Some(std::path::PathBuf::from("/code/bar")),
            _ => None,
        };
        // One live duplicate, reported once (the first registry spelling).
        assert_eq!(
            live_duplicates("foo", "/code/foo", &r, live),
            vec!["/tmp/foo".to_string()]
        );
        // Only a stale row besides us: nothing counts.
        let stale = |p: &str| (p == "/code/foo").then(|| std::path::PathBuf::from(p));
        assert!(live_duplicates("foo", "/code/foo", &r, stale).is_empty());
        // A second spelling of our OWN checkout is not a duplicate.
        let same = |p: &str| {
            matches!(p, "/code/foo" | "/tmp/foo").then(|| std::path::PathBuf::from("/code/foo"))
        };
        assert!(live_duplicates("foo", "/code/foo", &r, same).is_empty());
    }

    #[test]
    fn credential_carrying_candidates_are_detected_across_aliases() {
        let mut t = table(&["foo", "FOO", "bar"]);
        assert!(!candidates_carry_credentials(&t, "foo"));
        t.get_mut("FOO").unwrap().env_bundle = Some("work".into());
        assert!(candidates_carry_credentials(&t, "foo"));
        assert!(!candidates_carry_credentials(&t, "bar"));
    }

    #[test]
    fn validate_reports_every_non_canonical_colliding_or_empty_key() {
        assert!(validate(&table(&["alpha", "beta-2", "repo"])).is_empty());
        let errors = validate(&table(&["", "Foo", "foo", "my_repo", "___", "ok"]));
        assert_eq!(errors.len(), 4, "{errors:#?}");
        assert!(errors.iter().any(|e| e.starts_with("project.\"\"")));
        assert!(errors.iter().any(|e| e.starts_with("project.\"___\"")));
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("project.foo:") && e.contains("`Foo`"))
        );
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("project.my-repo:") && e.contains("rename"))
        );
    }

    #[test]
    fn non_utf8_like_and_unicode_names_have_no_ascii_alias() {
        // Non-ASCII-only names have no slug, so they cannot alias an ASCII
        // block (the old code sent them all to `[workspace.repo]`).
        let t = table(&["repo", "caf"]);
        assert!(resolve(&t, Some("日本語")).is_unconfigured());
        // Mixed names normalize their ASCII part only; that is the legacy key
        // and remains ambiguous across repositories until THE-516 binding.
        assert!(resolve(&t, Some("café")).overlay().is_some());
    }
}
