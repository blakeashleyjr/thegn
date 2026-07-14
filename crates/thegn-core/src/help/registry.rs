//! The validated help registry: every page parsed, indexed by id, arranged
//! into a TOC, and cross-checked (links resolve, contexts are known and
//! uniquely claimed, parents exist and don't cycle).
//!
//! `build` is total: it always returns a usable registry plus the list of
//! validation errors. At runtime the host logs the errors and serves what
//! parsed; in CI the ratchet test asserts the list is empty — that split is
//! what lets validation be strict without ever taking the compositor down.

use std::collections::HashMap;
use std::fmt;

use super::frontmatter::{self, FrontmatterError, PageMeta};
use super::markdown::{self, Block, LinkTarget};

/// One parsed help page. `body` is kept raw for search snippets.
#[derive(Debug, Clone)]
pub struct HelpPage {
    pub meta: PageMeta,
    pub blocks: Vec<Block>,
    pub body: String,
}

/// A node of the table of contents (page ids, ordered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocNode {
    pub id: String,
    pub children: Vec<TocNode>,
}

/// What `build` can find wrong with the page set. `source` indexes into the
/// `sources` slice (for pages whose id never parsed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    Frontmatter {
        source: usize,
        error: FrontmatterError,
    },
    DuplicateId {
        id: String,
    },
    DanglingParent {
        page: String,
        parent: String,
    },
    ParentCycle {
        page: String,
    },
    DanglingLink {
        page: String,
        target: String,
    },
    UnknownContext {
        page: String,
        context: String,
    },
    DuplicateContext {
        context: String,
        first: String,
        second: String,
    },
    MissingIndex,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frontmatter { source, error } => {
                write!(f, "page source #{source}: {error}")
            }
            Self::DuplicateId { id } => write!(f, "duplicate page id `{id}`"),
            Self::DanglingParent { page, parent } => {
                write!(f, "page `{page}`: parent `{parent}` does not exist")
            }
            Self::ParentCycle { page } => {
                write!(f, "page `{page}`: parent chain forms a cycle")
            }
            Self::DanglingLink { page, target } => {
                write!(f, "page `{page}`: [[{target}]] does not exist")
            }
            Self::UnknownContext { page, context } => {
                write!(f, "page `{page}`: unknown context `{context}`")
            }
            Self::DuplicateContext {
                context,
                first,
                second,
            } => {
                write!(
                    f,
                    "context `{context}` claimed by both `{first}` and `{second}`"
                )
            }
            Self::MissingIndex => write!(f, "no `index` page (the fallback root)"),
        }
    }
}

/// The immutable page index the help UI reads from.
#[derive(Debug, Clone, Default)]
pub struct HelpRegistry {
    pages: Vec<HelpPage>,
    by_id: HashMap<String, usize>,
    toc: Vec<TocNode>,
    by_context: HashMap<String, String>,
}

impl HelpRegistry {
    /// Parse and index `sources` (one string per page). `known_contexts` is
    /// the host's context-key vocabulary (`zone:*`, `panel:*`, …); any
    /// `contexts:` entry outside it is an error. Always returns a registry —
    /// pages that fail to parse are skipped and reported.
    pub fn build(sources: &[&str], known_contexts: &[&str]) -> (Self, Vec<ValidationError>) {
        let mut errors = Vec::new();
        let mut pages: Vec<HelpPage> = Vec::new();
        let mut by_id: HashMap<String, usize> = HashMap::new();

        for (idx, src) in sources.iter().enumerate() {
            match frontmatter::parse(src) {
                Ok((meta, body)) => {
                    if by_id.contains_key(&meta.id) {
                        errors.push(ValidationError::DuplicateId { id: meta.id });
                        continue;
                    }
                    let blocks = markdown::parse(body);
                    by_id.insert(meta.id.clone(), pages.len());
                    pages.push(HelpPage {
                        meta,
                        blocks,
                        body: body.to_string(),
                    });
                }
                Err(error) => errors.push(ValidationError::Frontmatter { source: idx, error }),
            }
        }

        let toc = build_toc(&pages, &by_id, &mut errors);

        for page in &pages {
            for target in markdown::links(&page.blocks) {
                if let LinkTarget::Page(id) = target
                    && !by_id.contains_key(id)
                {
                    errors.push(ValidationError::DanglingLink {
                        page: page.meta.id.clone(),
                        target: id.clone(),
                    });
                }
            }
        }

        let mut by_context: HashMap<String, String> = HashMap::new();
        for page in &pages {
            for ctx in &page.meta.contexts {
                if !known_contexts.contains(&ctx.as_str()) {
                    errors.push(ValidationError::UnknownContext {
                        page: page.meta.id.clone(),
                        context: ctx.clone(),
                    });
                    continue;
                }
                match by_context.entry(ctx.clone()) {
                    std::collections::hash_map::Entry::Occupied(e) => {
                        errors.push(ValidationError::DuplicateContext {
                            context: ctx.clone(),
                            first: e.get().clone(),
                            second: page.meta.id.clone(),
                        });
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(page.meta.id.clone());
                    }
                }
            }
        }

        if !by_id.contains_key("index") {
            errors.push(ValidationError::MissingIndex);
        }

        (
            Self {
                pages,
                by_id,
                toc,
                by_context,
            },
            errors,
        )
    }

    pub fn page(&self, id: &str) -> Option<&HelpPage> {
        self.by_id.get(id).map(|&i| &self.pages[i])
    }

    pub fn pages(&self) -> &[HelpPage] {
        &self.pages
    }

    pub fn toc(&self) -> &[TocNode] {
        &self.toc
    }

    /// The page documenting `context_key`, falling back to `index`, then to
    /// the first page at all (a registry can be sparse but stays navigable).
    pub fn page_for_context(&self, context_key: &str) -> Option<&str> {
        self.by_context
            .get(context_key)
            .map(String::as_str)
            .or_else(|| self.by_id.contains_key("index").then_some("index"))
            .or_else(|| self.pages.first().map(|p| p.meta.id.as_str()))
    }

    /// All (context key → page id) claims, for diagnostics and tests.
    pub fn contexts(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_context
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Arrange pages into a forest by `parent`, ordered by (`order`, title).
/// Dangling parents and cycle members are reported and promoted to roots so
/// every page stays reachable.
fn build_toc(
    pages: &[HelpPage],
    by_id: &HashMap<String, usize>,
    errors: &mut Vec<ValidationError>,
) -> Vec<TocNode> {
    let sort_key = |&i: &usize| {
        (
            pages[i].meta.order.unwrap_or(i64::MAX),
            pages[i].meta.title.to_lowercase(),
        )
    };

    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        match &page.meta.parent {
            None => roots.push(i),
            Some(parent) => match by_id.get(parent) {
                Some(&pi) if pi != i => children.entry(pi).or_default().push(i),
                _ => {
                    // Self-parent counts as dangling: promote to root.
                    errors.push(ValidationError::DanglingParent {
                        page: page.meta.id.clone(),
                        parent: parent.clone(),
                    });
                    roots.push(i);
                }
            },
        }
    }
    roots.sort_by_key(sort_key);
    for kids in children.values_mut() {
        kids.sort_by_key(sort_key);
    }

    fn node(
        i: usize,
        pages: &[HelpPage],
        children: &HashMap<usize, Vec<usize>>,
        visited: &mut [bool],
    ) -> TocNode {
        visited[i] = true;
        let mut kids = Vec::new();
        if let Some(cs) = children.get(&i) {
            for &c in cs {
                if !visited[c] {
                    kids.push(node(c, pages, children, visited));
                }
            }
        }
        TocNode {
            id: pages[i].meta.id.clone(),
            children: kids,
        }
    }

    let mut visited = vec![false; pages.len()];
    let mut toc: Vec<TocNode> = roots
        .iter()
        .map(|&r| node(r, pages, &children, &mut visited))
        .collect();

    // Anything unreachable from a root sits on a parent cycle.
    for i in 0..pages.len() {
        if !visited[i] {
            errors.push(ValidationError::ParentCycle {
                page: pages[i].meta.id.clone(),
            });
            toc.push(node(i, pages, &children, &mut visited));
        }
    }
    toc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &str, extra: &str, body: &str) -> String {
        format!(
            "---\nid: {id}\ntitle: {}\n{extra}---\n{body}",
            id.to_uppercase()
        )
    }

    const CONTEXTS: &[&str] = &["zone:sidebar", "zone:center", "panel:merge"];

    #[test]
    fn builds_a_clean_registry() {
        let index = page("index", "order: 1\n", "# Welcome\nsee [[child]]\n");
        let child = page("child", "parent: index\ncontexts: [zone:sidebar]\n", "body");
        let (reg, errors) = HelpRegistry::build(&[&index, &child], CONTEXTS);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(reg.pages().len(), 2);
        assert_eq!(reg.page("child").unwrap().meta.title, "CHILD");
        assert_eq!(
            reg.toc(),
            &[TocNode {
                id: "index".into(),
                children: vec![TocNode {
                    id: "child".into(),
                    children: vec![]
                }],
            }]
        );
        assert_eq!(reg.page_for_context("zone:sidebar"), Some("child"));
        assert_eq!(reg.page_for_context("zone:center"), Some("index"));
        assert_eq!(reg.contexts().count(), 1);
    }

    #[test]
    fn toc_orders_by_order_then_title() {
        let index = page("index", "", "");
        let b = page("bravo", "order: 2\n", "");
        let a = page("alpha", "", ""); // no order → after ordered, by title
        let z = page("zulu", "order: 1\n", "");
        let (reg, errors) = HelpRegistry::build(&[&index, &b, &a, &z], CONTEXTS);
        assert!(errors.is_empty(), "{errors:?}");
        let ids: Vec<&str> = reg.toc().iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["zulu", "bravo", "alpha", "index"]);
    }

    #[test]
    fn reports_bad_frontmatter_but_keeps_going() {
        let good = page("index", "", "");
        let (reg, errors) = HelpRegistry::build(&["no fence here", &good], CONTEXTS);
        assert_eq!(reg.pages().len(), 1);
        assert!(matches!(
            &errors[0],
            ValidationError::Frontmatter {
                source: 0,
                error: FrontmatterError::MissingOpen
            }
        ));
    }

    #[test]
    fn duplicate_id_keeps_the_first() {
        let a = page("index", "", "first");
        let b = page("index", "", "second");
        let (reg, errors) = HelpRegistry::build(&[&a, &b], CONTEXTS);
        assert_eq!(reg.pages().len(), 1);
        assert!(reg.page("index").unwrap().body.contains("first"));
        assert_eq!(
            errors,
            vec![ValidationError::DuplicateId { id: "index".into() }]
        );
    }

    #[test]
    fn dangling_parent_promotes_to_root() {
        let index = page("index", "", "");
        let orphan = page("orphan", "parent: ghost\n", "");
        let (reg, errors) = HelpRegistry::build(&[&index, &orphan], CONTEXTS);
        assert_eq!(
            errors,
            vec![ValidationError::DanglingParent {
                page: "orphan".into(),
                parent: "ghost".into()
            }]
        );
        assert_eq!(reg.toc().len(), 2, "orphan is still reachable");
    }

    #[test]
    fn parent_cycles_are_reported_and_kept_reachable() {
        let index = page("index", "", "");
        let a = page("aa", "parent: bb\n", "");
        let b = page("bb", "parent: aa\n", "");
        let selfp = page("selfy", "parent: selfy\n", "");
        let (reg, errors) = HelpRegistry::build(&[&index, &a, &b, &selfp], CONTEXTS);
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, ValidationError::ParentCycle { .. }))
        );
        assert!(
            errors.iter().any(
                |e| matches!(e, ValidationError::DanglingParent { page, .. } if page == "selfy")
            )
        );
        // Every page appears somewhere in the TOC.
        fn count(nodes: &[TocNode]) -> usize {
            nodes.iter().map(|n| 1 + count(&n.children)).sum()
        }
        assert_eq!(count(reg.toc()), 4);
    }

    #[test]
    fn dangling_links_are_reported() {
        let index = page("index", "", "see [[ghost]] and [ok](https://x.dev)\n");
        let (_, errors) = HelpRegistry::build(&[&index], CONTEXTS);
        assert_eq!(
            errors,
            vec![ValidationError::DanglingLink {
                page: "index".into(),
                target: "ghost".into()
            }]
        );
    }

    #[test]
    fn context_claims_are_validated_and_unique() {
        let index = page("index", "contexts: [zone:nowhere]\n", "");
        let a = page("aa", "contexts: [panel:merge]\n", "");
        let b = page("bb", "contexts: [panel:merge]\n", "");
        let (reg, errors) = HelpRegistry::build(&[&index, &a, &b], CONTEXTS);
        assert!(errors.iter().any(
            |e| matches!(e, ValidationError::UnknownContext { context, .. } if context == "zone:nowhere")
        ));
        assert!(errors.iter().any(|e| matches!(
            e,
            ValidationError::DuplicateContext { context, first, second }
                if context == "panel:merge" && first == "aa" && second == "bb"
        )));
        assert_eq!(
            reg.page_for_context("panel:merge"),
            Some("aa"),
            "first claim wins"
        );
    }

    #[test]
    fn missing_index_is_reported_and_fallback_degrades() {
        let solo = page("solo", "", "");
        let (reg, errors) = HelpRegistry::build(&[&solo], CONTEXTS);
        assert!(errors.contains(&ValidationError::MissingIndex));
        assert_eq!(
            reg.page_for_context("zone:center"),
            Some("solo"),
            "first page fallback"
        );

        let (empty, errors) = HelpRegistry::build(&[], CONTEXTS);
        assert!(errors.contains(&ValidationError::MissingIndex));
        assert_eq!(empty.page_for_context("zone:center"), None);
        assert!(empty.toc().is_empty());
    }

    #[test]
    fn errors_display_readably() {
        let cases: Vec<(ValidationError, &str)> = vec![
            (
                ValidationError::Frontmatter {
                    source: 3,
                    error: FrontmatterError::MissingKey("id"),
                },
                "#3",
            ),
            (ValidationError::DuplicateId { id: "x".into() }, "duplicate"),
            (
                ValidationError::DanglingParent {
                    page: "a".into(),
                    parent: "b".into(),
                },
                "does not exist",
            ),
            (ValidationError::ParentCycle { page: "a".into() }, "cycle"),
            (
                ValidationError::DanglingLink {
                    page: "a".into(),
                    target: "b".into(),
                },
                "[[b]]",
            ),
            (
                ValidationError::UnknownContext {
                    page: "a".into(),
                    context: "c".into(),
                },
                "unknown context",
            ),
            (
                ValidationError::DuplicateContext {
                    context: "c".into(),
                    first: "a".into(),
                    second: "b".into(),
                },
                "claimed by both",
            ),
            (ValidationError::MissingIndex, "index"),
        ];
        for (err, needle) in cases {
            let s = err.to_string();
            assert!(s.contains(needle), "{s:?} should contain {needle:?}");
        }
    }
}
