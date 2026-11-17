//! Which thread and which git subcommand spawn git processes.
//!
//! The global spawn counter says how many git processes start per second, not
//! who starts them. This module attributes spawns to `(thread-name prefix,
//! subcommand)` in a small fixed table that the perf rollup drains on its own
//! cadence. No thread, no timer: a spawn takes one short mutex.
//!
//! Labels never contain paths or values. The subcommand is kept only when it
//! looks like a git subcommand name (`[a-z0-9-]{1,24}`), otherwise `other`.

use std::sync::Mutex;

const CAPACITY: usize = 16;
const LABEL_CAP: usize = 24;

/// `(thread prefix, subcommand, count)`.
pub type SpawnEntry = (String, String, u64);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Slot {
    thread: String,
    sub: String,
    count: u64,
    overestimate: u64,
}

#[derive(Debug, Default)]
struct Table {
    slots: Vec<Slot>,
}

impl Table {
    const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    fn record(&mut self, thread: &str, sub: &str) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|s| s.thread == thread && s.sub == sub)
        {
            slot.count = slot.count.saturating_add(1);
            return;
        }
        let new = Slot {
            thread: thread.to_string(),
            sub: sub.to_string(),
            count: 1,
            overestimate: 0,
        };
        if self.slots.len() < CAPACITY {
            self.slots.push(new);
            return;
        }
        // Space-Saving: displace the minimum; its count bounds the error.
        if let Some(min) = self.slots.iter_mut().min_by_key(|s| s.count) {
            let floor = min.count;
            *min = Slot {
                count: floor.saturating_add(1),
                overestimate: floor,
                ..new
            };
        }
    }
}

static TABLE: Mutex<Table> = Mutex::new(Table::new());

/// Reduce git argv to a safe subcommand label. Leading options are skipped
/// (`-c`/`-C` consume the next arg); only a `[a-z0-9-]{1,24}` word survives.
pub fn subcommand_label(args: &[&str]) -> String {
    let mut it = args.iter();
    let mut first = None;
    while let Some(arg) = it.next() {
        if *arg == "-c" || *arg == "-C" {
            it.next();
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            continue;
        }
        first = Some(*arg);
        break;
    }
    match first {
        None => "?".to_string(),
        Some(word)
            if (1..=LABEL_CAP).contains(&word.len())
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') =>
        {
            word.to_string()
        }
        Some(_) => "other".to_string(),
    }
}

fn thread_prefix(name: Option<&str>) -> String {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return "?".to_string();
    };
    let cut = name
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(name.len());
    let prefix = name[..cut].trim_end_matches('-');
    if prefix.is_empty() {
        return "?".to_string();
    }
    prefix.chars().take(LABEL_CAP).collect()
}

/// Attribute one git spawn to the calling thread and the argv's subcommand.
pub fn record(args: &[&str]) {
    let thread = thread_prefix(std::thread::current().name());
    let sub = subcommand_label(args);
    TABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(&thread, &sub);
}

/// Swap the table out and return its entries, count descending (stable ties by
/// first-seen order).
pub fn take_window() -> Vec<SpawnEntry> {
    let table = std::mem::take(&mut *TABLE.lock().unwrap_or_else(|e| e.into_inner()));
    let mut out: Vec<SpawnEntry> = table
        .slots
        .into_iter()
        .map(|s| (s.thread, s.sub, s.count))
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.2));
    out
}

/// `"thread:sub xN, ..."` for the top `n` entries; empty when none.
pub fn top_text(entries: &[SpawnEntry], n: usize) -> String {
    entries
        .iter()
        .take(n)
        .map(|(t, s, c)| format!("{t}:{s} x{c}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    // best-effort: drop any previous window.
    drop(take_window());
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        g
    }

    #[test]
    fn subcommand_label_table() {
        assert_eq!(subcommand_label(&["status", "--porcelain"]), "status");
        assert_eq!(
            subcommand_label(&["-c", "diff.noprefix=false", "diff", "HEAD"]),
            "diff"
        );
        assert_eq!(
            subcommand_label(&["-C", "/x", "rev-parse", "HEAD"]),
            "rev-parse"
        );
        assert_eq!(subcommand_label(&["--no-pager", "log"]), "log");
        assert_eq!(subcommand_label(&["/abs/path"]), "other");
        assert_eq!(subcommand_label(&[]), "?");
        assert_eq!(subcommand_label(&["-c"]), "?");
        assert_eq!(subcommand_label(&["Status"]), "other");
        assert_eq!(subcommand_label(&[&"a".repeat(30)]), "other");
        assert_eq!(subcommand_label(&[""]), "other");
    }

    #[test]
    fn record_keys_on_thread_prefix() {
        let _g = guard();
        std::thread::Builder::new()
            .name("thegn-glyph-scan-3".into())
            .spawn(|| record(&["status"]))
            .unwrap()
            .join()
            .unwrap();
        let w = take_window();
        assert!(w.contains(&("thegn-glyph-scan".into(), "status".into(), 1)));
        assert!(!w.iter().any(|e| e.0.ends_with("-3")));
        assert!(take_window().iter().all(|e| e.0 != "thegn-glyph-scan"));
    }

    #[test]
    fn thread_prefix_cases() {
        assert_eq!(thread_prefix(None), "?");
        assert_eq!(thread_prefix(Some("")), "?");
        assert_eq!(thread_prefix(Some("7")), "?");
        assert_eq!(thread_prefix(Some("thegn-rt")), "thegn-rt");
        assert_eq!(thread_prefix(Some(&"x".repeat(40))).len(), 24);
    }

    #[test]
    fn eviction_is_monotone_and_marks_overestimate() {
        let mut t = Table::new();
        for i in 0..CAPACITY {
            t.record("t", &format!("s{i}"));
        }
        t.record("t", "s0");
        t.record("t", "new");
        assert_eq!(t.slots.len(), CAPACITY);
        let new = t.slots.iter().find(|s| s.sub == "new").unwrap();
        assert_eq!(new.count, 2);
        assert_eq!(new.overestimate, 1);
        assert!(t.slots.iter().any(|s| s.sub == "s0" && s.count == 2));
    }

    #[test]
    fn top_text_orders_with_stable_ties() {
        let mut t = Table::new();
        for (sub, n) in [("b", 2), ("a", 1), ("c", 1), ("top", 3)] {
            for _ in 0..n {
                t.record("t", sub);
            }
        }
        let mut out: Vec<SpawnEntry> = t
            .slots
            .into_iter()
            .map(|s| (s.thread, s.sub, s.count))
            .collect();
        out.sort_by_key(|e| std::cmp::Reverse(e.2));
        let names: Vec<_> = out.iter().map(|e| e.1.as_str()).collect();
        assert_eq!(names, ["top", "b", "a", "c"]);
        assert_eq!(top_text(&out, 2), "t:top x3, t:b x2");
        assert_eq!(top_text(&[], 3), "");
    }
}
