//! The one selector for the trusted `[workspace.<key>]` (`[project.<key>]`)
//! overlay (THE-515).
//!
//! That overlay carries security authority — coding-agent accounts, env
//! bundles / HOME, trusted hooks, ungated sandbox mounts, merge/PR queue gates
//! and targets, CI autofix, autopilot, Git policy — so *which* block applies to
//! a repository must be decided in exactly one place, and an ambiguous choice
//! must be refused rather than resolved by first match.
//!
//! Every production consumer goes through [`resolve`] or [`resolve_for_root`] (via
//! [`crate::config::Config::workspace_overlay`] /
//! [`crate::config::Config::workspace_overlay_for_key`]); nothing reads
//! `Config::workspace` directly. Today the key is still the legacy lossy
//! basename slug. An optional `repo_path` is the explicit path mapping asked
//! for by THE-515; the canonical binding to THE-516's `RepositoryId` (and the
//! admitted-config revision from THE-505) is still to come. See THE-515.
//!
//! What this module refuses now, independent of repository identity:
//!
//! * a table key that is not in normalized slug form (`Foo`, `my_repo`) — it
//!   could only ever be reached by some key derivations and not others, so the
//!   same block would govern different authorities depending on the path;
//! * two or more table keys that normalize to the requested key (`foo` and
//!   `Foo`) — the user's intent is ambiguous;
//! * a block bound to another checkout, or multiple blocks bound to this one;
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
    /// A key-matched block is explicitly bound to another checkout.
    BoundToOtherCheckout { key: String, repo_path: String },
    /// Several blocks explicitly claim the same checkout path.
    RepoPathCollision {
        repo_path: String,
        keys: Vec<String>,
    },
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
            Self::BoundToOtherCheckout { key, repo_path } => write!(
                f,
                "trusted [project.{key}] overlay refused: the block is bound to `{repo_path}` \
                 (`repo_path`), which is not this checkout; add a block bound to this checkout, \
                 or fix `repo_path` if it names this checkout by another spelling"
            ),
            Self::RepoPathCollision { repo_path, keys } => write!(
                f,
                "trusted workspace overlays refused: blocks {} are bound to `{repo_path}`; \
                 keep only one block bound to this checkout",
                keys.iter()
                    .map(|key| format!("[project.{key}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
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

/// Expand `~` and normalize an absolute repo path without filesystem access.
/// Empty, relative, or parent-traversing paths are invalid.
pub fn normalize_repo_path(raw: &str) -> Option<std::path::PathBuf> {
    let expanded = crate::util::expand_tilde(raw.trim());
    normalize_path(Path::new(&expanded))
}

fn normalize_path(path: &Path) -> Option<std::path::PathBuf> {
    use std::path::Component;

    if !path.is_absolute()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return None;
    }
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        if component != Component::CurDir {
            normalized.push(component.as_os_str());
        }
    }
    Some(normalized)
}

/// Compare paths after the same purely lexical normalization used for bindings.
pub fn lexical_same_path(bound: &Path, root: &Path) -> bool {
    let Some(bound) = normalize_path(bound) else {
        return false;
    };
    let Some(root) = normalize_path(root) else {
        return false;
    };
    bound == root
}

/// Select by legacy key. Key-only consumers cannot prove a checkout, so bound
/// blocks are intentionally invisible to them.
pub fn resolve<'a>(table: &'a OverlayTable, key: Option<&str>) -> WorkspaceOverlay<'a> {
    match resolve_by_key(table, key) {
        WorkspaceOverlay::Selected { overlay, .. } if overlay.repo_path.is_some() => {
            WorkspaceOverlay::Unconfigured
        }
        result => result,
    }
}

fn resolve_by_key<'a>(table: &'a OverlayTable, key: Option<&str>) -> WorkspaceOverlay<'a> {
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

/// Select a trusted overlay for a repository root using explicit path bindings
/// first, then today's legacy key behavior.
pub fn resolve_for_root<'a>(
    table: &'a OverlayTable,
    root: &Path,
    same_checkout: impl Fn(&Path, &Path) -> bool,
) -> WorkspaceOverlay<'a> {
    if table.is_empty() {
        return WorkspaceOverlay::Unconfigured;
    }
    let mut bound = Vec::new();
    for (key, overlay) in table {
        if let Some(raw) = overlay.repo_path.as_deref()
            && let Some(path) = normalize_repo_path(raw)
            && same_checkout(&path, root)
        {
            bound.push((key.as_str(), overlay));
        }
    }
    match bound.as_slice() {
        [one] => {
            let (key, overlay) = *one;
            if let Some(refusal) = refusal(table, key) {
                WorkspaceOverlay::Refused(refusal)
            } else {
                WorkspaceOverlay::Selected { key, overlay }
            }
        }
        [_, _, ..] => {
            let mut keys: Vec<String> = bound.iter().map(|(key, _)| (*key).to_string()).collect();
            keys.sort();
            WorkspaceOverlay::Refused(OverlayRefusal::RepoPathCollision {
                repo_path: root.display().to_string(),
                keys,
            })
        }
        [] => {
            let key = legacy_key_for_root(root);
            match resolve_by_key(table, key.as_deref()) {
                WorkspaceOverlay::Selected { key, overlay } => {
                    if let Some(repo_path) = overlay.repo_path.as_ref() {
                        WorkspaceOverlay::Refused(OverlayRefusal::BoundToOtherCheckout {
                            key: key.to_string(),
                            repo_path: repo_path.clone(),
                        })
                    } else {
                        WorkspaceOverlay::Selected { key, overlay }
                    }
                }
                result => result,
            }
        }
    }
}

/// Whether refusing this overlay also requires refusing credential-bearing
/// agent launches.
pub fn refusal_carries_credentials(table: &OverlayTable, refusal: &OverlayRefusal) -> bool {
    match refusal {
        OverlayRefusal::AliasCollision { key, .. }
        | OverlayRefusal::NonCanonicalKey { canonical: key, .. }
        | OverlayRefusal::AmbiguousRepositories { key, .. }
        | OverlayRefusal::BoundToOtherCheckout { key, .. } => {
            candidates_carry_credentials(table, key)
        }
        OverlayRefusal::RepoPathCollision { keys, .. } => keys.iter().any(|key| {
            table
                .get(key)
                .is_some_and(|overlay| !overlay.accounts.is_empty() || overlay.env_bundle.is_some())
        }),
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
/// AND are separate live main checkouts. `live` returns a path's filesystem
/// IDENTITY iff it is a live main checkout (the caller's probe), so stale rows
/// (removed clones, dir workspaces, linked worktrees) never count, and neither
/// does a second spelling of one checkout — a symlink, a bind mount, `/tmp` vs
/// `/private/tmp`, or a case-insensitive mount all share one identity.
/// Sorted, deduplicated by identity.
pub fn live_duplicates(
    key: &str,
    own: &str,
    rows: &[(String, String)],
    live: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let own_identity = live(own).unwrap_or_else(|| own.to_string());
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for (path, _) in rows {
        if path == own || legacy_key_for_root(Path::new(path)).as_deref() != Some(key) {
            continue;
        }
        let Some(identity) = live(path) else {
            continue;
        };
        if identity != own_identity && seen.insert(identity) {
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
        if let Some(raw) = table[key].repo_path.as_deref()
            && normalize_repo_path(raw).is_none()
        {
            errors.push(format!(
                "project.{key}.repo_path: must be an absolute path without `..` components"
            ));
        }
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
    let mut paths: BTreeMap<std::path::PathBuf, Vec<&str>> = BTreeMap::new();
    for (key, overlay) in table {
        if let Some(raw) = overlay.repo_path.as_deref()
            && let Some(path) = normalize_repo_path(raw)
        {
            paths.entry(path).or_default().push(key);
        }
    }
    for (path, mut keys) in paths {
        if keys.len() > 1 {
            keys.sort_unstable();
            errors.push(format!(
                "project.repo_path: `{}` is bound by keys {}; keep only one block for this path",
                path.display(),
                keys.iter()
                    .map(|key| format!("`{key}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
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
        // Identities, not paths: /tmp/foo and /private/tmp/foo are one inode.
        let live = |p: &str| match p {
            "/code/foo" => Some("dev:1".to_string()),
            "/tmp/foo" | "/private/tmp/foo" => Some("dev:2".to_string()),
            "/code/bar" => Some("dev:3".to_string()),
            _ => None,
        };
        // One live duplicate, reported once (the first registry spelling).
        assert_eq!(
            live_duplicates("foo", "/code/foo", &r, live),
            vec!["/tmp/foo".to_string()]
        );
        // Only a stale row besides us: nothing counts.
        let stale = |p: &str| (p == "/code/foo").then(|| "dev:1".to_string());
        assert!(live_duplicates("foo", "/code/foo", &r, stale).is_empty());
        // A second spelling of our OWN checkout is not a duplicate.
        // A bind mount of our own checkout shares its identity.
        let same = |p: &str| matches!(p, "/code/foo" | "/tmp/foo").then(|| "dev:1".to_string());
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

    #[test]
    fn repo_path_binding_selects_each_same_basename_checkout_its_own_block() {
        let mut t = table(&["foo", "foo-oss"]);
        t.get_mut("foo").unwrap().repo_path = Some("/a/foo".into());
        t.get_mut("foo-oss").unwrap().repo_path = Some("/b/foo".into());
        for (root, expected) in [("/a/foo", "foo"), ("/b/foo/", "foo-oss")] {
            match resolve_for_root(&t, Path::new(root), lexical_same_path) {
                WorkspaceOverlay::Selected { key, .. } => assert_eq!(key, expected),
                other => panic!("expected {expected} selection, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_bound_block_is_refused_for_another_checkout_by_key() {
        let mut t = table(&["foo"]);
        t.get_mut("foo").unwrap().repo_path = Some("/a/foo".into());
        let result = resolve_for_root(&t, Path::new("/c/foo"), lexical_same_path);
        assert!(matches!(
            &result,
            WorkspaceOverlay::Refused(OverlayRefusal::BoundToOtherCheckout {
                key,
                repo_path
            }) if key == "foo" && repo_path == "/a/foo"
        ));
        assert!(result.refusal().unwrap().to_string().contains("/a/foo"));
    }

    #[test]
    fn two_blocks_bound_to_one_path_are_refused_and_invalid() {
        let mut t = table(&["foo", "bar"]);
        t.get_mut("foo").unwrap().repo_path = Some("/a/foo".into());
        t.get_mut("bar").unwrap().repo_path = Some("/a/foo".into());
        assert!(matches!(
            resolve_for_root(&t, Path::new("/a/foo"), lexical_same_path),
            WorkspaceOverlay::Refused(OverlayRefusal::RepoPathCollision { ref keys, .. })
                if keys.iter().map(String::as_str).eq(["bar", "foo"])
        ));
        let errors = validate(&t);
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert!(errors[0].contains("/a/foo"));
        assert!(errors[0].contains("bar") && errors[0].contains("foo"));
    }

    #[test]
    fn malformed_repo_paths_fail_validation_and_never_match() {
        for raw in ["relative/foo", "", "/a/../foo"] {
            let mut t = table(&["foo"]);
            t.get_mut("foo").unwrap().repo_path = Some(raw.into());
            let errors = validate(&t);
            assert_eq!(errors.len(), 1, "{raw:?}: {errors:#?}");
            assert!(errors[0].contains("project.foo.repo_path"));
            assert!(matches!(
                resolve_for_root(&t, Path::new("/a/foo"), lexical_same_path),
                WorkspaceOverlay::Refused(OverlayRefusal::BoundToOtherCheckout { .. })
            ));
        }
    }

    #[test]
    fn key_only_lookup_never_selects_a_bound_block() {
        let mut t = table(&["foo"]);
        t.get_mut("foo").unwrap().repo_path = Some("/a/foo".into());
        assert!(resolve(&t, Some("foo")).is_unconfigured());
    }

    #[test]
    fn tab_slug_binding_resolves_two_live_same_named_checkouts() {
        use crate::store::WorkspaceStore;

        fn checkout(dir: &Path, name: &str) -> String {
            let path = dir.join(name);
            std::fs::create_dir_all(path.join(".git")).unwrap();
            path.to_string_lossy().into_owned()
        }

        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open_memory().unwrap();
        let a = checkout(dir.path(), "a/foo");
        let b = checkout(dir.path(), "b/foo");
        assert_eq!(db.slug_for_repo(&a, "foo").unwrap(), "foo");
        assert_eq!(db.slug_for_repo(&b, "foo").unwrap(), "foo-2");

        let mut cfg = crate::config::Config::default();
        cfg.workspace.entry("foo".into()).or_default().repo_path = Some(a);
        assert!(matches!(
            cfg.workspace_overlay_for_tab_slug(&db, "foo"),
            WorkspaceOverlay::Selected { key: "foo", .. }
        ));
        assert!(matches!(
            cfg.workspace_overlay_for_tab_slug(&db, "foo-2"),
            WorkspaceOverlay::Refused(OverlayRefusal::BoundToOtherCheckout { .. })
        ));
        cfg.workspace.entry("foo-b".into()).or_default().repo_path = Some(b);
        assert!(matches!(
            cfg.workspace_overlay_for_tab_slug(&db, "foo-2"),
            WorkspaceOverlay::Selected { key: "foo-b", .. }
        ));
    }

    #[test]
    fn refusal_carries_credentials_covers_every_variant() {
        let credentialed = |keys: &[&str]| {
            let mut t = table(keys);
            for key in keys {
                t.get_mut(*key)
                    .unwrap()
                    .accounts
                    .insert("codex".into(), "work".into());
            }
            t
        };
        let t = credentialed(&["foo", "FOO"]);
        for refusal in [
            OverlayRefusal::AliasCollision {
                key: "foo".into(),
                aliases: vec!["FOO".into()],
            },
            OverlayRefusal::NonCanonicalKey {
                key: "FOO".into(),
                canonical: "foo".into(),
            },
            OverlayRefusal::AmbiguousRepositories {
                key: "foo".into(),
                repositories: vec![],
            },
            OverlayRefusal::BoundToOtherCheckout {
                key: "foo".into(),
                repo_path: "/a/foo".into(),
            },
            OverlayRefusal::RepoPathCollision {
                repo_path: "/a/foo".into(),
                keys: vec!["foo".into()],
            },
        ] {
            assert!(refusal_carries_credentials(&t, &refusal), "{refusal:?}");
        }
        let empty = table(&["foo", "FOO"]);
        for refusal in [
            OverlayRefusal::AliasCollision {
                key: "foo".into(),
                aliases: vec!["FOO".into()],
            },
            OverlayRefusal::NonCanonicalKey {
                key: "FOO".into(),
                canonical: "foo".into(),
            },
            OverlayRefusal::AmbiguousRepositories {
                key: "foo".into(),
                repositories: vec![],
            },
            OverlayRefusal::BoundToOtherCheckout {
                key: "foo".into(),
                repo_path: "/a/foo".into(),
            },
            OverlayRefusal::RepoPathCollision {
                repo_path: "/a/foo".into(),
                keys: vec!["foo".into()],
            },
        ] {
            assert!(
                !refusal_carries_credentials(&empty, &refusal),
                "{refusal:?}"
            );
        }
    }

    #[test]
    fn an_unbound_block_still_selects_by_key_through_resolve_for_root() {
        let t = table(&["foo"]);
        assert!(matches!(
            resolve_for_root(&t, Path::new("/x/foo"), lexical_same_path),
            WorkspaceOverlay::Selected { key: "foo", .. }
        ));
    }
}
