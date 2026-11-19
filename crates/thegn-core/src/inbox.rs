//! The native inbox's pure core (THE-2214): item types, the sanitizer every
//! backend's output passes through, the notification `source_ref`/message
//! shape, and the seen-set diff and rule facts.
//!
//! Everything an inbox backend returns is third-party content. Titles,
//! senders, labels, URLs and ids pass [`sanitize_item`] before they are
//! stored, shown or turned into rule facts: control, bidi and zero-width
//! characters are stripped, whitespace is collapsed, and every field is
//! capped. Item text never becomes argv, env, a path, a scope, a rule, an
//! autonomy level or an approval.

use serde::{Deserialize, Serialize};

/// One inbox item as a backend reports it (before or after sanitizing).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InboxItem {
    /// The backend's stable id for the item.
    pub id: String,
    pub title: String,
    pub url: String,
    /// Sender or author, display form.
    pub from: String,
    pub labels: Vec<String>,
    /// Due date or time, as the backend spells it (capped at 32 chars).
    pub due: Option<String>,
    /// Unix seconds of the item's last update, when the backend knows it.
    pub updated_at: Option<i64>,
    /// Backend priority (higher is more urgent), when it has one.
    pub priority: Option<u8>,
}

/// Whether a page is the whole current set or a change set since `cursor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageMode {
    /// `items` is everything currently in the inbox (up to the cap).
    #[default]
    Snapshot,
    /// `items` are additions/changes and `removed` the ids that left.
    Delta,
}

/// One fetch's result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InboxPage {
    pub items: Vec<InboxItem>,
    /// Ids that left the inbox (`Delta` pages only).
    pub removed: Vec<String>,
    /// The next cursor to persist, when the backend has one.
    pub cursor: Option<String>,
    pub mode: PageMode,
    /// The backend had more than `max_items`.
    pub truncated: bool,
}

pub const MAX_ITEM_ID_CHARS: usize = 128;
pub const MAX_TITLE_CHARS: usize = 300;
pub const MAX_FROM_CHARS: usize = 120;
pub const MAX_URL_CHARS: usize = 2048;
pub const MAX_LABELS: usize = 16;
pub const MAX_LABEL_CHARS: usize = 64;
/// The bound on an account's persisted seen-set.
pub const MAX_SEEN: usize = 2000;
/// The most items a baseline poll surfaces as notification rows.
pub const FIRST_POLL_SURFACE: usize = 20;
/// The longest `due` string kept.
const MAX_DUE_CHARS: usize = 32;

/// Characters stripped outright: C0/C1 controls (whitespace controls become a
/// space first, see [`sanitize_text`]), zero-width and directional marks,
/// bidi embeddings/overrides/isolates, and the BOM.
fn stripped(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
        )
}

/// Strip control, bidi and zero-width characters, collapse every whitespace
/// run to one space, trim, and cap at `max_chars` characters.
pub fn sanitize_text(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 4));
    let mut pending_space = false;
    let mut count = 0usize;
    for c in s.chars() {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if stripped(c) {
            continue;
        }
        if pending_space && count > 0 {
            if count + 1 >= max_chars {
                break;
            }
            out.push(' ');
            count += 1;
        }
        pending_space = false;
        if count >= max_chars {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// True for an id made only of `[A-Za-z0-9._:@+=-]`, 1..=128 characters.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ITEM_ID_CHARS
        && id.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '@' | '+' | '=' | '-')
        })
}

/// The URL kept only when it is http(s), has a host, contains no whitespace
/// or control characters, and fits the cap; otherwise empty.
fn sanitize_url(url: &str) -> String {
    let url = url.trim();
    if url.is_empty()
        || url.chars().count() > MAX_URL_CHARS
        || url.chars().any(|c| c.is_whitespace() || stripped(c))
    {
        return String::new();
    }
    match url::Url::parse(url) {
        Ok(parsed)
            if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() =>
        {
            url.to_owned()
        }
        _ => String::new(),
    }
}

/// Sanitize one backend item. `None` when its id is outside the id charset
/// (the item is dropped). Title, sender and labels are capped (empty labels
/// dropped, at most [`MAX_LABELS`]); a non-http(s) or over-long URL is
/// cleared; `due` is capped at 32 characters and dropped when empty.
pub fn sanitize_item(raw: InboxItem) -> Option<InboxItem> {
    let id = raw.id.trim();
    if !valid_id(id) {
        return None;
    }
    let labels = raw
        .labels
        .iter()
        .map(|label| sanitize_text(label, MAX_LABEL_CHARS))
        .filter(|label| !label.is_empty())
        .take(MAX_LABELS)
        .collect();
    let due = raw
        .due
        .as_deref()
        .map(|due| sanitize_text(due, MAX_DUE_CHARS))
        .filter(|due| !due.is_empty());
    Some(InboxItem {
        id: id.to_owned(),
        title: sanitize_text(&raw.title, MAX_TITLE_CHARS),
        url: sanitize_url(&raw.url),
        from: sanitize_text(&raw.from, MAX_FROM_CHARS),
        labels,
        due,
        updated_at: raw.updated_at,
        priority: raw.priority,
    })
}

const SOURCE_PREFIX: &str = "inbox:";

/// The notification `source_ref` (`issue_id` column) of an inbox item:
/// `inbox:<account>:<id>`.
pub fn source_ref(account: &str, item_id: &str) -> String {
    format!("{SOURCE_PREFIX}{account}:{item_id}")
}

/// Inverse of [`source_ref`]: `(account, item_id)`, or `None` when `s` is not
/// an inbox source ref. Account names hold no `:`, so the first one after
/// the prefix splits; the id may contain more.
pub fn parse_source_ref(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix(SOURCE_PREFIX)?;
    let (account, id) = rest.split_once(':')?;
    (!account.is_empty() && !id.is_empty()).then_some((account, id))
}

/// The notification message for an item: `[<account>] <title>`, the title
/// sanitized again here, `(no title)` when it is empty.
pub fn message(account: &str, item: &InboxItem) -> String {
    let title = sanitize_text(&item.title, MAX_TITLE_CHARS);
    if title.is_empty() {
        format!("[{account}] (no title)")
    } else {
        format!("[{account}] {title}")
    }
}

/// What one poll changes: the items to surface, the next seen-set, and
/// whether this poll was a baseline (no rule events).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InboxDiff {
    pub new: Vec<InboxItem>,
    pub seen: Vec<String>,
    pub baseline: bool,
}

/// The page's items, de-duplicated by id (first occurrence wins), newest
/// first: `updated_at` descending (unknown last), then page order.
fn newest_first(items: &[InboxItem]) -> Vec<&InboxItem> {
    let mut ids = std::collections::HashSet::new();
    let mut out: Vec<&InboxItem> = items.iter().filter(|i| ids.insert(i.id.as_str())).collect();
    // Stable: equal keys keep page order.
    out.sort_by(|a, b| {
        b.updated_at
            .unwrap_or(i64::MIN)
            .cmp(&a.updated_at.unwrap_or(i64::MIN))
    });
    out
}

/// Keep the newest [`MAX_SEEN`] ids (the tail).
fn cap_seen(mut seen: Vec<String>) -> Vec<String> {
    let skip = seen.len().saturating_sub(MAX_SEEN);
    seen.drain(..skip);
    seen
}

/// Diff a page against the persisted seen-set (oldest id first).
///
/// - A baseline (`!baselined`, or the account's `config_digest` changed):
///   `new` is the [`FIRST_POLL_SURFACE`] newest items and `baseline` is set,
///   so the caller writes rows but submits no rule events. The previous
///   seen-set is discarded (a digest change means a different source).
/// - Otherwise `new` is every item whose id is not in `prev_seen` (and, for a
///   `Delta` page, not in `removed`). An item that left the set and came back
///   is new again.
/// - `seen`: a `Snapshot` page's ids; for a `Delta` page, the previous set
///   minus `removed` plus the page's ids. Oldest first, capped at
///   [`MAX_SEEN`] with the newest kept.
pub fn diff(
    prev_seen: &[String],
    baselined: bool,
    digest_changed: bool,
    page: &InboxPage,
) -> InboxDiff {
    use std::collections::HashSet;
    let baseline = !baselined || digest_changed;
    let ordered = newest_first(&page.items);
    let removed: HashSet<&str> = page.removed.iter().map(String::as_str).collect();
    let prev: HashSet<&str> = if baseline {
        HashSet::new()
    } else {
        prev_seen.iter().map(String::as_str).collect()
    };
    let new: Vec<InboxItem> = if baseline {
        ordered
            .iter()
            .take(FIRST_POLL_SURFACE)
            .map(|i| (*i).clone())
            .collect()
    } else {
        ordered
            .iter()
            .filter(|i| !prev.contains(i.id.as_str()))
            .filter(|i| page.mode == PageMode::Snapshot || !removed.contains(i.id.as_str()))
            .map(|i| (*i).clone())
            .collect()
    };
    // The page's ids, oldest first, so the cap keeps the newest.
    let page_ids = ordered.iter().rev().map(|i| i.id.clone());
    let seen = match page.mode {
        PageMode::Snapshot => page_ids.collect(),
        PageMode::Delta => {
            let base = if baseline { &[][..] } else { prev_seen };
            let mut seen: Vec<String> = base
                .iter()
                .filter(|id| !removed.contains(id.as_str()))
                .cloned()
                .collect();
            let mut have: HashSet<String> = seen.iter().cloned().collect();
            for id in page_ids {
                if !removed.contains(id.as_str()) && have.insert(id.clone()) {
                    seen.push(id);
                }
            }
            seen
        }
    };
    InboxDiff {
        new,
        seen: cap_seen(seen),
        baseline,
    }
}

/// The rule facts for an already-sanitized item. `title` is sanitized and
/// capped again here; it reaches rules only as `message`.
pub fn facts(kind: &str, account: &str, item: &InboxItem) -> crate::automation::InboxFacts {
    crate::automation::InboxFacts {
        kind: kind.to_owned(),
        account: account.to_owned(),
        item_id: item.id.clone(),
        url: item.url.clone(),
        labels: item.labels.clone(),
        title: sanitize_text(&item.title, MAX_TITLE_CHARS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> InboxItem {
        InboxItem {
            id: id.into(),
            title: "Hello".into(),
            ..Default::default()
        }
    }

    #[test]
    fn sanitize_text_strips_bidi_zero_width_and_controls() {
        assert_eq!(
            sanitize_text("a\u{202E}b\u{200B}c\u{2066}d\u{FEFF}e\u{200F}f", 100),
            "abcdef"
        );
        assert_eq!(sanitize_text("x\u{1b}[31my\u{7}\u{9b}z", 100), "x[31myz");
        assert_eq!(sanitize_text("  a \t\n b\r\n\nc  ", 100), "a b c");
        assert_eq!(sanitize_text("\u{2028}a\u{00A0}\u{3000}b", 100), "a b");
        assert_eq!(sanitize_text("", 10), "");
        assert_eq!(sanitize_text(" \u{200B} ", 10), "");
    }

    #[test]
    fn sanitize_text_caps_by_characters() {
        assert_eq!(sanitize_text("abcdef", 3), "abc");
        assert_eq!(sanitize_text("ééééé", 2), "éé");
        assert_eq!(sanitize_text("ab cd", 3), "ab");
        assert_eq!(sanitize_text("ab cd", 4), "ab c");
        assert_eq!(sanitize_text("x", 0), "");
        let long = "w ".repeat(1000);
        assert!(sanitize_text(&long, MAX_TITLE_CHARS).chars().count() <= MAX_TITLE_CHARS);
    }

    #[test]
    fn sanitize_item_checks_the_id_charset() {
        let longest = "x".repeat(128);
        let too_long = "x".repeat(129);
        for good in ["a", "A-z_0.9:@+=", "msg:123@host", longest.as_str()] {
            assert_eq!(sanitize_item(item(good)).unwrap().id, good);
        }
        assert_eq!(sanitize_item(item("  padded ")).unwrap().id, "padded");
        for bad in [
            "",
            "a b",
            "a/b",
            "a\u{202E}b",
            "é",
            "a\nb",
            "a;b",
            too_long.as_str(),
        ] {
            assert!(sanitize_item(item(bad)).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn sanitize_item_caps_and_cleans_every_field() {
        let raw = InboxItem {
            id: "1".into(),
            title: format!("\u{202E}evil\u{200B} {}", "t".repeat(400)),
            url: "https://example.com/x?y=1".into(),
            from: format!("Bob\u{7}{}", "f".repeat(200)),
            labels: (0..20)
                .map(|i| format!("l{i}\u{200D}{}", "x".repeat(100)))
                .chain(["  ".into()])
                .collect(),
            due: Some(format!("2026-11-18 {}", "z".repeat(50))),
            updated_at: Some(7),
            priority: Some(4),
        };
        let clean = sanitize_item(raw).unwrap();
        assert!(clean.title.starts_with("evil t"));
        assert_eq!(clean.title.chars().count(), MAX_TITLE_CHARS);
        assert!(clean.from.starts_with("Bobf"));
        assert_eq!(clean.from.chars().count(), MAX_FROM_CHARS);
        assert_eq!(clean.labels.len(), MAX_LABELS);
        assert!(
            clean
                .labels
                .iter()
                .all(|l| l.chars().count() <= MAX_LABEL_CHARS)
        );
        assert!(clean.labels.iter().all(|l| !l.contains('\u{200D}')));
        assert_eq!(clean.due.as_deref().map(|d| d.chars().count()), Some(32));
        assert_eq!(clean.url, "https://example.com/x?y=1");
        assert_eq!((clean.updated_at, clean.priority), (Some(7), Some(4)));
        let blank_due = sanitize_item(InboxItem {
            due: Some(" \u{200B}".into()),
            ..item("2")
        })
        .unwrap();
        assert_eq!(blank_due.due, None);
    }

    #[test]
    fn sanitize_item_keeps_only_http_urls() {
        for (url, kept) in [
            ("http://example.com", true),
            ("https://example.com/a#b", true),
            ("javascript:alert(1)", false),
            ("file:///etc/passwd", false),
            ("data:text/html,x", false),
            ("mailto:a@b", false),
            ("https://exa mple.com", false),
            ("https://example.com/\u{202E}x", false),
            ("not a url", false),
            ("", false),
        ] {
            let clean = sanitize_item(InboxItem {
                url: url.into(),
                ..item("1")
            })
            .unwrap();
            assert_eq!(clean.url.is_empty(), !kept, "{url:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        let clean = sanitize_item(InboxItem {
            url: long,
            ..item("1")
        })
        .unwrap();
        assert!(clean.url.is_empty(), "over the cap");
    }

    #[test]
    fn source_ref_round_trips() {
        let s = source_ref("work", "msg:12@host");
        assert_eq!(s, "inbox:work:msg:12@host");
        assert_eq!(parse_source_ref(&s), Some(("work", "msg:12@host")));
        for bad in [
            "work:1",
            "inbox:",
            "inbox:work",
            "inbox::1",
            "inbox:work:",
            "issue:x:y",
        ] {
            assert_eq!(parse_source_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn message_is_account_tagged_and_sanitized() {
        assert_eq!(message("work", &item("1")), "[work] Hello");
        let untitled = InboxItem {
            title: " \u{200B}\n".into(),
            ..item("1")
        };
        assert_eq!(message("work", &untitled), "[work] (no title)");
        let sneaky = InboxItem {
            title: "a\u{202E}\nb".into(),
            ..item("1")
        };
        assert_eq!(message("m", &sneaky), "[m] a b");
    }

    #[test]
    fn page_defaults_and_mode_serde() {
        let page = InboxPage::default();
        assert_eq!(page.mode, PageMode::Snapshot);
        assert!(!page.truncated && page.items.is_empty() && page.cursor.is_none());
        assert_eq!(
            serde_json::to_string(&PageMode::Delta).unwrap(),
            "\"delta\""
        );
        let parsed: InboxItem = serde_json::from_str(r#"{"id":"1","title":"t","url":"","from":"","labels":[],"due":null,"updated_at":null,"priority":null}"#).unwrap();
        assert_eq!(parsed.id, "1");
    }

    fn at(id: &str, updated_at: i64) -> InboxItem {
        InboxItem {
            updated_at: Some(updated_at),
            ..item(id)
        }
    }

    fn snapshot(items: Vec<InboxItem>) -> InboxPage {
        InboxPage {
            items,
            ..Default::default()
        }
    }

    fn ids(items: &[InboxItem]) -> Vec<&str> {
        items.iter().map(|i| i.id.as_str()).collect()
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn diff_baseline_surfaces_at_most_twenty_newest() {
        let items: Vec<_> = (0..30).map(|i| at(&format!("i{i}"), i)).collect();
        let d = diff(&[], false, false, &snapshot(items));
        assert!(d.baseline);
        assert_eq!(d.new.len(), FIRST_POLL_SURFACE);
        assert_eq!(d.new[0].id, "i29", "newest first");
        assert_eq!(d.new[19].id, "i10");
        assert_eq!(d.seen.len(), 30, "the whole page is seen");
        assert_eq!(d.seen.last().map(String::as_str), Some("i29"));
        // Unknown updated_at sorts last, page order breaks ties.
        let d = diff(
            &[],
            false,
            false,
            &snapshot(vec![item("x"), at("y", 5), item("z")]),
        );
        assert_eq!(ids(&d.new), ["y", "x", "z"]);
    }

    #[test]
    fn diff_snapshot_new_and_removed_items() {
        let prev = strs(&["a", "b"]);
        let d = diff(&prev, true, false, &snapshot(vec![item("a"), item("c")]));
        assert!(!d.baseline);
        assert_eq!(ids(&d.new), ["c"]);
        let mut seen = d.seen.clone();
        seen.sort();
        assert_eq!(seen, strs(&["a", "c"]), "b left the set");
        // b comes back: new again.
        let d = diff(
            &d.seen,
            true,
            false,
            &snapshot(vec![item("a"), item("b"), item("c")]),
        );
        assert_eq!(ids(&d.new), ["b"]);
        // Duplicate ids in a page count once.
        let d = diff(&[], true, false, &snapshot(vec![item("d"), item("d")]));
        assert_eq!(ids(&d.new), ["d"]);
        assert_eq!(d.seen, strs(&["d"]));
    }

    #[test]
    fn diff_delta_keeps_the_previous_set() {
        let prev = strs(&["a", "b"]);
        let page = InboxPage {
            items: vec![item("b"), item("c"), item("x")],
            removed: vec!["a".into(), "x".into()],
            mode: PageMode::Delta,
            ..Default::default()
        };
        let d = diff(&prev, true, false, &page);
        assert!(!d.baseline);
        assert_eq!(ids(&d.new), ["c"], "b seen, x removed");
        assert_eq!(d.seen, strs(&["b", "c"]));
    }

    #[test]
    fn diff_a_digest_change_rebaselines() {
        let prev = strs(&["a"]);
        let d = diff(&prev, true, true, &snapshot(vec![item("a"), item("b")]));
        assert!(d.baseline);
        assert_eq!(ids(&d.new), ["a", "b"], "the old set is discarded");
        let delta = InboxPage {
            items: vec![item("b")],
            mode: PageMode::Delta,
            ..Default::default()
        };
        let d = diff(&prev, true, true, &delta);
        assert_eq!(d.seen, strs(&["b"]), "a different source: prev dropped");
    }

    #[test]
    fn diff_seen_is_capped_newest_kept() {
        let prev: Vec<String> = (0..MAX_SEEN).map(|i| format!("p{i}")).collect();
        let page = InboxPage {
            items: vec![item("n1"), item("n2")],
            mode: PageMode::Delta,
            ..Default::default()
        };
        let d = diff(&prev, true, false, &page);
        assert_eq!(d.new.len(), 2);
        assert_eq!(d.seen.len(), MAX_SEEN);
        assert_eq!(d.seen.first().map(String::as_str), Some("p2"));
        assert!(d.seen.iter().any(|s| s == "n1") && d.seen.iter().any(|s| s == "n2"));
    }

    #[test]
    fn facts_copy_the_item_and_cap_the_title() {
        let it = InboxItem {
            id: "7".into(),
            title: format!("t\u{202E}{}", "x".repeat(400)),
            url: "https://e.com".into(),
            labels: vec!["l".into()],
            ..Default::default()
        };
        let f = facts("command", "work", &it);
        assert_eq!(
            (f.kind.as_str(), f.account.as_str(), f.item_id.as_str()),
            ("command", "work", "7")
        );
        assert_eq!(f.url, "https://e.com");
        assert_eq!(f.labels, ["l"]);
        assert_eq!(f.title.chars().count(), MAX_TITLE_CHARS);
        assert!(!f.title.contains('\u{202E}'));
    }
}
