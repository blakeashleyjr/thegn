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

/// How many body matches one page may contribute. A page that mentions a term
/// ten times should be reachable at each mention, but not flood the results.
pub const MAX_SNIPPETS_PER_PAGE: usize = 4;

/// Body matches in document order, tracking the enclosing section, capped at
/// `cap`. Returning several (rather than only the first) is what lets a long
/// page be opened at the mention you actually wanted.
fn body_snippets(page: &HelpPage, query: &str, cap: usize) -> Vec<Snippet> {
    let mut out: Vec<Snippet> = Vec::new();
    let mut section: Option<String> = None;
    let check = |text: String, section: &Option<String>, out: &mut Vec<Snippet>| {
        if out.len() < cap
            && let Some((hl_start, hl_len)) = find_ci(&text, query)
        {
            out.push(Snippet {
                text,
                hl_start,
                hl_len,
                section: section.clone(),
            });
        }
    };
    for block in &page.blocks {
        if out.len() >= cap {
            break;
        }
        match block {
            Block::Heading { spans, .. } => {
                let text = plain(spans);
                check(text.clone(), &section, &mut out);
                section = Some(text);
            }
            Block::Para(spans) | Block::Quote(spans) => check(plain(spans), &section, &mut out),
            Block::List(items) => {
                for item in items {
                    check(plain(&item.spans), &section, &mut out);
                }
            }
            Block::Code { text, .. } => {
                for line in text.lines() {
                    check(line.to_string(), &section, &mut out);
                }
            }
            // Each row is one snippet line — a table row reads as a unit
            // ("`Alt-n` — Split pane down"), which is what a hit should show.
            Block::Table { header, rows } => {
                for row in std::iter::once(header).chain(rows.iter()) {
                    let line = row.iter().map(|c| plain(c)).collect::<Vec<_>>().join(" — ");
                    check(line, &section, &mut out);
                }
            }
            Block::Rule => {}
        }
    }
    out
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
    let mut hits: Vec<SearchHit> = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let snippets = body_snippets(page, query, MAX_SNIPPETS_PER_PAGE);
        let title = title_score[i].saturating_mul(2);
        if snippets.is_empty() {
            if title > 0 {
                hits.push(SearchHit {
                    page: page.meta.id.clone(),
                    title: page.meta.title.clone(),
                    score: title,
                    snippet: None,
                });
            }
            continue;
        }
        // One hit per match, so each mention is separately openable. Only the
        // first carries the title bonus — later mentions on the same page rank
        // below a fresh page's first match rather than crowding it out.
        for (n, snippet) in snippets.into_iter().enumerate() {
            hits.push(SearchHit {
                page: page.meta.id.clone(),
                title: page.meta.title.clone(),
                score: if n == 0 {
                    title + BODY_MATCH_SCORE
                } else {
                    BODY_MATCH_SCORE.saturating_sub(n as u32)
                },
                snippet: Some(snippet),
            });
        }
    }
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

    /// A page mentioning the query several times yields one hit per mention,
    /// each separately openable — not just the first.
    #[test]
    fn repeated_mentions_yield_one_hit_each() {
        let p = "---\nid: index\ntitle: Welcome\n---\n\
                 # A\nqueue one\n## B\nqueue two\n## C\nqueue three\n";
        let (reg, errors) = HelpRegistry::build(&[p], &[]);
        assert!(errors.is_empty(), "{errors:?}");
        let hits = search(reg.pages(), "queue", &ranker);
        assert_eq!(hits.len(), 3, "{hits:?}");
        let texts: Vec<&str> = hits
            .iter()
            .map(|h| h.snippet.as_ref().unwrap().text.as_str())
            .collect();
        assert_eq!(texts, ["queue one", "queue two", "queue three"]);
        // Each carries its own section, so `↵` jumps to the right one.
        let sections: Vec<Option<&str>> = hits
            .iter()
            .map(|h| h.snippet.as_ref().unwrap().section.as_deref())
            .collect();
        assert_eq!(sections, [Some("A"), Some("B"), Some("C")]);
        // The first mention ranks highest; later ones descend.
        assert!(hits[0].score > hits[1].score && hits[1].score > hits[2].score);
    }

    /// The per-page cap keeps one page from flooding the result list.
    #[test]
    fn snippets_per_page_are_capped() {
        let body: String = (0..20).map(|i| format!("queue line {i}\n\n")).collect();
        let p = format!("---\nid: index\ntitle: Welcome\n---\n{body}");
        let (reg, _) = HelpRegistry::build(&[&p], &[]);
        let hits = search(reg.pages(), "queue", &ranker);
        assert_eq!(hits.len(), MAX_SNIPPETS_PER_PAGE);
    }

    /// A title-only match (no body mention) still yields exactly one hit.
    #[test]
    fn title_only_match_yields_a_single_snippetless_hit() {
        let hits = search(&pages(), "Merge", &ranker);
        let mq: Vec<&SearchHit> = hits.iter().filter(|h| h.page == "merge-queue").collect();
        assert_eq!(mq.len(), 1);
        assert!(mq[0].snippet.is_none(), "no body mention of 'Merge'");
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
