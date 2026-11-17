//! Vercel cost accrual for `thegn fleet cost`: the current session's active
//! CPU and memory, and snapshot storage, priced at the Pro iad1 rates. An
//! estimate for the current session only.

/// The current session's usage fields (all optional in the API response).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionUsage {
    pub active_cpu_ms: Option<u64>,
    pub memory_mb: Option<u64>,
    pub vcpus: Option<u32>,
    pub started_at_ms: Option<i64>,
    pub stopped_at_ms: Option<i64>,
}

/// One snapshot of a sandbox.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotUsage {
    pub id: String,
    pub size_bytes: u64,
    pub expires_at_ms: Option<i64>,
    pub status: String,
}

/// Unit prices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub cpu_usd_per_vcpu_hour: f64,
    pub mem_usd_per_gb_hour: f64,
    pub snapshot_usd_per_gb_month: f64,
}

/// Vercel Pro, iad1 (the b154 rates; see vercel.com/docs/sandbox/pricing).
pub const PRO_IAD1: Rates = Rates {
    cpu_usd_per_vcpu_hour: 0.128,
    mem_usd_per_gb_hour: 0.0212,
    snapshot_usd_per_gb_month: 0.08,
};

/// Accrued spend for one session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accrual {
    pub cpu_usd: f64,
    pub memory_usd: f64,
}

/// The current session's accrued spend at `now_ms`, or `None` when the
/// response lacks the fields to price it.
///
/// CPU = `active_cpu_ms / 3_600_000 × rate`; it is NOT multiplied by vCPUs
/// (UNVERIFIED whether the API value is already per-vCPU). Memory is
/// `memory_mb / 1024 × wall hours × rate`.
pub fn session_accrual(u: &SessionUsage, now_ms: i64, r: &Rates) -> Option<Accrual> {
    let start = u.started_at_ms?;
    let end = u.stopped_at_ms.unwrap_or(now_ms);
    let span_ms = end.checked_sub(start)?;
    if span_ms < 0 {
        return None;
    }
    let wall_h = span_ms as f64 / 3_600_000.0;
    let cpu_h = u.active_cpu_ms.unwrap_or(0) as f64 / 3_600_000.0;
    let mem_gb = u.memory_mb.unwrap_or(0) as f64 / 1024.0;
    let a = Accrual {
        cpu_usd: cpu_h * r.cpu_usd_per_vcpu_hour,
        memory_usd: mem_gb * wall_h * r.mem_usd_per_gb_hour,
    };
    (a.cpu_usd.is_finite() && a.memory_usd.is_finite()).then_some(a)
}

/// Monthly storage cost of the `created` snapshots.
pub fn snapshot_month_usd(s: &[SnapshotUsage], r: &Rates) -> f64 {
    let bytes: f64 = s
        .iter()
        .filter(|x| x.status == "created")
        .map(|x| x.size_bytes as f64)
        .sum();
    bytes / (1024.0 * 1024.0 * 1024.0) * r.snapshot_usd_per_gb_month
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u() -> SessionUsage {
        SessionUsage {
            active_cpu_ms: Some(3_600_000),
            memory_mb: Some(2048),
            vcpus: Some(2),
            started_at_ms: Some(0),
            stopped_at_ms: Some(7_200_000),
        }
    }

    #[test]
    fn accrual_basic() {
        let a = session_accrual(&u(), 0, &PRO_IAD1).unwrap();
        assert!((a.cpu_usd - 0.128).abs() < 1e-9);
        assert!((a.memory_usd - 2.0 * 2.0 * 0.0212).abs() < 1e-9);
    }

    #[test]
    fn accrual_uses_now_when_running_and_boundaries() {
        let mut x = u();
        x.stopped_at_ms = None;
        let a = session_accrual(&x, 3_600_000, &PRO_IAD1).unwrap();
        assert!((a.memory_usd - 2.0 * 0.0212).abs() < 1e-9);
        // zero span
        assert!(session_accrual(&x, 0, &PRO_IAD1).is_some());
        // negative span
        assert!(session_accrual(&x, -1, &PRO_IAD1).is_none());
        // missing start
        x.started_at_ms = None;
        assert!(session_accrual(&x, 1, &PRO_IAD1).is_none());
        // missing optional metrics price as zero
        let y = SessionUsage {
            started_at_ms: Some(0),
            ..Default::default()
        };
        let a = session_accrual(&y, 10, &PRO_IAD1).unwrap();
        assert_eq!((a.cpu_usd, a.memory_usd), (0.0, 0.0));
    }

    #[test]
    fn accrual_overflow_is_none() {
        let x = SessionUsage {
            started_at_ms: Some(i64::MIN),
            stopped_at_ms: Some(i64::MAX),
            ..Default::default()
        };
        assert!(session_accrual(&x, 0, &PRO_IAD1).is_none());
        let inf = Rates {
            cpu_usd_per_vcpu_hour: f64::INFINITY,
            ..PRO_IAD1
        };
        assert!(session_accrual(&u(), 0, &inf).is_none());
    }

    #[test]
    fn snapshots_priced_only_when_created() {
        let s = |size, st: &str| SnapshotUsage {
            id: "s".into(),
            size_bytes: size,
            expires_at_ms: None,
            status: st.into(),
        };
        let gb = 1024u64 * 1024 * 1024;
        let v = vec![
            s(gb, "created"),
            s(gb, "deleted"),
            s(gb, "failed"),
            s(gb, "created"),
        ];
        assert!((snapshot_month_usd(&v, &PRO_IAD1) - 0.16).abs() < 1e-9);
        assert_eq!(snapshot_month_usd(&[], &PRO_IAD1), 0.0);
    }
}
