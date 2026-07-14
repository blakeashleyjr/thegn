//! Help search — fuzzy over titles, substring over bodies.
//!
//! The ranker is injected (the host passes its `fuzzy_rank` backend) so this
//! stays dependency-free and testable with a trivial substring ranker.
//! Titles are ranked fuzzily and weighted double; body text is matched by
//! case-insensitive substring (fuzzy over long prose is noise) and yields a
//! snippet — the matched line plus the nearest preceding heading, which the
//! host uses to jump the rendered page to the right section.

use super::markdown::{Block, plain};
use super::registry::HelpPage;

/// Score added for a body match (title fuzzy scores ride on top, doubled).
const BODY_MATCH_SCORE: u32 = 40;

/// A body match: the matched plain-text line, the query's char range within
/// it (for highlight), and the nearest preceding heading (for jump-to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    pub hl_start: usize,
    pub hl_len: usize,
    pub section: Option<String>,
}

/// One search result, best first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub page: String,
    pub title: String,
    pub score: u32,
    pub snippet: Option<Snippet>,
}

/// Case-insensitive (ASCII fold) substring find over chars; returns the
/// match's (char offset, char length).
fn find_ci(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let h: Vec<char> = hay.chars().map(|c| c.to_ascii_lowercase()).collect();
    let n: Vec<char> = needle.chars().map(|c| c.to_ascii_lowercase()).collect();
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len())
        .find(|&s| h[s..s + n.len()] == n[..])
        .map(|s| (s, n.len()))
}

/// First body match in document order, tracking the enclosing section.
fn body_snippet(page: &HelpPage, query: &str) -> Option<Snippet> {
    let mut section: Option<String> = None;
    let check = |text: String, section: &Option<String>| {
        find_ci(&text, query).map(|(hl_start, hl_len)| Snippet {
            text,
            hl_start,
            hl_len,
            section: section.clone(),
        })
    };
    for block in &page.blocks {
        match block {
            Block::Heading { spans, .. } => {
                let text = plain(spans);
                if let Some(hit) = check(text.clone(), &section) {
                    return Some(hit);
                }
                section = Some(text);
            }
            Block::Para(spans) | Block::Quote(spans) => {
                if let Some(hit) = check(plain(spans), &section) {
                    return Some(hit);
                }
            }
            Block::List(items) => {
                for item in items {
                    if let Some(hit) = check(plain(&item.spans), &section) {
                        return Some(hit);
                    }
                }
            }
            Block::Code { text, .. } => {
                for line in text.lines() {
                    if let Some(hit) = check(line.to_string(), &section) {
                        return Some(hit);
                    }
                }
            }
            Block::Rule => {}
        }
    }
    None
}

/// The injected fuzzy-ranker shape: `(needle, haystacks) → (index, score)`
/// pairs, best first — the signature of the host's fuzzy backend.
pub type Ranker = dyn Fn(&str, &[&str]) -> Vec<(usize, u16)>;

/// Rank `pages` against `query`. Pages matching neither title nor body are
/// dropped; results are sorted by combined score, stable on ties.
pub fn search(pages: &[HelpPage], query: &str, ranker: &Ranker) -> Vec<SearchHit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let titles: Vec<&str> = pages.iter().map(|p| p.meta.title.as_str()).collect();
    let mut title_score = vec![0u32; pages.len()];
    for (idx, score) in ranker(query, &titles) {
        if let Some(slot) = title_score.get_mut(idx) {
            *slot = u32::from(score);
        }
    }
    let mut hits: Vec<SearchHit> = pages
        .iter()
        .enumerate()
        .filter_map(|(i, page)| {
            let snippet = body_snippet(page, query);
            let score = title_score[i].saturating_mul(2)
                + if snippet.is_some() {
                    BODY_MATCH_SCORE
                } else {
                    0
                };
            (score > 0).then(|| SearchHit {
                page: page.meta.id.clone(),
                title: page.meta.title.clone(),
                score,
                snippet,
            })
        })
        .collect();
    hits.sort_by_key(|h| std::cmp::Reverse(h.score)); // stable: ties keep page order
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::help::registry::HelpRegistry;

    /// A trivial ranker: score 10 for a case-insensitive substring hit.
    fn ranker(needle: &str, haystacks: &[&str]) -> Vec<(usize, u16)> {
        haystacks
            .iter()
            .enumerate()
            .filter(|(_, h)| find_ci(h, needle).is_some())
            .map(|(i, _)| (i, 10))
            .collect()
    }

    fn pages() -> Vec<HelpPage> {
        let index = "---\nid: index\ntitle: Welcome\n---\n# Tour\nthegn is a worktree IDE.\n";
        let mq = "---\nid: merge-queue\ntitle: Merge queue\n---\n\
                  # Basics\nthe queue holds branches\n## Draining\n- run the drain command\n\
                  ```sh\nthegn integrate --all\n```\n";
        let (reg, errors) = HelpRegistry::build(&[index, mq], &[]);
        assert!(errors.is_empty(), "{errors:?}");
        reg.pages().to_vec()
    }

    #[test]
    fn empty_query_returns_nothing() {
        assert!(search(&pages(), "", &ranker).is_empty());
        assert!(search(&pages(), "   ", &ranker).is_empty());
    }

    #[test]
    fn title_and_body_scores_combine() {
        // Title-only hit: ranker's 10, doubled.
        let hits = search(&pages(), "merge", &ranker);
        assert_eq!((hits[0].page.as_str(), hits[0].score), ("merge-queue", 20));
        // Body-only hit.
        let hits = search(&pages(), "worktree", &ranker);
        assert_eq!(
            (hits[0].page.as_str(), hits[0].score),
            ("index", BODY_MATCH_SCORE)
        );
        // Both: 10×2 + 40, and the combined hit sorts first.
        let hits = search(&pages(), "queue", &ranker);
        assert_eq!((hits[0].page.as_str(), hits[0].score), ("merge-queue", 60));
    }

    #[test]
    fn body_match_yields_snippet_with_section() {
        let hits = search(&pages(), "drain command", &ranker);
        assert_eq!(hits.len(), 1);
        let snip = hits[0].snippet.as_ref().unwrap();
        assert_eq!(snip.text, "run the drain command");
        assert_eq!(snip.section.as_deref(), Some("Draining"));
        assert_eq!(&snip.text[..snip.hl_start], "run the ");
    }

    #[test]
    fn heading_match_snippets_the_heading_itself() {
        let hits = search(&pages(), "draining", &ranker);
        let snip = hits[0].snippet.as_ref().unwrap();
        assert_eq!(snip.text, "Draining");
        assert_eq!(
            snip.section.as_deref(),
            Some("Basics"),
            "section is the *preceding* heading"
        );
    }

    #[test]
    fn code_lines_are_searchable() {
        let hits = search(&pages(), "integrate --all", &ranker);
        assert_eq!(hits[0].page, "merge-queue");
        assert_eq!(
            hits[0].snippet.as_ref().unwrap().text,
            "thegn integrate --all"
        );
    }

    #[test]
    fn case_insensitive_and_multibyte_safe() {
        let hits = search(&pages(), "WORKTREE ide", &ranker);
        assert_eq!(hits[0].page, "index");
        assert_eq!(find_ci("naïve — Bold", "bold"), Some((8, 4)));
        assert_eq!(find_ci("short", "much longer needle"), None);
        assert_eq!(find_ci("anything", ""), None);
    }

    #[test]
    fn no_match_no_hit() {
        assert!(search(&pages(), "zebra unicycle", &ranker).is_empty());
    }

    #[test]
    fn out_of_range_ranker_indices_are_ignored() {
        let wild = |_: &str, _: &[&str]| vec![(999, 50u16)];
        assert!(search(&pages(), "merge", &wild).is_empty());
    }
}
