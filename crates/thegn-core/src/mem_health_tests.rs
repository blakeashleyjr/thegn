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
fn os(level: OsPressure) -> MemHealthInput {
    MemHealthInput {
        os_level: Some(level),
        ..MemHealthInput::default()
    }
}
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
    expect!(
        input_field!(secs_since_oom_kill, OOM_WINDOW_SECS),
        Verdict::Oom
    );
    expect!(
        input_field!(secs_since_oom_kill, OOM_WINDOW_SECS - EPS),
        Verdict::Oom
    );
    expect!(
        input_field!(secs_since_oom_kill, OOM_WINDOW_SECS + EPS),
        Verdict::Unknown
    );
    threshold!(psi_full_avg10, THRASH_PSI_FULL_AVG10, Verdict::Thrashing);
    threshold!(psi_some_avg10, PRESSURE_PSI_SOME_AVG10, Verdict::Pressure);
    threshold!(
        direct_reclaim_pages_per_sec,
        PRESSURE_DIRECT_RECLAIM_PPS,
        Verdict::Pressure
    );
    threshold!(
        allocstall_per_sec,
        PRESSURE_ALLOCSTALL_PS,
        Verdict::Pressure
    );
    threshold!(
        slice_used_frac_of_high,
        TIGHT_SLICE_HIGH_FRAC,
        Verdict::Tight
    );
    expect!(available(10), Verdict::Tight);
    expect!(available(9), Verdict::Pressure);
    expect!(available(20), Verdict::Ok);
    expect!(available(19), Verdict::Tight);
    let refault_pair = |r, m| MemHealthInput {
        refault_pages_per_sec: Some(r),
        majflt_per_sec: Some(m),
        ..MemHealthInput::default()
    };
    expect!(
        refault_pair(THRASH_REFAULT_PPS - EPS, THRASH_MAJFLT_PS + EPS),
        Verdict::Unknown
    );
    expect!(
        refault_pair(THRASH_REFAULT_PPS + EPS, THRASH_MAJFLT_PS - EPS),
        Verdict::Unknown
    );
    expect!(
        refault_pair(THRASH_REFAULT_PPS + EPS, THRASH_MAJFLT_PS + EPS),
        Verdict::Thrashing
    );
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
        |x| input_field!(secs_since_oom_kill, x),
        |x| input_field!(psi_full_avg10, x),
        |x| input_field!(refault_pages_per_sec, x),
        |x| input_field!(psi_some_avg10, x),
        |x| input_field!(direct_reclaim_pages_per_sec, x),
        |x| input_field!(allocstall_per_sec, x),
        |x| input_field!(slice_used_frac_of_high, x),
        |x| input_field!(swap_in_per_sec, x),
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
        |x| input_field!(psi_some_avg10, x),
        |x| input_field!(psi_full_avg10, x),
        |x| input_field!(direct_reclaim_pages_per_sec, x),
        |x| input_field!(allocstall_per_sec, x),
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
        (OsPressure::MacNormal, Verdict::Ok),
        (OsPressure::MacWarn, Verdict::Pressure),
        (OsPressure::MacCritical, Verdict::Thrashing),
        (OsPressure::WinHigh, Verdict::Ok),
        (OsPressure::WinNeither, Verdict::Ok),
        (OsPressure::WinLow, Verdict::Pressure),
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
        mem_total_bytes: Some(100),
        mem_available_bytes: Some(5),
        swap_total_bytes: Some(100),
        swap_free_bytes: Some(0),
        swap_in_per_sec: Some(1.0),
        psi_some_avg10: Some(5.0),
        psi_full_avg10: Some(10.0),
        refault_pages_per_sec: Some(5000.0),
        majflt_per_sec: Some(200.0),
        direct_reclaim_pages_per_sec: Some(1000.0),
        allocstall_per_sec: Some(10.0),
        secs_since_oom_kill: Some(60.0),
        slice_used_frac_of_high: Some(0.85),
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
            Reason::OomRecent,
            Reason::PsiFull,
            Reason::RefaultMajflt,
            Reason::PsiSome,
            Reason::DirectReclaim,
            Reason::AllocStall,
            Reason::AvailableLow,
            Reason::AvailableTight,
            Reason::SwapActive,
            Reason::SliceHigh,
            Reason::OsLevel,
        ]
    );
    assert!(Reasons::default().is_empty());
    assert_eq!(Reasons::default().iter().count(), 0);
    assert_eq!(
        [
            Verdict::Unknown.label(),
            Verdict::Ok.label(),
            Verdict::Tight.label(),
            Verdict::Pressure.label(),
            Verdict::Thrashing.label(),
            Verdict::Oom.label()
        ],
        ["unknown", "ok", "tight", "pressure", "thrashing", "oom"]
    );
    assert_eq!(Verdict::Unknown.severity(), None);
    assert_eq!(Verdict::Oom.severity(), Some(4));
}
