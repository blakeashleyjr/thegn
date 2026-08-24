//! Token rollups from the AI harnesses' local transcripts — the pure parser.
//!
//! **These totals are host-wide and cannot be attributed to an account.** Two
//! independent reasons, and either alone would be enough: transcript records
//! carry no account, org, or credential-home field; and several credential homes
//! routinely *share* one transcript directory (Claude Code profiles commonly
//! symlink `projects/` at a single tree). So a rollup describes a machine, not a
//! login. The per-account percentages in [`crate::usage`] come from the
//! providers themselves and are per-account accurate; these are a separate,
//! coarser number, and every surface that shows them says so.
//!
//! Two dedup traps live here, both of which inflate totals badly if missed:
//!
//! 1. **Streaming re-emits.** The same `requestId` appears on consecutive lines
//!    carrying identical usage. Counting each line double- or triple-counts
//!    every response.
//! 2. **`usage.iterations[]`.** Multi-iteration responses restate the same
//!    counters in a nested array. Summing it counts each response twice over.
//!    This parser reads only the top-level counters and never descends.

use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};

/// One bucket's token counters. Claude reports cache reads and cache writes
/// separately from plain input, and they price and behave differently, so they
/// are kept apart rather than folded into one "input" number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscriptTokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub thinking: u64,
}

impl TranscriptTokens {
    /// Everything billed as input: fresh input plus both cache paths.
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_read + self.cache_creation
    }

    /// Every token in the bucket.
    pub fn total(&self) -> u64 {
        self.total_input() + self.output
    }

    fn add(&mut self, o: &TranscriptTokens) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_creation += o.cache_creation;
        self.thinking += o.thinking;
    }
}

/// A host-wide rollup, bucketed three ways. `BTreeMap` so iteration order is
/// stable — a rollup rendered in chrome must not reshuffle between polls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenRollup {
    /// `"2026-08-22"` → totals.
    pub by_day: BTreeMap<String, TranscriptTokens>,
    /// Model id → totals.
    pub by_model: BTreeMap<String, TranscriptTokens>,
    /// Project slug → totals.
    pub by_project: BTreeMap<String, TranscriptTokens>,
    pub total: TranscriptTokens,
    /// Distinct API responses counted (post-dedup), for a "from N responses"
    /// footnote and as the honest signal that a rollup is thin.
    pub records: usize,
}

impl TokenRollup {
    fn record(&mut self, day: &str, model: &str, project: &str, t: &TranscriptTokens) {
        self.total.add(t);
        self.records += 1;
        for (map, key) in [
            (&mut self.by_day, day),
            (&mut self.by_model, model),
            (&mut self.by_project, project),
        ] {
            if !key.is_empty() {
                map.entry(key.to_string()).or_default().add(t);
            }
        }
    }

    /// Fold another rollup in (merging per-file or per-home results).
    pub fn merge(&mut self, other: &TokenRollup) {
        self.total.add(&other.total);
        self.records += other.records;
        for (map, src) in [
            (&mut self.by_day, &other.by_day),
            (&mut self.by_model, &other.by_model),
            (&mut self.by_project, &other.by_project),
        ] {
            for (k, v) in src {
                map.entry(k.clone()).or_default().add(v);
            }
        }
    }

    /// The busiest buckets first, capped at `n` — what a narrow chrome column
    /// can actually show.
    pub fn top_models(&self, n: usize) -> Vec<(String, TranscriptTokens)> {
        top(&self.by_model, n)
    }

    /// As [`TokenRollup::top_models`], for projects.
    pub fn top_projects(&self, n: usize) -> Vec<(String, TranscriptTokens)> {
        top(&self.by_project, n)
    }

    /// Day buckets in chronological order — a trend, so it must not be sorted
    /// by size the way the others are.
    pub fn days(&self) -> Vec<(String, TranscriptTokens)> {
        self.by_day.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

fn top(map: &BTreeMap<String, TranscriptTokens>, n: usize) -> Vec<(String, TranscriptTokens)> {
    let mut v: Vec<(String, TranscriptTokens)> = map.iter().map(|(k, t)| (k.clone(), *t)).collect();
    // Descending by size, then by key: without the tiebreak two equal buckets
    // could swap places between polls and flicker.
    v.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then(a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

// --- line shape -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Line {
    #[serde(default, rename = "requestId")]
    request_id: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// Only the TOP-LEVEL counters. `iterations[]` is deliberately not a field here:
/// it restates these same numbers, and summing it double-counts.
#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Debug, Deserialize)]
struct OutputDetails {
    #[serde(default)]
    thinking_tokens: Option<u64>,
}

/// The `YYYY-MM-DD` prefix of an ISO-8601 stamp. Deliberately a slice rather
/// than a parse: the day bucket needs no calendar arithmetic, and a chrono
/// dependency here would drag a clock into a pure module.
fn day_of(ts: &str) -> String {
    match ts.len() >= 10 && ts.is_char_boundary(10) {
        true => ts[..10].to_string(),
        false => String::new(),
    }
}

/// The project bucket for a record: the working directory's last path segment,
/// falling back to the caller's `default_project` (the transcript's own
/// directory name) when the record carries no `cwd`.
fn project_of(cwd: Option<&str>, default_project: &str) -> String {
    cwd.and_then(|c| {
        c.trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
    .unwrap_or_else(|| default_project.to_string())
}

/// The dedup identity of a record. `requestId` is the API request; pairing it
/// with the message id (as ccusage does) also separates two responses that a
/// harness reported under one request. Records with neither can't be deduped
/// and are counted once each.
fn dedup_key(l: &Line, msg: &Message) -> Option<String> {
    match (l.request_id.as_deref(), msg.id.as_deref()) {
        (None, None) => l.uuid.clone(),
        (r, m) => Some(format!("{}|{}", r.unwrap_or("-"), m.unwrap_or("-"))),
    }
}

/// Fold one transcript file into `acc`, skipping records whose identity is
/// already in `seen`.
///
/// `seen` is threaded across files by the caller: the same response can appear
/// in more than one transcript (a resumed session), and per-file dedup would
/// miss that.
pub fn fold_transcript(
    bytes: &[u8],
    default_project: &str,
    seen: &mut HashSet<String>,
    acc: &mut TokenRollup,
) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            continue; // a truncated tail line is normal on a live transcript
        };
        let Some(msg) = &l.message else { continue };
        let Some(u) = &msg.usage else { continue };
        if let Some(key) = dedup_key(&l, msg)
            && !seen.insert(key)
        {
            continue; // streaming re-emit of a response already counted
        }
        let t = TranscriptTokens {
            input: u.input_tokens.unwrap_or(0),
            output: u.output_tokens.unwrap_or(0),
            cache_read: u.cache_read_input_tokens.unwrap_or(0),
            cache_creation: u.cache_creation_input_tokens.unwrap_or(0),
            thinking: u
                .output_tokens_details
                .as_ref()
                .and_then(|d| d.thinking_tokens)
                .unwrap_or(0),
        };
        // A record with no counters at all is not a response worth a bucket.
        if t.total() == 0 {
            continue;
        }
        acc.record(
            &l.timestamp.as_deref().map(day_of).unwrap_or_default(),
            msg.model.as_deref().unwrap_or_default(),
            &project_of(l.cwd.as_deref(), default_project),
            &t,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(lines: &[&str]) -> TokenRollup {
        let mut acc = TokenRollup::default();
        let mut seen = HashSet::new();
        fold_transcript(lines.join("\n").as_bytes(), "fallback", &mut seen, &mut acc);
        acc
    }

    fn record(req: &str, id: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"requestId":"{req}","timestamp":"2026-08-22T10:00:00.000Z","cwd":"/home/u/code/thegn",
               "message":{{"id":"{id}","model":"claude-opus-4-8","usage":{{
                 "input_tokens":{input},"output_tokens":{output},
                 "cache_read_input_tokens":100,"cache_creation_input_tokens":50,
                 "output_tokens_details":{{"thinking_tokens":7}}}}}}}}"#
        )
        .replace('\n', "")
    }

    #[test]
    fn counters_are_bucketed_three_ways() {
        let r = fold(&[&record("r1", "m1", 10, 20)]);
        assert_eq!(r.records, 1);
        assert_eq!(r.total.input, 10);
        assert_eq!(r.total.output, 20);
        assert_eq!(r.total.cache_read, 100);
        assert_eq!(r.total.cache_creation, 50);
        assert_eq!(r.total.thinking, 7);
        // Cache reads and writes are input, and are kept distinct from it.
        assert_eq!(r.total.total_input(), 160);
        assert_eq!(r.total.total(), 180);
        assert_eq!(r.by_day.keys().collect::<Vec<_>>(), ["2026-08-22"]);
        assert_eq!(r.by_model.keys().collect::<Vec<_>>(), ["claude-opus-4-8"]);
        // The project is the cwd's last segment, not the whole path.
        assert_eq!(r.by_project.keys().collect::<Vec<_>>(), ["thegn"]);
    }

    #[test]
    fn streaming_re_emits_are_counted_once() {
        // The SAME requestId + message id on consecutive lines is one response
        // re-emitted, not two responses. Counting both doubles every total.
        let dup = record("r1", "m1", 10, 20);
        let r = fold(&[&dup, &dup, &dup, &record("r2", "m2", 1, 2)]);
        assert_eq!(r.records, 2);
        assert_eq!(r.total.input, 11);
        assert_eq!(r.total.output, 22);
    }

    #[test]
    fn two_responses_under_one_request_are_both_counted() {
        // A shared requestId with distinct message ids is two real responses —
        // deduping on requestId alone would lose one.
        let r = fold(&[&record("r1", "m1", 10, 20), &record("r1", "m2", 5, 6)]);
        assert_eq!(r.records, 2);
        assert_eq!(r.total.input, 15);
    }

    #[test]
    fn nested_iterations_are_never_summed() {
        // `iterations[]` restates the top-level counters. If this parser ever
        // grows a field for it, this total doubles and the test fails.
        let line = r#"{"requestId":"r1","timestamp":"2026-08-22T00:00:00Z",
            "message":{"id":"m1","model":"x","usage":{"input_tokens":100,"output_tokens":10,
              "iterations":[{"input_tokens":100,"output_tokens":10},
                            {"input_tokens":100,"output_tokens":10}]}}}"#
            .replace('\n', "");
        let r = fold(&[&line]);
        assert_eq!(r.total.input, 100);
        assert_eq!(r.total.output, 10);
    }

    #[test]
    fn dedup_carries_across_files() {
        // A resumed session re-writes earlier responses into a new transcript;
        // per-file dedup would count them twice.
        let mut acc = TokenRollup::default();
        let mut seen = HashSet::new();
        let line = record("r1", "m1", 10, 20);
        fold_transcript(line.as_bytes(), "a", &mut seen, &mut acc);
        fold_transcript(line.as_bytes(), "b", &mut seen, &mut acc);
        assert_eq!(acc.records, 1);
    }

    #[test]
    fn non_usage_lines_and_junk_are_skipped_without_failing() {
        let r = fold(&[
            r#"{"type":"user","message":{"role":"user"}}"#,
            "not json at all",
            "",
            // A truncated tail line — normal on a transcript being written.
            r#"{"requestId":"r9","message":{"usage":{"input_"#,
            // Present but all-zero: not a response worth a bucket.
            r#"{"requestId":"r8","message":{"id":"m8","usage":{"input_tokens":0,"output_tokens":0}}}"#,
            &record("r1", "m1", 3, 4),
        ]);
        assert_eq!(r.records, 1);
        assert_eq!(r.total.input, 3);
    }

    #[test]
    fn missing_fields_fall_back_rather_than_dropping_the_record() {
        // No cwd → the caller's default project; no model / timestamp → no
        // bucket for those, but the totals still count.
        let line = r#"{"requestId":"r1","message":{"id":"m1","usage":{"input_tokens":9,"output_tokens":1}}}"#;
        let r = fold(&[line]);
        assert_eq!(r.records, 1);
        assert_eq!(r.total.input, 9);
        assert_eq!(r.by_project.keys().collect::<Vec<_>>(), ["fallback"]);
        assert!(r.by_model.is_empty());
        assert!(r.by_day.is_empty());
    }

    #[test]
    fn records_with_no_identity_are_each_counted() {
        // Nothing to dedupe on: counting them once each is the only honest
        // option — silently dropping them would understate the total.
        let line = r#"{"message":{"usage":{"input_tokens":5,"output_tokens":1}}}"#;
        let r = fold(&[line, line]);
        assert_eq!(r.records, 2);
        assert_eq!(r.total.input, 10);
    }

    #[test]
    fn merge_combines_buckets_and_counts() {
        let a = fold(&[&record("r1", "m1", 10, 1)]);
        let mut b = fold(&[&record("r2", "m2", 5, 2)]);
        b.merge(&a);
        assert_eq!(b.records, 2);
        assert_eq!(b.total.input, 15);
        assert_eq!(b.by_model["claude-opus-4-8"].input, 15);
    }

    #[test]
    fn top_buckets_are_biggest_first_and_capped() {
        let mut r = TokenRollup::default();
        for (model, n) in [("small", 1u64), ("big", 100), ("mid", 10)] {
            r.by_model.insert(
                model.into(),
                TranscriptTokens {
                    input: n,
                    ..Default::default()
                },
            );
        }
        let top = r.top_models(2);
        assert_eq!(
            top.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["big", "mid"]
        );
        // Days are chronological, not by size — they are a trend.
        r.by_day
            .insert("2026-08-22".into(), TranscriptTokens::default());
        r.by_day
            .insert("2026-08-01".into(), TranscriptTokens::default());
        assert_eq!(
            r.days().iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["2026-08-01", "2026-08-22"]
        );
    }

    #[test]
    fn day_and_project_extraction_handle_odd_input() {
        assert_eq!(day_of("2026-08-22T10:00:00Z"), "2026-08-22");
        assert_eq!(day_of("short"), "");
        assert_eq!(day_of(""), "");
        assert_eq!(project_of(Some("/home/u/code/thegn"), "fb"), "thegn");
        // A trailing separator must not yield an empty bucket name.
        assert_eq!(project_of(Some("/home/u/code/thegn/"), "fb"), "thegn");
        assert_eq!(project_of(Some("C:\\src\\thegn"), "fb"), "thegn");
        assert_eq!(project_of(Some(""), "fb"), "fb");
        assert_eq!(project_of(None, "fb"), "fb");
    }
}
