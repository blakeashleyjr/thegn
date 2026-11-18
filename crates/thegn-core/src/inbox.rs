//! The native inbox's pure core (THE-2214): item types, the sanitizer every
//! backend's output passes through, the notification `source_ref`/message
//! shape, and (b163 lane 5) the seen-set diff and rule facts.
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

/// Diff a page against the persisted seen-set. Stub: surfaces nothing and
/// keeps the seen-set (filled by b163 lane 5).
#[allow(dead_code, reason = "filled by b163 lane 5")]
pub fn diff(
    prev_seen: &[String],
    _baselined: bool,
    _digest_changed: bool,
    _page: &InboxPage,
) -> InboxDiff {
    InboxDiff {
        new: vec![],
        seen: prev_seen.to_vec(),
        baseline: false,
    }
}

/// The rule facts for an item. Stub: empty facts (filled by b163 lane 5).
#[allow(dead_code, reason = "filled by b163 lane 5")]
pub fn facts(_kind: &str, _account: &str, _item: &InboxItem) -> crate::automation::InboxFacts {
    crate::automation::InboxFacts::default()
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

    #[test]
    fn stubs_fail_closed() {
        let seen = vec!["a".to_owned()];
        let page = InboxPage {
            items: vec![item("b")],
            ..Default::default()
        };
        let d = diff(&seen, true, false, &page);
        assert!(d.new.is_empty() && !d.baseline);
        assert_eq!(d.seen, seen);
        assert_eq!(facts("command", "work", &item("b")), Default::default());
    }
}
