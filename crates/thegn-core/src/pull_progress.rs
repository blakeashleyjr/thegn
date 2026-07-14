//! Tolerant OCI image-pull progress parser.
//!
//! `podman pull` and `docker pull` stream very different per-layer progress
//! text; this module folds either stream into a best-effort aggregate
//! [`PullSnapshot`] the loading screen can draw as a byte bar. The parser is
//! deliberately forgiving: any line it does not understand contributes
//! nothing (a garbage transcript yields no snapshots at all, and the splash
//! just shows the plain "pulling" step), and emission is delta-throttled so a
//! fast pull cannot storm the compositor loop with frames.
//!
//! Representative input, podman (plain progress on stderr):
//! ```text
//! Trying to pull docker.io/library/debian:stable...
//! Copying blob 3f4ca61aafcd [=>------------------] 12.0MiB / 45.2MiB
//! Copying blob 8a1e25ce7c4f done
//! ```
//! docker (non-TTY stdout):
//! ```text
//! stable: Pulling from library/debian
//! 3f4ca61aafcd: Downloading [=>       ]  12.3MB/45.2MB
//! 3f4ca61aafcd: Pull complete
//! ```

use std::collections::HashMap;

/// Aggregate pull progress across all layers seen so far. Byte totals are
/// best-effort: layers the runtime has not yet sized are simply absent from
/// `bytes_total`, so the fraction can only move forward as sizes appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PullSnapshot {
    /// Bytes copied so far, summed across layers reporting bytes.
    pub bytes_done: u64,
    /// Sum of known layer totals; `None` until any layer reports a total.
    pub bytes_total: Option<u64>,
    /// Layers the stream has marked complete.
    pub layers_done: usize,
    /// Distinct layers seen in the stream so far.
    pub layers_total: usize,
}

impl PullSnapshot {
    /// Best-effort completion fraction in `0.0..=1.0`, when byte totals are
    /// known.
    pub fn fraction(&self) -> Option<f64> {
        let total = self.bytes_total.filter(|t| *t > 0)?;
        Some((self.bytes_done as f64 / total as f64).clamp(0.0, 1.0))
    }

    /// Human progress text, degrading with what the stream gave us:
    /// bytes (`"142 MB / 380 MB"`) → layer count (`"layer 3/7"`) → `None`
    /// (caller shows its plain "pulling" label).
    pub fn detail(&self) -> Option<String> {
        if let Some(total) = self.bytes_total.filter(|t| *t > 0) {
            return Some(format!(
                "{} / {}",
                fmt_bytes(self.bytes_done),
                fmt_bytes(total)
            ));
        }
        if self.layers_total > 0 {
            return Some(format!("layer {}/{}", self.layers_done, self.layers_total));
        }
        None
    }
}

/// Format a byte count for progress text: `999 B`, `12.3 kB`, `142 MB`,
/// `1.2 GB`. Decimal units — matches what docker prints, close enough to
/// podman's binary units for a progress line.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut unit = "B";
    for u in UNITS {
        if v < 1000.0 {
            break;
        }
        v /= 1000.0;
        unit = u;
    }
    if v < 10.0 {
        format!("{v:.1} {unit}")
    } else {
        format!("{} {unit}", v.round() as u64)
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Layer {
    done: u64,
    total: Option<u64>,
    complete: bool,
}

/// Streaming line parser for `podman pull` / `docker pull` output. Feed it
/// every line from either stdout or stderr; it returns a fresh snapshot only
/// when the aggregate moved enough to be worth a frame (see [`Self::feed_line`]).
#[derive(Debug, Default)]
pub struct PullParser {
    layers: HashMap<String, Layer>,
    last: Option<PullSnapshot>,
}

impl PullParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one output line in. Returns `Some(snapshot)` when the line changed
    /// the aggregate meaningfully: the first parsed progress, a layer
    /// appearing or completing, or the byte fraction moving ≥1% (≥8 MB when
    /// no total is known yet). The delta throttle bounds a whole pull to
    /// ~100 + 2·layers emissions regardless of how chatty the runtime is, so
    /// the caller can forward every `Some` to the UI unconditionally.
    pub fn feed_line(&mut self, line: &str) -> Option<PullSnapshot> {
        let line = line.trim_end();
        let (id, rest) = split_layer_line(line)?;
        let layer = self.layers.entry(id.to_string()).or_default();
        if is_complete_marker(rest) {
            layer.complete = true;
            if let Some(t) = layer.total {
                layer.done = t;
            }
        } else if let Some((done, total)) = parse_byte_pair(rest) {
            // Progress can only move forward; interleaved Download/Extract
            // phases (docker) both match, and extraction restarting from 0
            // must not walk the aggregate backwards.
            layer.done = layer.done.max(done);
            layer.total = Some(layer.total.map_or(total, |t| t.max(total)));
        }
        // (An unrecognized rest still *registered* the layer — "Pulling fs
        // layer" / "Waiting" lines legitimately announce layers early.)
        self.maybe_snapshot()
    }

    fn aggregate(&self) -> PullSnapshot {
        let mut snap = PullSnapshot {
            layers_total: self.layers.len(),
            ..Default::default()
        };
        let mut total = 0u64;
        let mut any_total = false;
        for l in self.layers.values() {
            snap.bytes_done += l.done;
            if let Some(t) = l.total {
                total += t;
                any_total = true;
            }
            if l.complete {
                snap.layers_done += 1;
            }
        }
        snap.bytes_total = any_total.then_some(total);
        snap
    }

    fn maybe_snapshot(&mut self) -> Option<PullSnapshot> {
        let snap = self.aggregate();
        if snap == PullSnapshot::default() {
            return None; // nothing parsed yet
        }
        let emit = match self.last {
            None => true,
            Some(prev) => {
                snap.layers_done != prev.layers_done
                    || snap.layers_total != prev.layers_total
                    || moved_a_percent(prev, snap)
            }
        };
        if emit {
            self.last = Some(snap);
            Some(snap)
        } else {
            None
        }
    }
}

/// Whether the byte progress moved enough for a new frame: ≥1% of the known
/// total, or ≥8 MB when no total is known.
fn moved_a_percent(prev: PullSnapshot, cur: PullSnapshot) -> bool {
    let delta = cur.bytes_done.saturating_sub(prev.bytes_done);
    match cur.bytes_total {
        Some(t) if t > 0 => delta.saturating_mul(100) >= t,
        _ => delta >= 8_000_000,
    }
}

/// Split a per-layer progress line into `(layer_id, rest)`.
/// Podman: `Copying blob <id> <rest>`; docker: `<hex-id>: <rest>`.
fn split_layer_line(line: &str) -> Option<(&str, &str)> {
    if let Some(rest) = line.strip_prefix("Copying blob ") {
        let (id, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        return Some((id, rest));
    }
    let (head, rest) = line.split_once(": ")?;
    let is_layer_id = head.len() >= 6
        && head
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase());
    is_layer_id.then_some((head, rest))
}

/// Whether `rest` marks its layer finished. Podman says `done`; docker says
/// `Pull complete` (with `Already exists` for cached layers).
fn is_complete_marker(rest: &str) -> bool {
    let rest = rest.trim();
    rest == "done" || rest == "Pull complete" || rest == "Already exists"
}

/// Scan `rest` for a `<size> / <size>` or `<size>/<size>` byte pair, ignoring
/// any `[==>---]` bar. Returns `(done, total)` in bytes.
fn parse_byte_pair(rest: &str) -> Option<(u64, u64)> {
    // Strip the ASCII progress bar so its `=`/`-` runs can't confuse parsing.
    let cleaned = match (rest.find('['), rest.rfind(']')) {
        (Some(a), Some(b)) if a < b => format!("{}{}", &rest[..a], &rest[b + 1..]),
        _ => rest.to_string(),
    };
    let (left, right) = cleaned.split_once('/')?;
    let done = parse_size(left.split_whitespace().last()?)?;
    let total = parse_size(right.split_whitespace().next()?)?;
    Some((done, total))
}

/// Parse `12.0MiB` / `45.2MB` / `999B` / `1.2GB` into bytes. Binary units
/// (KiB/MiB/GiB/TiB — podman) use 1024; decimal (kB/KB/MB/GB/TB — docker)
/// use 1000.
fn parse_size(tok: &str) -> Option<u64> {
    let tok = tok.trim();
    let split = tok.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = tok.split_at(split);
    let num: f64 = num.parse().ok()?;
    if !num.is_finite() || num < 0.0 {
        return None;
    }
    let mult: u64 = match unit {
        "B" | "b" => 1,
        "kB" | "KB" => 1000,
        "MB" => 1000 * 1000,
        "GB" => 1000 * 1000 * 1000,
        "TB" => 1000 * 1000 * 1000 * 1000,
        "KiB" => 1024,
        "MiB" => 1024 * 1024,
        "GiB" => 1024 * 1024 * 1024,
        "TiB" => 1024u64.pow(4),
        _ => return None,
    };
    Some((num * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_podman_transcript() {
        let mut p = PullParser::new();
        assert!(
            p.feed_line("Trying to pull docker.io/library/debian:stable...")
                .is_none()
        );
        assert!(p.feed_line("Getting image source signatures").is_none());
        let s = p
            .feed_line("Copying blob 3f4ca61aafcd [=>------------------] 12.0MiB / 45.2MiB")
            .expect("first progress emits");
        assert_eq!(s.bytes_done, (12.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(s.bytes_total, Some((45.2 * 1024.0 * 1024.0) as u64));
        assert_eq!((s.layers_done, s.layers_total), (0, 1));
        let s = p
            .feed_line("Copying blob 8a1e25ce7c4f done")
            .expect("new layer + completion emits");
        assert_eq!((s.layers_done, s.layers_total), (1, 2));
    }

    #[test]
    fn parses_docker_transcript() {
        let mut p = PullParser::new();
        assert!(p.feed_line("stable: Pulling from library/debian").is_none());
        // Announces the layer with no bytes: layers_total moves ⇒ emit.
        let s = p.feed_line("3f4ca61aafcd: Pulling fs layer").unwrap();
        assert_eq!((s.layers_done, s.layers_total), (0, 1));
        let s = p
            .feed_line("3f4ca61aafcd: Downloading [=>       ]  12.3MB/45.2MB")
            .unwrap();
        assert_eq!(s.bytes_done, 12_300_000);
        assert_eq!(s.bytes_total, Some(45_200_000));
        let s = p.feed_line("3f4ca61aafcd: Pull complete").unwrap();
        assert_eq!(s.layers_done, 1);
        assert_eq!(s.bytes_done, 45_200_000, "completion snaps done to total");
        // Cached layer.
        let s = p.feed_line("9a1e25ce7c4f: Already exists").unwrap();
        assert_eq!((s.layers_done, s.layers_total), (2, 2));
    }

    #[test]
    fn garbage_yields_nothing_forever() {
        let mut p = PullParser::new();
        for line in [
            "",
            "error: something exploded",
            "WARN[0000] missing config",
            "  42  ",
            "Pulling from library/debian",    // no layer id
            "NOTALAYER: Downloading 1MB/2MB", // uppercase ⇒ not a hex id
        ] {
            assert!(p.feed_line(line).is_none(), "line {line:?} must not emit");
        }
    }

    #[test]
    fn throttles_sub_percent_progress() {
        let mut p = PullParser::new();
        assert!(
            p.feed_line("Copying blob aaaa11 [>] 0B / 1000MB").is_some(),
            "first progress emits"
        );
        // +0.5% — swallowed.
        assert!(
            p.feed_line("Copying blob aaaa11 [>] 5MB / 1000MB")
                .is_none()
        );
        // Cumulative +1.0% since the last emit — emits.
        let s = p
            .feed_line("Copying blob aaaa11 [>] 10MB / 1000MB")
            .unwrap();
        assert_eq!(s.bytes_done, 10_000_000);
        // Layer completion always emits, regardless of byte delta.
        assert!(p.feed_line("Copying blob aaaa11 done").is_some());
    }

    #[test]
    fn unknown_total_throttles_on_8mb_and_never_regresses() {
        let mut p = PullParser::new();
        assert!(p.feed_line("bbbb22: Waiting").is_some(), "layer appears");
        // Extracting lines with bytes but docker sometimes omits totals; feed a
        // pair where a later line reports FEWER bytes (extract phase restarts).
        assert!(p.feed_line("bbbb22: Downloading [>]  20MB/50MB").is_some());
        let before = p.aggregate();
        assert!(p.feed_line("bbbb22: Extracting [>]  1MB/50MB").is_none());
        assert_eq!(p.aggregate(), before, "progress never walks backwards");
    }

    #[test]
    fn snapshot_detail_degrades_gracefully() {
        let bytes = PullSnapshot {
            bytes_done: 142_000_000,
            bytes_total: Some(380_000_000),
            layers_done: 3,
            layers_total: 7,
        };
        assert_eq!(bytes.detail().unwrap(), "142 MB / 380 MB");
        assert!((bytes.fraction().unwrap() - 0.3737).abs() < 0.01);
        let layers_only = PullSnapshot {
            layers_done: 3,
            layers_total: 7,
            ..Default::default()
        };
        assert_eq!(layers_only.detail().unwrap(), "layer 3/7");
        assert_eq!(layers_only.fraction(), None);
        assert_eq!(PullSnapshot::default().detail(), None);
    }

    #[test]
    fn fmt_bytes_scales() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(12_300), "12 kB");
        assert_eq!(fmt_bytes(1_200), "1.2 kB");
        assert_eq!(fmt_bytes(142_000_000), "142 MB");
        assert_eq!(fmt_bytes(1_200_000_000), "1.2 GB");
    }

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("999B"), Some(999));
        assert_eq!(parse_size("1kB"), Some(1000));
        assert_eq!(parse_size("1KiB"), Some(1024));
        assert_eq!(parse_size("12.0MiB"), Some((12.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("45.2MB"), Some(45_200_000));
        assert_eq!(parse_size("2GiB"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1TB"), Some(1_000_000_000_000));
        assert_eq!(parse_size("1TiB"), Some(1024u64.pow(4)));
        assert_eq!(parse_size("12"), None, "bare number has no unit");
        assert_eq!(parse_size("xMB"), None);
        assert_eq!(parse_size("-3MB"), None, "negative sizes rejected");
        assert_eq!(parse_size("1XB"), None, "unknown unit rejected");
    }

    #[test]
    fn layer_id_detection() {
        assert_eq!(
            split_layer_line("Copying blob sha256:abc123 done"),
            Some(("sha256:abc123", "done"))
        );
        assert_eq!(
            split_layer_line("3f4ca61aafcd: Pull complete"),
            Some(("3f4ca61aafcd", "Pull complete"))
        );
        // Prose with a colon is not a layer.
        assert_eq!(split_layer_line("Status: Downloaded newer image"), None);
        assert_eq!(split_layer_line("short: x"), None, "id must be ≥6 chars");
    }
}
