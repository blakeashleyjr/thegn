//! The placement **capacity index** — pure vocabulary for deciding whether a
//! sandbox's declared resource floor fits on a host. Integer units only
//! (milli-cores / MiB): decisions must be deterministic, so no float ever
//! enters the comparison math. The broker ([`crate::scheduler`]) consumes
//! these; persistence lives in `db_placement.rs`; all I/O stays in the host
//! crate's drivers.
//!
//! The load-bearing rule: `fits` compares **declared floors** (Σ of reserved
//! tenants + the candidate) against **spec × overcommit** per axis. Measured
//! load is observational (UI ranking hints) and never a capacity source — a
//! Managed host's spec is authoritative from create time, and scheduling off
//! live samples would make the decision nondeterministic.

use serde::{Deserialize, Serialize};

/// Who created a host and therefore what thegn may do to it. `Managed`
/// hosts are created and destroyed by thegn (autoscale, cloud reach);
/// `Independent` hosts are user-owned machines thegn was pointed at —
/// never destroyed, never machine-image-rebuilt (the destroy handle does not
/// exist for them, by construction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostOwnership {
    Managed,
    Independent,
}

impl HostOwnership {
    pub fn as_str(self) -> &'static str {
        match self {
            HostOwnership::Managed => "managed",
            HostOwnership::Independent => "independent",
        }
    }
    pub fn parse(s: &str) -> Option<HostOwnership> {
        match s.trim() {
            "managed" => Some(HostOwnership::Managed),
            "independent" => Some(HostOwnership::Independent),
            _ => None,
        }
    }
}

impl std::fmt::Display for HostOwnership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A sandbox's declared resource ask. Floors are what the scheduler reserves;
/// ceilings (when declared) feed the container `--cpus`/`--memory` limits and
/// are NOT part of the fits math (bursting up to the host ceiling is the point
/// of overcommit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceReq {
    pub cpu_floor_milli: u32,
    pub mem_floor_mb: u64,
    pub cpu_ceiling_milli: Option<u32>,
    pub mem_ceiling_mb: Option<u64>,
}

impl Default for ResourceReq {
    /// The engine default when neither the env nor `[placement.default_resources]`
    /// declares an ask: 1 core / 2 GiB — small enough to pack several sandboxes
    /// on a commodity box, large enough that a toolchain build isn't starved.
    fn default() -> Self {
        ResourceReq {
            cpu_floor_milli: 1000,
            mem_floor_mb: 2048,
            cpu_ceiling_milli: None,
            mem_ceiling_mb: None,
        }
    }
}

/// Parse a cpu quantity into milli-cores. Accepts the same forms as the
/// container `--cpus` flag plus a k8s-style `m` suffix: `"2"` ⇒ 2000,
/// `"0.5"` ⇒ 500, `"500m"` ⇒ 500. `None` for junk or zero.
pub fn parse_cpu_milli(s: &str) -> Option<u32> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let milli = if let Some(m) = t.strip_suffix(['m', 'M']) {
        m.trim().parse::<u32>().ok()?
    } else {
        // Decimal cores; parse manually to avoid float rounding surprises.
        let (whole, frac) = match t.split_once('.') {
            Some((w, f)) => (w, f),
            None => (t, ""),
        };
        if whole.is_empty() && frac.is_empty() {
            return None;
        }
        let whole: u32 = if whole.is_empty() {
            0
        } else {
            whole.parse().ok()?
        };
        if frac.len() > 3 || !frac.chars().all(|c| c.is_ascii_digit()) {
            if !frac.is_empty() {
                return None;
            }
            0
        } else if frac.is_empty() {
            0
        } else {
            // "5" ⇒ 500, "25" ⇒ 250, "125" ⇒ 125.
            let padded = format!("{frac:0<3}");
            let f: u32 = padded.parse().ok()?;
            return Some(whole.checked_mul(1000)?.checked_add(f)?).filter(|&v| v > 0);
        };
        whole.checked_mul(1000)?
    };
    (milli > 0).then_some(milli)
}

/// Parse a memory quantity into MiB. Suffixes `k`/`m`/`g`/`t` (optionally
/// `b`/`ib`, any case) are binary units; a bare number is already MiB (the
/// documented `[env.<n>.resources]` grammar). `None` for junk or zero.
pub fn parse_mem_mb(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let (num, unit) = {
        let idx = t
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(t.len());
        (t[..idx].trim(), t[idx..].trim())
    };
    if num.is_empty() {
        return None;
    }
    // Kib-precision fixed-point so "0.5g" works without float drift.
    let kib: u64 = {
        let (whole, frac) = match num.split_once('.') {
            Some((w, f)) => (w, f),
            None => (num, ""),
        };
        if frac.len() > 3 || !frac.chars().all(|c| c.is_ascii_digit()) && !frac.is_empty() {
            return None;
        }
        let whole: u64 = if whole.is_empty() {
            0
        } else {
            whole.parse().ok()?
        };
        let frac_milli: u64 = if frac.is_empty() {
            0
        } else {
            format!("{frac:0<3}").parse().ok()?
        };
        let unit_kib: u64 = match unit {
            "k" | "kb" | "kib" => 1,
            "m" | "mb" | "mib" | "" => 1024,
            "g" | "gb" | "gib" => 1024 * 1024,
            "t" | "tb" | "tib" => 1024 * 1024 * 1024,
            _ => return None,
        };
        whole
            .checked_mul(unit_kib)?
            .checked_add(frac_milli.checked_mul(unit_kib)? / 1000)?
    };
    let mb = kib / 1024;
    (mb > 0).then_some(mb)
}

/// A host's declared machine size. Authoritative-from-create for Managed
/// hosts (the autoscale template / provider plan); assumed or probed for
/// Independent hosts (a later change fills those sources in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpec {
    pub cpu_milli: u32,
    pub mem_mb: u64,
}

/// Σ of the declared floors of a host's live (reserved or active) tenants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReservedTotals {
    pub cpu_milli: u64,
    pub mem_mb: u64,
    pub tenants: u32,
}

/// An observational load sample (panel display, spread-ranking hints). Never
/// consulted by [`fits`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MeasuredLoad {
    pub cpu_milli: u64,
    pub mem_mb: u64,
    pub at: i64,
}

/// The per-host capacity index the broker snapshots before deciding.
#[derive(Debug, Clone, PartialEq)]
pub struct HostCapacity {
    pub ownership: HostOwnership,
    /// `None` ⇒ unknown machine size ⇒ never pack-eligible (an overcommit
    /// ceiling over an unknown base is meaningless); Dedicated may still use it.
    pub spec: Option<HostSpec>,
    /// Burst ceiling = `spec × pct / 100` per axis. 100 = no overcommit.
    pub overcommit_cpu_pct: u32,
    pub overcommit_mem_pct: u32,
    pub reserved: ReservedTotals,
    pub measured: Option<MeasuredLoad>,
}

impl HostCapacity {
    /// The packing ceilings `(cpu_milli, mem_mb)`, `None` when the spec is
    /// unknown.
    pub fn ceilings(&self) -> Option<(u64, u64)> {
        let spec = self.spec?;
        let pct = |p: u32| if p == 0 { 100 } else { p } as u64;
        Some((
            u64::from(spec.cpu_milli) * pct(self.overcommit_cpu_pct) / 100,
            spec.mem_mb * pct(self.overcommit_mem_pct) / 100,
        ))
    }
}

/// The capacity rule: `reserved_floor + req.floor ≤ spec × overcommit`,
/// independently for cpu and mem. Unknown spec ⇒ `false`.
pub fn fits(cap: &HostCapacity, req: &ResourceReq) -> bool {
    let Some((cpu_ceiling, mem_ceiling)) = cap.ceilings() else {
        return false;
    };
    cap.reserved.cpu_milli + u64::from(req.cpu_floor_milli) <= cpu_ceiling
        && cap.reserved.mem_mb + req.mem_floor_mb <= mem_ceiling
}

/// Utilization for deterministic ranking: the more-loaded axis's reserved
/// share of its ceiling, in permille, clamped to 0..=1000. Unknown spec ⇒
/// 1000 (sorts as full).
pub fn utilization_permille(cap: &HostCapacity) -> u32 {
    let Some((cpu_ceiling, mem_ceiling)) = cap.ceilings() else {
        return 1000;
    };
    let share = |used: u64, ceiling: u64| -> u32 {
        if ceiling == 0 {
            return 1000;
        }
        ((used.saturating_mul(1000)) / ceiling).min(1000) as u32
    };
    share(cap.reserved.cpu_milli, cpu_ceiling).max(share(cap.reserved.mem_mb, mem_ceiling))
}

/// Convert a config overcommit ratio (`2.0`) to the integer percent the index
/// uses (`200`). Non-finite / sub-1.0 values clamp to 100 (never *under*-commit
/// the declared spec by config error); a hard sanity ceiling of 16x.
pub fn overcommit_pct(ratio: f64) -> u32 {
    if ratio.is_nan() || ratio < 1.0 {
        return 100;
    }
    ((ratio.min(16.0)) * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_round_trips_and_rejects_junk() {
        for o in [HostOwnership::Managed, HostOwnership::Independent] {
            assert_eq!(HostOwnership::parse(o.as_str()), Some(o));
            assert_eq!(o.to_string(), o.as_str());
        }
        assert_eq!(
            HostOwnership::parse(" managed "),
            Some(HostOwnership::Managed)
        );
        assert_eq!(HostOwnership::parse("rented"), None);
        assert_eq!(HostOwnership::parse(""), None);
    }

    #[test]
    fn cpu_grammar_table() {
        // (input, expected milli)
        for (s, want) in [
            ("2", Some(2000)),
            ("1", Some(1000)),
            ("0.5", Some(500)),
            (".5", Some(500)),
            ("1.25", Some(1250)),
            ("0.125", Some(125)),
            ("500m", Some(500)),
            ("1500M", Some(1500)),
            (" 2 ", Some(2000)),
            ("16", Some(16000)),
        ] {
            assert_eq!(parse_cpu_milli(s), want, "{s:?}");
        }
        for junk in [
            "", "0", "0m", "0.0", "-1", "two", "1.2345", "1.0x", "m", ".",
        ] {
            assert_eq!(parse_cpu_milli(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn mem_grammar_table() {
        for (s, want) in [
            ("4096", Some(4096)), // bare = MiB
            ("512m", Some(512)),
            ("512MB", Some(512)),
            ("512MiB", Some(512)),
            ("4g", Some(4096)),
            ("4GB", Some(4096)),
            ("0.5g", Some(512)),
            ("1t", Some(1024 * 1024)),
            ("2048k", Some(2)),
            (" 8 g ", Some(8192)),
        ] {
            assert_eq!(parse_mem_mb(s), want, "{s:?}");
        }
        for junk in [
            "",
            "0",
            "0g",
            "-4g",
            "4x",
            "lots",
            "g",
            "1.2345g",
            "512kb extra",
        ] {
            assert_eq!(parse_mem_mb(junk), None, "{junk:?}");
        }
        // Sub-MiB quantities round down to zero ⇒ rejected (a floor of 0 is junk).
        assert_eq!(parse_mem_mb("512k"), None);
    }

    fn cap(
        spec: Option<(u32, u64)>,
        oc_cpu: u32,
        oc_mem: u32,
        reserved: (u64, u64, u32),
    ) -> HostCapacity {
        HostCapacity {
            ownership: HostOwnership::Managed,
            spec: spec.map(|(c, m)| HostSpec {
                cpu_milli: c,
                mem_mb: m,
            }),
            overcommit_cpu_pct: oc_cpu,
            overcommit_mem_pct: oc_mem,
            reserved: ReservedTotals {
                cpu_milli: reserved.0,
                mem_mb: reserved.1,
                tenants: reserved.2,
            },
            measured: None,
        }
    }

    fn req(cpu: u32, mem: u64) -> ResourceReq {
        ResourceReq {
            cpu_floor_milli: cpu,
            mem_floor_mb: mem,
            cpu_ceiling_milli: None,
            mem_ceiling_mb: None,
        }
    }

    #[test]
    fn fits_boundary_matrix() {
        // 4 cores / 8 GiB, no overcommit, empty host.
        let empty = cap(Some((4000, 8192)), 100, 100, (0, 0, 0));
        assert!(fits(&empty, &req(4000, 8192)), "exact fit");
        assert!(!fits(&empty, &req(4001, 8192)), "+1 milli over cpu");
        assert!(!fits(&empty, &req(4000, 8193)), "+1 MiB over mem");

        // Reserved 3 cores / 6 GiB: 1 core / 2 GiB left.
        let loaded = cap(Some((4000, 8192)), 100, 100, (3000, 6144, 3));
        assert!(fits(&loaded, &req(1000, 2048)), "exactly the remainder");
        assert!(!fits(&loaded, &req(1001, 2048)));
        assert!(!fits(&loaded, &req(1000, 2049)));

        // 2x cpu overcommit doubles the cpu ceiling only.
        let oc = cap(Some((4000, 8192)), 200, 100, (4000, 4096, 2));
        assert!(fits(&oc, &req(4000, 4096)), "cpu bursts past spec at 200%");
        assert!(!fits(&oc, &req(4001, 4096)));
        assert!(!fits(&oc, &req(1000, 4097)), "mem still uncommitted");

        // Axes are independent: plenty of cpu can't compensate for mem.
        let mem_full = cap(Some((16000, 4096)), 100, 100, (0, 4096, 1));
        assert!(!fits(&mem_full, &req(100, 1)));

        // 150% both axes.
        let both = cap(Some((2000, 2048)), 150, 150, (0, 0, 0));
        assert!(fits(&both, &req(3000, 3072)));
        assert!(!fits(&both, &req(3001, 3072)));
    }

    #[test]
    fn unknown_spec_never_fits() {
        let unknown = cap(None, 400, 400, (0, 0, 0));
        assert!(!fits(&unknown, &req(1, 1)), "even the tiniest ask");
        assert_eq!(unknown.ceilings(), None);
        assert_eq!(utilization_permille(&unknown), 1000);
    }

    #[test]
    fn zero_overcommit_pct_reads_as_100() {
        let z = cap(Some((1000, 1024)), 0, 0, (0, 0, 0));
        assert_eq!(z.ceilings(), Some((1000, 1024)));
    }

    #[test]
    fn utilization_takes_the_hotter_axis() {
        let c = cap(Some((4000, 8192)), 100, 100, (1000, 6144, 2));
        // cpu 25%, mem 75% ⇒ 750‰.
        assert_eq!(utilization_permille(&c), 750);
        let idle = cap(Some((4000, 8192)), 100, 100, (0, 0, 0));
        assert_eq!(utilization_permille(&idle), 0);
        let over = cap(Some((1000, 1024)), 100, 100, (5000, 0, 1));
        assert_eq!(utilization_permille(&over), 1000, "clamped");
    }

    #[test]
    fn overcommit_pct_conversion() {
        assert_eq!(overcommit_pct(1.0), 100);
        assert_eq!(overcommit_pct(2.0), 200);
        assert_eq!(overcommit_pct(1.5), 150);
        assert_eq!(overcommit_pct(0.5), 100, "sub-1.0 clamps up");
        assert_eq!(overcommit_pct(0.0), 100);
        assert_eq!(overcommit_pct(-3.0), 100);
        assert_eq!(overcommit_pct(f64::NAN), 100);
        assert_eq!(overcommit_pct(f64::INFINITY), 1600, "sanity ceiling 16x");
        assert_eq!(overcommit_pct(64.0), 1600);
    }

    #[test]
    fn default_req_is_one_core_two_gib() {
        let d = ResourceReq::default();
        assert_eq!(d.cpu_floor_milli, 1000);
        assert_eq!(d.mem_floor_mb, 2048);
        assert!(d.cpu_ceiling_milli.is_none() && d.mem_ceiling_mb.is_none());
    }

    #[test]
    fn types_round_trip_serde() {
        let r = ResourceReq {
            cpu_floor_milli: 1500,
            mem_floor_mb: 3072,
            cpu_ceiling_milli: Some(4000),
            mem_ceiling_mb: None,
        };
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<ResourceReq>(&j).unwrap(), r);
        let m = MeasuredLoad {
            cpu_milli: 1,
            mem_mb: 2,
            at: 3,
        };
        let j = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<MeasuredLoad>(&j).unwrap(), m);
    }
}
