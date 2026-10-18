//! Pure memory-pressure verdict policy for the system monitor.
//!
//! Inputs are optional so a missing reader, an unsupported platform, and a
//! non-finite rate remain unknown rather than becoming a healthy zero. The
//! policy has no hysteresis because the memory specification gives none; its
//! 60-second OOM window is the only time-based persistence. macOS levels map
//! 1 to `Ok`, 2 (warn) to `Pressure`, and 4 (critical) to `Thrashing`. Windows
//! low maps to `Pressure`; high and neither map to `Ok`. These platform
//! mappings are policy choices for owner review, not platform readers.

/// Recent OOM kill window, inclusive, in seconds.
pub const OOM_WINDOW_SECS: f64 = 60.0;
/// Full PSI average over ten seconds that indicates thrashing, in percent.
pub const THRASH_PSI_FULL_AVG10: f64 = 10.0;
/// Refault rate threshold for the combined refault/major-fault rule.
pub const THRASH_REFAULT_PPS: f64 = 5000.0;
/// Major-fault rate threshold for the combined refault/major-fault rule.
pub const THRASH_MAJFLT_PS: f64 = 200.0;
/// Some PSI average over ten seconds that indicates pressure, in percent.
pub const PRESSURE_PSI_SOME_AVG10: f64 = 5.0;
/// Direct reclaim rate that indicates pressure, in pages per second.
pub const PRESSURE_DIRECT_RECLAIM_PPS: f64 = 1000.0;
/// Allocation stall rate that indicates pressure, per second.
pub const PRESSURE_ALLOCSTALL_PS: f64 = 10.0;
/// Available memory below this percentage indicates pressure.
pub const PRESSURE_AVAILABLE_PCT: f64 = 10.0;
/// Available memory below this percentage indicates tight memory.
pub const TIGHT_AVAILABLE_PCT: f64 = 20.0;
/// Swap used above this percentage, with swap-in activity, indicates tight memory.
pub const TIGHT_SWAP_USED_PCT: f64 = 50.0;
/// Slice use at or above this fraction of MemoryHigh indicates tight memory.
pub const TIGHT_SLICE_HIGH_FRAC: f64 = 0.85;

/// Inputs to [`evaluate`]. Each reader may be unavailable independently.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct MemHealthInput {
    /// Total physical memory in bytes.
    pub mem_total_bytes: Option<u64>,
    /// Available physical memory in bytes.
    pub mem_available_bytes: Option<u64>,
    /// Total swap in bytes.
    pub swap_total_bytes: Option<u64>,
    /// Free swap in bytes.
    pub swap_free_bytes: Option<u64>,
    /// Swap-in rate in pages per second; only whether it is greater than zero matters.
    pub swap_in_per_sec: Option<f64>,
    /// Some-pressure PSI average over ten seconds, in percent.
    pub psi_some_avg10: Option<f64>,
    /// Full-pressure PSI average over ten seconds, in percent.
    pub psi_full_avg10: Option<f64>,
    /// Anonymous plus file refault pages per second.
    pub refault_pages_per_sec: Option<f64>,
    /// Major faults per second.
    pub majflt_per_sec: Option<f64>,
    /// Direct reclaim pages per second.
    pub direct_reclaim_pages_per_sec: Option<f64>,
    /// Allocation stalls per second.
    pub allocstall_per_sec: Option<f64>,
    /// Seconds since the last OOM kill.
    pub secs_since_oom_kill: Option<f64>,
    /// `memory.current / MemoryHigh`, which may exceed one.
    pub slice_used_frac_of_high: Option<f64>,
    /// Coarse pressure state supplied by an OS-specific reader.
    pub os_level: Option<OsPressure>,
}

/// Coarse pressure state reported by an operating system API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsPressure {
    /// macOS memorystatus level 1.
    MacNormal,
    /// macOS memorystatus level 2.
    MacWarn,
    /// macOS memorystatus level 4.
    MacCritical,
    /// Windows memory resource notification: high.
    WinHigh,
    /// Windows memory resource notification: neither high nor low.
    WinNeither,
    /// Windows memory resource notification: low.
    WinLow,
}

impl OsPressure {
    /// Convert a known macOS memorystatus level; unknown values stay absent.
    pub const fn from_mac_level(level: u32) -> Option<Self> {
        match level {
            1 => Some(Self::MacNormal),
            2 => Some(Self::MacWarn),
            4 => Some(Self::MacCritical),
            _ => None,
        }
    }
}

/// Ordered severity returned by [`evaluate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No rule fired and available memory could not be calculated.
    Unknown,
    /// No pressure rule fired.
    Ok,
    /// A tight-memory rule fired.
    Tight,
    /// A pressure rule fired.
    Pressure,
    /// A thrashing rule fired.
    Thrashing,
    /// An OOM kill occurred within [`OOM_WINDOW_SECS`].
    Oom,
}

impl Verdict {
    /// Numeric ordering for known verdicts; unknown has no severity.
    pub const fn severity(self) -> Option<u8> {
        match self {
            Self::Unknown => None,
            Self::Ok => Some(0),
            Self::Tight => Some(1),
            Self::Pressure => Some(2),
            Self::Thrashing => Some(3),
            Self::Oom => Some(4),
        }
    }

    /// Stable ASCII label for display and logging.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ok => "ok",
            Self::Tight => "tight",
            Self::Pressure => "pressure",
            Self::Thrashing => "thrashing",
            Self::Oom => "oom",
        }
    }
}

/// A named rule that contributed to a memory-health verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// OOM kill within the recent-kill window.
    OomRecent,
    /// Full PSI threshold reached.
    PsiFull,
    /// Refault and major-fault thresholds both reached.
    RefaultMajflt,
    /// Some PSI threshold reached.
    PsiSome,
    /// Direct reclaim threshold reached.
    DirectReclaim,
    /// Allocation stall threshold reached.
    AllocStall,
    /// Available memory below the pressure threshold.
    AvailableLow,
    /// Available memory below the tight threshold.
    AvailableTight,
    /// Swap usage and swap-in activity indicate tight memory.
    SwapActive,
    /// Slice usage reached its MemoryHigh fraction threshold.
    SliceHigh,
    /// An OS pressure level contributed a verdict.
    OsLevel,
}

impl Reason {
    const ALL: [Self; 11] = [
        Self::OomRecent,
        Self::PsiFull,
        Self::RefaultMajflt,
        Self::PsiSome,
        Self::DirectReclaim,
        Self::AllocStall,
        Self::AvailableLow,
        Self::AvailableTight,
        Self::SwapActive,
        Self::SliceHigh,
        Self::OsLevel,
    ];

    const fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// Allocation-free set of reasons, iterated in [`Reason`] declaration order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reasons(u16);

impl Reasons {
    /// Whether this set contains `reason`.
    pub const fn contains(self, reason: Reason) -> bool {
        self.0 & reason.bit() != 0
    }

    /// Whether no rule contributed a reason.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Iterate over included reasons in declaration order without allocating.
    pub fn iter(self) -> impl Iterator<Item = Reason> {
        Reason::ALL
            .into_iter()
            .filter(move |reason| self.contains(*reason))
    }

    fn insert(&mut self, reason: Reason) {
        self.0 |= reason.bit();
    }
}

/// The verdict, all fired reasons, and whether PSI and OS pressure were absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemHealth {
    /// Worst severity among all fired rules, or unknown when evidence is absent.
    pub verdict: Verdict,
    /// Every rule that fired, including rules below the winning severity.
    pub reasons: Reasons,
    /// True when both PSI readings and the OS-level reading are absent.
    pub no_psi: bool,
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

fn available_pct(input: &MemHealthInput) -> Option<f64> {
    let total = input.mem_total_bytes?;
    if total == 0 {
        return None;
    }
    let available = input.mem_available_bytes?.min(total);
    Some((available as f64 / total as f64) * 100.0)
}

struct Acc {
    verdict: Verdict,
    reasons: Reasons,
}

fn fire(acc: &mut Acc, reason: Reason, next: Verdict) {
    acc.reasons.insert(reason);
    if next.severity() > acc.verdict.severity() {
        acc.verdict = next;
    }
}

/// Evaluate every known memory-health rule and keep all fired reasons.
///
/// Missing or non-finite operands skip their rule. Available bytes are clamped
/// to total memory; zero total memory and zero total swap are unknown/no-swap.
/// An OS reading maps to a rule even when it contributes `Ok`, preserving its
/// provenance in `reasons`.
pub fn evaluate(input: &MemHealthInput) -> MemHealth {
    let mut acc = Acc {
        verdict: Verdict::Unknown,
        reasons: Reasons::default(),
    };

    if let Some(age) = finite(input.secs_since_oom_kill)
        && age <= OOM_WINDOW_SECS
    {
        fire(&mut acc, Reason::OomRecent, Verdict::Oom);
    }

    if let Some(full) = finite(input.psi_full_avg10)
        && full >= THRASH_PSI_FULL_AVG10
    {
        fire(&mut acc, Reason::PsiFull, Verdict::Thrashing);
    }

    if let Some(refault) = finite(input.refault_pages_per_sec)
        && let Some(majflt) = finite(input.majflt_per_sec)
        && refault >= THRASH_REFAULT_PPS
        && majflt >= THRASH_MAJFLT_PS
    {
        fire(&mut acc, Reason::RefaultMajflt, Verdict::Thrashing);
    }

    if let Some(some) = finite(input.psi_some_avg10)
        && some >= PRESSURE_PSI_SOME_AVG10
    {
        fire(&mut acc, Reason::PsiSome, Verdict::Pressure);
    }

    if let Some(reclaim) = finite(input.direct_reclaim_pages_per_sec)
        && reclaim >= PRESSURE_DIRECT_RECLAIM_PPS
    {
        fire(&mut acc, Reason::DirectReclaim, Verdict::Pressure);
    }

    if let Some(stalls) = finite(input.allocstall_per_sec)
        && stalls >= PRESSURE_ALLOCSTALL_PS
    {
        fire(&mut acc, Reason::AllocStall, Verdict::Pressure);
    }

    let available = available_pct(input);
    if let Some(pct) = available
        && pct < PRESSURE_AVAILABLE_PCT
    {
        fire(&mut acc, Reason::AvailableLow, Verdict::Pressure);
    }
    if let Some(pct) = available
        && pct < TIGHT_AVAILABLE_PCT
    {
        fire(&mut acc, Reason::AvailableTight, Verdict::Tight);
    }

    if let Some(total) = input.swap_total_bytes
        && total > 0
        && let Some(free) = input.swap_free_bytes
        && let Some(swap_in) = finite(input.swap_in_per_sec)
        && swap_in > 0.0
    {
        let used_pct = (total.saturating_sub(free) as f64 / total as f64) * 100.0;
        if used_pct > TIGHT_SWAP_USED_PCT {
            fire(&mut acc, Reason::SwapActive, Verdict::Tight);
        }
    }

    if let Some(slice) = finite(input.slice_used_frac_of_high)
        && slice >= TIGHT_SLICE_HIGH_FRAC
    {
        fire(&mut acc, Reason::SliceHigh, Verdict::Tight);
    }

    if let Some(os_level) = input.os_level {
        let os_verdict = match os_level {
            OsPressure::MacNormal | OsPressure::WinHigh | OsPressure::WinNeither => Verdict::Ok,
            OsPressure::MacWarn | OsPressure::WinLow => Verdict::Pressure,
            OsPressure::MacCritical => Verdict::Thrashing,
        };
        fire(&mut acc, Reason::OsLevel, os_verdict);
    }

    if acc.reasons.is_empty() && available.is_some() {
        acc.verdict = Verdict::Ok;
    }
    MemHealth {
        verdict: acc.verdict,
        reasons: acc.reasons,
        no_psi: finite(input.psi_some_avg10).is_none()
            && finite(input.psi_full_avg10).is_none()
            && input.os_level.is_none(),
    }
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use super::*;

    const EPS: f64 = 1e-6;

    macro_rules! input_field {
        ($field:ident, $value:expr) => {
            MemHealthInput {
                $field: Some($value),
                ..MemHealthInput::default()
            }
        };
    }
    macro_rules! threshold {
        ($field:ident, $limit:expr, $expected:expr) => {
            check_threshold(|x| input_field!($field, x), $limit, $expected)
        };
    }
    macro_rules! expect {
        ($input:expr, $expected:expr) => {
            check_verdict($input, $expected)
        };
    }
    fn verdict(input: MemHealthInput) -> Verdict {
        evaluate(&input).verdict
    }
    fn has_reason(input: MemHealthInput, reason: Reason) -> bool {
        evaluate(&input).reasons.contains(reason)
    }
    fn available(bytes: u64) -> MemHealthInput {
        memory(100, bytes, MemHealthInput::default())
    }
    fn memory(total: u64, available: u64, input: MemHealthInput) -> MemHealthInput {
        MemHealthInput {
            mem_total_bytes: Some(total),
            mem_available_bytes: Some(available),
            ..input
        }
    }
    fn known(input: MemHealthInput) -> MemHealthInput {
        MemHealthInput {
            mem_total_bytes: Some(100),
            mem_available_bytes: Some(100),
            ..input
        }
    }
    fn paired(refault: Option<f64>, majflt: Option<f64>, swap_in: Option<f64>) -> MemHealthInput {
        MemHealthInput {
            refault_pages_per_sec: refault,
            majflt_per_sec: majflt,
            swap_total_bytes: Some(100),
            swap_free_bytes: Some(0),
            swap_in_per_sec: swap_in,
            ..MemHealthInput::default()
        }
    }
    fn swap(total: u64, free: u64, rate: Option<f64>) -> MemHealthInput {
        MemHealthInput {
            swap_total_bytes: Some(total),
            swap_free_bytes: Some(free),
            swap_in_per_sec: rate,
            ..MemHealthInput::default()
        }
    }
    fn os(level: OsPressure) -> MemHealthInput { MemHealthInput { os_level: Some(level), ..MemHealthInput::default() } }
    fn check_threshold(make: fn(f64) -> MemHealthInput, threshold: f64, result: Verdict) {
        assert_eq!(verdict(make(threshold - EPS)), Verdict::Unknown);
        assert_eq!(verdict(make(threshold + EPS)), result);
    }
    fn check_verdict(input: MemHealthInput, expected: Verdict) {
        assert_eq!(verdict(input), expected);
    }
    fn check_equal(left: MemHealthInput, right: MemHealthInput) {
        assert_eq!(evaluate(&left), evaluate(&right));
    }
    macro_rules! same {
        ($left:expr, $right:expr) => {
            check_equal($left, $right)
        };
    }

    #[test]
    fn all_threshold_boundaries_and_oom_window() {
        expect!(input_field!(secs_since_oom_kill, OOM_WINDOW_SECS), Verdict::Oom);
        expect!(input_field!(secs_since_oom_kill, OOM_WINDOW_SECS - EPS), Verdict::Oom);
        expect!(input_field!(secs_since_oom_kill, OOM_WINDOW_SECS + EPS), Verdict::Unknown);
        threshold!(psi_full_avg10, THRASH_PSI_FULL_AVG10, Verdict::Thrashing);
        threshold!(psi_some_avg10, PRESSURE_PSI_SOME_AVG10, Verdict::Pressure);
        threshold!(direct_reclaim_pages_per_sec, PRESSURE_DIRECT_RECLAIM_PPS, Verdict::Pressure);
        threshold!(allocstall_per_sec, PRESSURE_ALLOCSTALL_PS, Verdict::Pressure);
        threshold!(slice_used_frac_of_high, TIGHT_SLICE_HIGH_FRAC, Verdict::Tight);
        expect!(available(10), Verdict::Tight);
        expect!(available(9), Verdict::Pressure);
        expect!(available(20), Verdict::Ok);
        expect!(available(19), Verdict::Tight);
        let refault_pair = |r, m| MemHealthInput {
            refault_pages_per_sec: Some(r),
            majflt_per_sec: Some(m),
            ..MemHealthInput::default()
        };
        expect!(refault_pair(THRASH_REFAULT_PPS - EPS, THRASH_MAJFLT_PS + EPS), Verdict::Unknown);
        expect!(refault_pair(THRASH_REFAULT_PPS + EPS, THRASH_MAJFLT_PS - EPS), Verdict::Unknown);
        expect!(refault_pair(THRASH_REFAULT_PPS + EPS, THRASH_MAJFLT_PS + EPS), Verdict::Thrashing);
    }

    #[test]
    fn swap_rule_is_a_two_by_two_and_uses_strict_half_threshold() {
        let half_used_free = (100.0 * (1.0 - TIGHT_SWAP_USED_PCT / 100.0)) as u64;
        for (free, rate, expected) in [
            (half_used_free - 1, 1.0, true),
            (half_used_free - 1, 0.0, false),
            (half_used_free, 1.0, false),
            (half_used_free, 0.0, false),
        ] {
            let input = swap(100, free, Some(rate));
            assert_eq!(has_reason(input, Reason::SwapActive), expected);
        }
    }

    #[test]
    fn missing_non_finite_and_zero_totals_stay_unknown() {
        let empty = MemHealthInput::default();
        let result = evaluate(&empty);
        assert_eq!(result.verdict, Verdict::Unknown);
        assert!(result.reasons.is_empty() && result.no_psi);
        let fields: [fn(f64) -> MemHealthInput; 9] = [
            |x| input_field!(secs_since_oom_kill, x), |x| input_field!(psi_full_avg10, x),
            |x| input_field!(refault_pages_per_sec, x), |x| input_field!(psi_some_avg10, x),
            |x| input_field!(direct_reclaim_pages_per_sec, x), |x| input_field!(allocstall_per_sec, x),
            |x| input_field!(slice_used_frac_of_high, x), |x| input_field!(swap_in_per_sec, x),
            |x| input_field!(majflt_per_sec, x),
        ];
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for make in fields {
                assert_eq!(evaluate(&make(value)), result);
            }
            same!(
                paired(Some(value), Some(300.0), None),
                paired(None, Some(300.0), None)
            );
            same!(
                paired(Some(6000.0), Some(value), None),
                paired(Some(6000.0), None, None)
            );
            same!(paired(None, None, Some(value)), paired(None, None, None));
        }
        assert_eq!(
            verdict(input_field!(psi_some_avg10, 6.0)),
            Verdict::Pressure
        );
        check_verdict(memory(100, 80, empty), Verdict::Ok);
        check_verdict(memory(0, 0, empty), Verdict::Unknown);
        assert!(!has_reason(swap(0, 0, Some(1.0)), Reason::SwapActive));
        assert!(!has_reason(memory(100, 150, empty), Reason::AvailableLow));
    }

    #[test]
    fn rate_sweeps_are_monotone_and_available_sweep_is_reverse() {
        let fields: [fn(f64) -> MemHealthInput; 5] = [
            |x| input_field!(psi_some_avg10, x), |x| input_field!(psi_full_avg10, x),
            |x| input_field!(direct_reclaim_pages_per_sec, x), |x| input_field!(allocstall_per_sec, x),
            |x| input_field!(slice_used_frac_of_high, x),
        ];
        for make in fields {
            let mut previous = 0;
            for value in [0.0, 1.0, 5.0, 10.0, 200.0, 1000.0, 5000.0, 10000.0] {
                let severity = verdict(known(make(value))).severity().unwrap();
                assert!(severity >= previous);
                previous = severity;
            }
        }
        let mut previous = 0;
        for value in [0.0, 1.0, 5.0, 10.0, 200.0, 1000.0, 5000.0, 10000.0] {
            let coupled = MemHealthInput {
                refault_pages_per_sec: Some(value),
                majflt_per_sec: Some(value),
                swap_total_bytes: Some(100),
                swap_free_bytes: Some(0),
                swap_in_per_sec: Some(value),
                ..known(MemHealthInput::default())
            };
            let severity = verdict(coupled).severity().unwrap();
            assert!(severity >= previous);
            previous = severity;
        }
        let mut previous = 0;
        for value in [100, 80, 50, 20, 19, 9, 0] {
            let severity = verdict(available(value)).severity().unwrap();
            assert!(severity >= previous);
            previous = severity;
        }
    }

    #[test]
    fn os_levels_and_mac_level_conversion_match_policy() {
        assert_eq!(OsPressure::from_mac_level(1), Some(OsPressure::MacNormal));
        assert_eq!(OsPressure::from_mac_level(2), Some(OsPressure::MacWarn));
        assert_eq!(OsPressure::from_mac_level(4), Some(OsPressure::MacCritical));
        assert_eq!(OsPressure::from_mac_level(3), None);
        assert_eq!(OsPressure::from_mac_level(0), None);
        for (level, expected) in [
            (OsPressure::MacNormal, Verdict::Ok), (OsPressure::MacWarn, Verdict::Pressure),
            (OsPressure::MacCritical, Verdict::Thrashing), (OsPressure::WinHigh, Verdict::Ok),
            (OsPressure::WinNeither, Verdict::Ok), (OsPressure::WinLow, Verdict::Pressure),
        ] {
            let result = evaluate(&os(level));
            assert_eq!(result.verdict, expected);
            assert!(result.reasons.contains(Reason::OsLevel));
            assert!(!result.no_psi);
        }
    }

    #[test]
    fn reason_iteration_and_verdict_labels_are_stable() {
        let all = evaluate(&MemHealthInput {
            mem_total_bytes: Some(100), mem_available_bytes: Some(5),
            swap_total_bytes: Some(100), swap_free_bytes: Some(0), swap_in_per_sec: Some(1.0),
            psi_some_avg10: Some(5.0), psi_full_avg10: Some(10.0),
            refault_pages_per_sec: Some(5000.0), majflt_per_sec: Some(200.0),
            direct_reclaim_pages_per_sec: Some(1000.0), allocstall_per_sec: Some(10.0),
            secs_since_oom_kill: Some(60.0), slice_used_frac_of_high: Some(0.85),
            os_level: Some(OsPressure::MacWarn),
        });
        assert_eq!(all.verdict, Verdict::Oom);
        assert_eq!(
            verdict(MemHealthInput {
                psi_some_avg10: Some(PRESSURE_PSI_SOME_AVG10),
                slice_used_frac_of_high: Some(TIGHT_SLICE_HIGH_FRAC),
                ..MemHealthInput::default()
            }),
            Verdict::Pressure
        );
        assert_eq!(
            all.reasons.iter().collect::<Vec<_>>(),
            [
                Reason::OomRecent, Reason::PsiFull, Reason::RefaultMajflt, Reason::PsiSome,
                Reason::DirectReclaim, Reason::AllocStall, Reason::AvailableLow, Reason::AvailableTight,
                Reason::SwapActive, Reason::SliceHigh,
                Reason::OsLevel,
            ]
        );
        assert!(Reasons::default().is_empty());
        assert_eq!(Reasons::default().iter().count(), 0);
        assert_eq!([Verdict::Unknown.label(), Verdict::Ok.label(), Verdict::Tight.label(), Verdict::Pressure.label(), Verdict::Thrashing.label(), Verdict::Oom.label()], ["unknown", "ok", "tight", "pressure", "thrashing", "oom"]);
        assert_eq!(Verdict::Unknown.severity(), None);
        assert_eq!(Verdict::Oom.severity(), Some(4));
    }
}
