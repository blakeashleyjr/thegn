//! Time-window vocabulary for the history plots: how much of the past a graph
//! shows, and the configurable ladder the `[`/`]` keys step through.
//!
//! Pure and allocation-light — the arithmetic half is [`crate::series`], the
//! string half is [`crate::viz`]; this is the *span* half.
//!
//! # Why a newtype rather than an enum
//!
//! The ladder is `[monitor] window_ladder` — user data. An enum would make every
//! new rung a code change, and its persistence slug would be a variant name
//! rather than the duration itself, so relabelling `2m` → `120s` in the UI would
//! orphan what a user saved. A duration *is* its own stable slug.
//!
//! # Why the span is seconds, not samples
//!
//! The host samples on a cadence the user controls (`[stats] refresh_secs`,
//! cycled at runtime) and which the UI itself raises while a live surface is
//! open. A window counted in samples would mean something different at every
//! cadence; a window counted in seconds is resolved against real timestamps.

/// How much history a plot shows: a bounded span in seconds, or everything the
/// rings still hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Window(Option<u32>);

impl Window {
    /// Everything retained — bounded only by the recorder's own horizon.
    ///
    /// Named `EVERYTHING`, not `ALL`: the enum this replaced had *both*
    /// `Window::All` (the unbounded window) and `Window::ALL` (the array of
    /// every window), one letter-case apart. That pun is now impossible.
    pub const EVERYTHING: Window = Window(None);

    /// The window a tab starts on when config supplies nothing usable.
    ///
    /// Deliberately **not** reached by deriving `Default`: the derived default
    /// of `Option<u32>` is `None`, i.e. [`Self::EVERYTHING`] — the widest and
    /// most expensive window — which is the last thing an accidental default
    /// should be.
    pub const DEFAULT: Window = Window(Some(60));

    /// A bounded window of `secs`.
    ///
    /// Zero clamps to one second: a zero-span window has no time to bucket and
    /// would divide by nothing downstream.
    pub const fn from_secs(secs: u32) -> Window {
        Window(Some(if secs == 0 { 1 } else { secs }))
    }

    /// Span in seconds; `None` for [`Self::EVERYTHING`].
    pub const fn secs(self) -> Option<u32> {
        self.0
    }

    pub const fn is_everything(self) -> bool {
        self.0.is_none()
    }

    /// Parse `all` or a duration (`30s`, `12h`, `1h30s`, bare `42` = seconds).
    ///
    /// `None` for anything unrecognized — a preference file is a convenience,
    /// not a contract, so the caller falls back rather than refusing to start.
    /// Spans past `u32::MAX` seconds saturate rather than wrapping.
    pub fn parse(s: &str) -> Option<Window> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("all") || t.eq_ignore_ascii_case("everything") {
            return Some(Window::EVERYTHING);
        }
        let secs = parse_secs(t)?;
        Some(Window::from_secs(secs.min(u64::from(u32::MAX)) as u32))
    }

    /// Alias of [`Self::parse`], for the `from_key` call sites.
    pub fn from_key(s: &str) -> Option<Window> {
        Window::parse(s)
    }

    /// The canonical form: the largest whole unit that divides the span
    /// (`30s`, `5m`, `1h`, `12h`, `all`), else plain seconds.
    ///
    /// This is *both* the display label and the persistence slug, on purpose.
    /// The pair it replaced were two copies of one match kept in step by hand;
    /// making them one function makes `parse(label(w)) == Some(w)` a property
    /// rather than a convention.
    pub fn label(self) -> String {
        let Some(s) = self.0 else {
            return "all".into();
        };
        for (div, suffix) in [(86_400, 'd'), (3_600, 'h'), (60, 'm')] {
            if s % div == 0 {
                return format!("{}{suffix}", s / div);
            }
        }
        format!("{s}s")
    }

    /// The persistence slug. Identical to [`Self::label`] — see its docs.
    pub fn key(self) -> String {
        self.label()
    }
}

impl Default for Window {
    fn default() -> Self {
        Window::DEFAULT
    }
}

/// Widest last: [`Window::EVERYTHING`] sorts above every bounded window.
///
/// That total order is what lets [`WindowLadder`] be a sorted `Vec` and
/// `wider`/`narrower` be a `partition_point` rather than a linear scan for an
/// exact match — which is also what makes an *off-ladder* window behave sanely.
impl Ord for Window {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .unwrap_or(u32::MAX)
            .cmp(&other.0.unwrap_or(u32::MAX))
            // `EVERYTHING` and a literal u32::MAX-second window compare equal on
            // the span alone; break the tie so the order stays total and
            // `EVERYTHING` is still the widest.
            .then(self.0.is_none().cmp(&other.0.is_none()))
    }
}

impl PartialOrd for Window {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// The rungs `[`/`]` step through: ascending, deduped, never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLadder(Vec<Window>);

impl WindowLadder {
    /// The shipped default ladder.
    pub const DEFAULT_KEYS: [&'static str; 9] =
        ["30s", "1m", "5m", "10m", "30m", "1h", "6h", "12h", "all"];

    /// Build from config strings.
    ///
    /// Unparseable entries are **dropped, not fatal** — one typo in a nine-entry
    /// list must not cost the user the other eight. An empty result falls back
    /// to [`Self::DEFAULT_KEYS`] rather than leaving `[`/`]` inert, which would
    /// read as the keys being broken.
    pub fn parse<S: AsRef<str>>(keys: &[S]) -> WindowLadder {
        let mut v: Vec<Window> = keys
            .iter()
            .filter_map(|k| Window::parse(k.as_ref()))
            .collect();
        v.sort_unstable();
        v.dedup();
        if v.is_empty() {
            return WindowLadder::default_ladder();
        }
        WindowLadder(v)
    }

    pub fn default_ladder() -> WindowLadder {
        let mut v: Vec<Window> = WindowLadder::DEFAULT_KEYS
            .iter()
            .filter_map(|k| Window::parse(k))
            .collect();
        v.sort_unstable();
        v.dedup();
        WindowLadder(v)
    }

    pub fn windows(&self) -> &[Window] {
        &self.0
    }

    pub fn contains(&self, w: Window) -> bool {
        self.0.binary_search(&w).is_ok()
    }

    /// The next strictly-wider rung; **saturating** at the widest.
    ///
    /// Widening the widest is a no-op rather than a wrap back to the narrowest,
    /// which would read as the key having glitched. A window that is *not* on
    /// the ladder — a preference saved under a wider ladder, or a live config
    /// edit — snaps to the next rung above it; the enum this replaced used
    /// `position(..).unwrap_or(0)` and silently jumped such a value to the
    /// *narrowest*.
    pub fn wider(&self, w: Window) -> Window {
        let i = self.0.partition_point(|x| *x <= w);
        self.0
            .get(i)
            .copied()
            .unwrap_or_else(|| self.widest_or_default())
    }

    /// The next strictly-narrower rung; saturating, for the same reason.
    pub fn narrower(&self, w: Window) -> Window {
        let i = self.0.partition_point(|x| *x < w);
        match i.checked_sub(1) {
            Some(j) => self.0[j],
            // Already at or below the narrowest rung.
            None => self.0.first().copied().unwrap_or(Window::DEFAULT),
        }
    }

    /// The rung closest to `w` — for clamping a config default onto the ladder.
    /// Ties go to the narrower rung: over-showing history is the cheaper error.
    pub fn nearest(&self, w: Window) -> Window {
        if self.contains(w) {
            return w;
        }
        let hi = self.wider(w);
        let lo = self.narrower(w);
        match (w.secs(), lo.secs(), hi.secs()) {
            // `w` is unbounded, or one neighbour is: the unbounded end can only
            // be reached exactly, so prefer the bounded neighbour.
            (None, _, _) => self.widest_or_default(),
            (Some(_), None, _) | (Some(_), _, None) => lo,
            (Some(t), Some(l), Some(h)) => {
                if t.saturating_sub(l) <= h.saturating_sub(t) {
                    lo
                } else {
                    hi
                }
            }
        }
    }

    /// The widest **bounded** rung in seconds, ignoring `all`.
    ///
    /// This is the retention target: a recorder that keeps less than this has a
    /// rung that shows less history than it asks for.
    pub fn widest_bounded_secs(&self) -> u32 {
        self.0
            .iter()
            .rev()
            .find_map(|w| w.secs())
            .unwrap_or_else(|| Window::DEFAULT.secs().unwrap_or(60))
    }

    fn widest_or_default(&self) -> Window {
        self.0.last().copied().unwrap_or(Window::DEFAULT)
    }
}

impl Default for WindowLadder {
    fn default() -> Self {
        WindowLadder::default_ladder()
    }
}

/// Parse a duration run — `1h30s`, `3d`, `90s`, or a bare integer of seconds —
/// into whole seconds.
///
/// Units are `d`/`h`/`m`/`s`. `None` for empty input, an unknown unit, or a
/// trailing unitless number after a unit run (`1h30`), which is ambiguous.
/// Overflow saturates rather than wrapping.
pub fn parse_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // A bare integer is seconds — the common case, and the one shape that has no
    // unit to disambiguate.
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut saw_unit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let mult = match c.to_ascii_lowercase() {
            'd' => 86_400u64,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        if digits.is_empty() {
            return None;
        }
        let n: u64 = digits.parse().ok()?;
        total = total.saturating_add(n.saturating_mul(mult));
        digits.clear();
        saw_unit = true;
    }
    // `1h30` — a trailing number with no unit. Guessing would be worse than
    // refusing.
    if !digits.is_empty() || !saw_unit {
        return None;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(secs: u32) -> Window {
        Window::from_secs(secs)
    }

    #[test]
    fn parse_and_label_round_trip() {
        for s in [
            "30s", "1m", "5m", "10m", "30m", "1h", "6h", "12h", "1d", "all",
        ] {
            let parsed = Window::parse(s).unwrap_or_else(|| panic!("parse {s}"));
            assert_eq!(parsed.label(), s, "label round trip for {s}");
            assert_eq!(Window::parse(&parsed.label()), Some(parsed));
        }
    }

    #[test]
    fn canonical_label_picks_the_largest_whole_unit() {
        assert_eq!(w(30).label(), "30s");
        assert_eq!(w(60).label(), "1m");
        assert_eq!(w(600).label(), "10m");
        assert_eq!(w(3_600).label(), "1h");
        assert_eq!(w(43_200).label(), "12h");
        assert_eq!(w(86_400).label(), "1d");
        // Not a whole number of minutes, so it stays in seconds rather than
        // rounding to a label that would parse back to a different window.
        assert_eq!(w(5_430).label(), "5430s");
        assert_eq!(Window::parse("5430s"), Some(w(5_430)));
        assert_eq!(Window::EVERYTHING.label(), "all");
    }

    #[test]
    fn parse_accepts_compound_bare_and_case_insensitive_all() {
        assert_eq!(Window::parse("1h30s"), Some(w(3_630)));
        assert_eq!(Window::parse("2h15m"), Some(w(8_100)));
        assert_eq!(Window::parse("42"), Some(w(42)));
        assert_eq!(Window::parse("  10m  "), Some(w(600)));
        assert_eq!(Window::parse("ALL"), Some(Window::EVERYTHING));
        assert_eq!(Window::parse("All"), Some(Window::EVERYTHING));
    }

    #[test]
    fn parse_rejects_junk_without_panicking() {
        for bad in ["", "   ", "abc", "10x", "h", "1h30", "17 fortnights", "-5m"] {
            assert_eq!(Window::parse(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn zero_clamps_so_a_window_always_has_span() {
        // A zero-span window has no time to bucket; downstream would divide by
        // nothing.
        assert_eq!(w(0).secs(), Some(1));
        assert_eq!(Window::parse("0"), Some(w(1)));
        assert_eq!(Window::parse("0s"), Some(w(1)));
    }

    #[test]
    fn parse_secs_saturates_rather_than_wrapping() {
        // A hostile config value must not wrap into a small window.
        let huge = format!("{}d", u64::MAX);
        assert_eq!(parse_secs(&huge), Some(u64::MAX));
        assert_eq!(Window::parse(&huge), Some(w(u32::MAX)));
    }

    #[test]
    fn parse_secs_matches_the_replay_expressions() {
        // The host's `replay::parse_duration` is now a wrapper over this; these
        // are its table-test cases verbatim, so the move cannot regress it.
        assert_eq!(parse_secs("90s"), Some(90));
        assert_eq!(parse_secs("5m"), Some(300));
        assert_eq!(parse_secs("1h30s"), Some(3_630));
        assert_eq!(parse_secs("3d"), Some(259_200));
        assert_eq!(parse_secs("2h15m"), Some(8_100));
        assert_eq!(parse_secs("42"), Some(42));
        assert_eq!(parse_secs(""), None);
        assert_eq!(parse_secs("abc"), None);
        assert_eq!(parse_secs("10x"), None);
        assert_eq!(parse_secs("h"), None);
        assert_eq!(parse_secs("1h30"), None);
    }

    #[test]
    fn ordering_puts_everything_widest() {
        let mut v = vec![Window::EVERYTHING, w(3_600), w(30), w(43_200), w(60)];
        v.sort();
        assert_eq!(
            v,
            vec![w(30), w(60), w(3_600), w(43_200), Window::EVERYTHING]
        );
        assert!(Window::EVERYTHING > w(u32::MAX));
    }

    #[test]
    fn the_default_window_is_not_everything() {
        // Guards the `Default`-derive footgun: `Option<u32>::default()` is
        // `None`, which here means the WIDEST window.
        assert_eq!(Window::default(), Window::DEFAULT);
        assert!(!Window::default().is_everything());
        assert_eq!(Window::default().secs(), Some(60));
    }

    #[test]
    fn ladder_parse_sorts_dedups_and_drops_junk() {
        let l = WindowLadder::parse(&["1h", "30s", "not-a-window", "30s", "5m"]);
        assert_eq!(l.windows(), &[w(30), w(300), w(3_600)]);
        // Equivalent spellings collapse to one rung.
        let l = WindowLadder::parse(&["60s", "1m", "5m"]);
        assert_eq!(l.windows(), &[w(60), w(300)]);
    }

    #[test]
    fn ladder_falls_back_when_config_is_empty_or_all_junk() {
        // Leaving `[`/`]` inert would read as the keys being broken.
        let empty: [&str; 0] = [];
        assert_eq!(WindowLadder::parse(&empty), WindowLadder::default_ladder());
        assert_eq!(
            WindowLadder::parse(&["nonsense", "17 fortnights"]),
            WindowLadder::default_ladder()
        );
    }

    #[test]
    fn the_default_ladder_reaches_twelve_hours() {
        let l = WindowLadder::default_ladder();
        assert!(
            l.contains(w(43_200)),
            "12h must be a rung: {:?}",
            l.windows()
        );
        assert!(l.contains(Window::EVERYTHING));
        assert_eq!(l.widest_bounded_secs(), 43_200);
        // Every documented rung parses — a typo in DEFAULT_KEYS would silently
        // shorten the shipped ladder.
        assert_eq!(l.windows().len(), WindowLadder::DEFAULT_KEYS.len());
    }

    #[test]
    fn wider_and_narrower_saturate_at_both_ends() {
        let l = WindowLadder::parse(&["30s", "5m", "1h"]);
        assert_eq!(l.wider(w(30)), w(300));
        assert_eq!(l.wider(w(300)), w(3_600));
        // Saturating, NOT wrapping: widening the widest is a no-op.
        assert_eq!(l.wider(w(3_600)), w(3_600));
        assert_eq!(l.narrower(w(3_600)), w(300));
        assert_eq!(l.narrower(w(300)), w(30));
        assert_eq!(l.narrower(w(30)), w(30));
    }

    #[test]
    fn an_off_ladder_window_snaps_to_a_neighbour_not_the_narrowest() {
        // A preference saved under a wider ladder, or a live config edit. The
        // enum this replaced jumped such a value to the narrowest rung.
        let l = WindowLadder::parse(&["30s", "5m", "1h"]);
        assert_eq!(l.wider(w(120)), w(300));
        assert_eq!(l.narrower(w(120)), w(30));
        // Off the top: widening saturates at the widest rather than wrapping.
        assert_eq!(l.wider(w(7_200)), w(3_600));
        assert_eq!(l.narrower(w(7_200)), w(3_600));
    }

    #[test]
    fn everything_walks_the_ladder_like_any_other_rung() {
        let l = WindowLadder::default_ladder();
        assert_eq!(l.wider(Window::EVERYTHING), Window::EVERYTHING);
        assert_eq!(l.narrower(Window::EVERYTHING), w(43_200));
        assert_eq!(l.wider(w(43_200)), Window::EVERYTHING);
    }

    #[test]
    fn nearest_clamps_onto_the_ladder() {
        let l = WindowLadder::parse(&["30s", "5m", "1h"]);
        assert_eq!(l.nearest(w(300)), w(300));
        assert_eq!(l.nearest(w(60)), w(30)); // 30s away vs 240s away
        assert_eq!(l.nearest(w(3_000)), w(3_600)); // 2700 away vs 600 away
        // A tie goes to the narrower rung: over-showing history is cheaper than
        // asking for history that isn't there.
        assert_eq!(l.nearest(w(1_815)), w(300));
        // Unbounded snaps to the widest rung on a ladder that lacks `all`.
        assert_eq!(l.nearest(Window::EVERYTHING), w(3_600));
    }

    #[test]
    fn widest_bounded_ignores_everything() {
        let l = WindowLadder::parse(&["30s", "1h", "all"]);
        assert_eq!(l.widest_bounded_secs(), 3_600);
        // A ladder of nothing but `all` still yields a usable retention target.
        let l = WindowLadder::parse(&["all"]);
        assert_eq!(l.widest_bounded_secs(), 60);
    }

    #[test]
    fn display_matches_the_label() {
        assert_eq!(w(43_200).to_string(), "12h");
        assert_eq!(Window::EVERYTHING.to_string(), "all");
    }
}
