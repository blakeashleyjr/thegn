//! The **headroom probe** contract — the pure half of the lightweight
//! "what's actually free on this box right now" sample every reachable host
//! gets (managed and independent alike). The script lives in the service
//! crate next to `PROBE_SCRIPT` (extend both together; the contract test
//! pins script↔parser agreement); this module owns the KEY=VALUE parser and
//! the independent-host capacity math.
//!
//! Role split (the resource-view doctrine): for a MANAGED host the sample is
//! observational (display + ranking hints — the create-time spec is
//! authoritative). For an INDEPENDENT host the sample is the only honest
//! capacity input, so [`independent_effective_ceiling`] compounds the two
//! guesses (declared size, probed size) conservatively: take the pessimist of
//! both, then overcommit, then a safety haircut — an uncontrolled box carries
//! invisible co-workloads and superzej has no eviction lever there.

use serde::{Deserialize, Serialize};

use crate::capacity::HostSpec;

/// One headroom sample, parsed from the script's KEY=VALUE output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Headroom {
    pub cpus: u32,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    /// 1-minute load average in milli (2.5 ⇒ 2500).
    pub load1_milli: u32,
    pub disk_free_bytes: u64,
    /// Running containers, when a runtime answered (`0` otherwise).
    pub containers_running: u32,
}

impl Headroom {
    /// The probed machine size as a spec (total, not available).
    pub fn as_spec(&self) -> HostSpec {
        HostSpec {
            cpu_milli: self.cpus.saturating_mul(1000),
            mem_mb: self.mem_total_kb / 1024,
        }
    }
}

/// Parse the headroom script's output. Same discipline as
/// [`crate::host::HostCaps::parse_probe`]: one `KEY=VALUE` per line, unknown
/// keys ignored (script and core evolve independently), required keys
/// (`NPROC`, `MEM_TOTAL_KB`, `MEM_AVAIL_KB`) missing ⇒ `Err`.
pub fn parse_headroom(out: &str) -> Result<Headroom, String> {
    let mut kv = std::collections::BTreeMap::new();
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            kv.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let num = |k: &str| -> Option<u64> { kv.get(k).and_then(|v| v.parse().ok()) };
    let cpus = num("NPROC").ok_or("headroom: missing NPROC")? as u32;
    if cpus == 0 {
        return Err("headroom: NPROC=0".into());
    }
    let mem_total_kb = num("MEM_TOTAL_KB").ok_or("headroom: missing MEM_TOTAL_KB")?;
    let mem_available_kb = num("MEM_AVAIL_KB").ok_or("headroom: missing MEM_AVAIL_KB")?;
    if mem_total_kb == 0 {
        return Err("headroom: MEM_TOTAL_KB=0".into());
    }
    Ok(Headroom {
        cpus,
        mem_total_kb,
        mem_available_kb,
        load1_milli: num("LOAD1_MILLI").unwrap_or(0) as u32,
        disk_free_bytes: num("DISK_FREE").unwrap_or(0),
        containers_running: num("CONTAINERS").unwrap_or(0) as u32,
    })
}

/// The effective PACKING ceiling for an independent host:
/// `min(declared, probed) × overcommit × safety`, per axis, integer math.
/// `None` when neither a declared capacity nor a probe exists — such a host
/// serves dedicated placements only.
///
/// `safety_pct` (config `[placement] independent_safety_pct`, default 85) is
/// clamped to 1..=100: it may only ever *shrink* the ceiling.
pub fn independent_effective_ceiling(
    declared: Option<HostSpec>,
    probed: Option<&Headroom>,
    overcommit_cpu_pct: u32,
    overcommit_mem_pct: u32,
    safety_pct: u32,
) -> Option<HostSpec> {
    let probed_spec = probed.map(Headroom::as_spec);
    let base = match (declared, probed_spec) {
        (Some(d), Some(p)) => HostSpec {
            cpu_milli: d.cpu_milli.min(p.cpu_milli),
            mem_mb: d.mem_mb.min(p.mem_mb),
        },
        (Some(d), None) => d,
        (None, Some(p)) => p,
        (None, None) => return None,
    };
    let safety = u64::from(safety_pct.clamp(1, 100));
    let oc = |pct: u32| u64::from(if pct == 0 { 100 } else { pct });
    Some(HostSpec {
        cpu_milli: (u64::from(base.cpu_milli) * oc(overcommit_cpu_pct) / 100 * safety / 100)
            .min(u64::from(u32::MAX)) as u32,
        mem_mb: base.mem_mb * oc(overcommit_mem_pct) / 100 * safety / 100,
    })
}

/// The live gate that dominates the static arithmetic on a box superzej does
/// not control: pack only while measured available memory (after the safety
/// haircut) covers the requested floor.
pub fn live_mem_gate_ok(probed: &Headroom, safety_pct: u32, req_floor_mb: u64) -> bool {
    let avail_mb = probed.mem_available_kb / 1024;
    avail_mb * u64::from(safety_pct.clamp(1, 100)) / 100 >= req_floor_mb
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "NPROC=8\nMEM_TOTAL_KB=32768000\nMEM_AVAIL_KB=16384000\n\
                        LOAD1_MILLI=2500\nDISK_FREE=100000000000\nCONTAINERS=3\n";

    #[test]
    fn parses_full_output() {
        let h = parse_headroom(FULL).unwrap();
        assert_eq!(h.cpus, 8);
        assert_eq!(h.mem_total_kb, 32_768_000);
        assert_eq!(h.mem_available_kb, 16_384_000);
        assert_eq!(h.load1_milli, 2500);
        assert_eq!(h.disk_free_bytes, 100_000_000_000);
        assert_eq!(h.containers_running, 3);
        assert_eq!(
            h.as_spec(),
            HostSpec {
                cpu_milli: 8000,
                mem_mb: 32_000
            }
        );
    }

    #[test]
    fn optional_keys_default_and_unknown_keys_ignored() {
        let h =
            parse_headroom("# v1\nNPROC=4\nMEM_TOTAL_KB=8000000\nMEM_AVAIL_KB=4000000\nFUTURE=1\n")
                .unwrap();
        assert_eq!(
            (h.load1_milli, h.disk_free_bytes, h.containers_running),
            (0, 0, 0)
        );
    }

    #[test]
    fn missing_or_zero_required_keys_err() {
        for junk in [
            "",
            "NPROC=4\n",
            "NPROC=4\nMEM_TOTAL_KB=1\n",
            "NPROC=0\nMEM_TOTAL_KB=1\nMEM_AVAIL_KB=1\n",
            "NPROC=4\nMEM_TOTAL_KB=0\nMEM_AVAIL_KB=1\n",
            "NPROC=four\nMEM_TOTAL_KB=1\nMEM_AVAIL_KB=1\n",
        ] {
            assert!(parse_headroom(junk).is_err(), "{junk:?}");
        }
    }

    fn spec(cpu: u32, mem: u64) -> HostSpec {
        HostSpec {
            cpu_milli: cpu,
            mem_mb: mem,
        }
    }

    #[test]
    fn ceiling_takes_the_pessimist_of_both_claims() {
        let probed = parse_headroom(FULL).unwrap(); // 8 cores / 32000 MiB
        // Declared 16 cores / 64 GiB: the probe caps it.
        let c =
            independent_effective_ceiling(Some(spec(16_000, 65_536)), Some(&probed), 100, 100, 100)
                .unwrap();
        assert_eq!(c, spec(8000, 32_000));
        // Declared smaller than probed: the declaration caps it.
        let c = independent_effective_ceiling(Some(spec(2000, 4096)), Some(&probed), 100, 100, 100)
            .unwrap();
        assert_eq!(c, spec(2000, 4096));
    }

    #[test]
    fn ceiling_single_source_and_none() {
        let probed = parse_headroom(FULL).unwrap();
        assert_eq!(
            independent_effective_ceiling(None, Some(&probed), 100, 100, 100).unwrap(),
            spec(8000, 32_000)
        );
        assert_eq!(
            independent_effective_ceiling(Some(spec(4000, 8192)), None, 100, 100, 100).unwrap(),
            spec(4000, 8192)
        );
        assert_eq!(
            independent_effective_ceiling(None, None, 200, 200, 85),
            None,
            "no source ⇒ dedicated-only"
        );
    }

    #[test]
    fn ceiling_applies_overcommit_then_safety() {
        // 8 cores × 200% × 85% = 13600 milli; 32000 MiB × 100% × 85% = 27200.
        let probed = parse_headroom(FULL).unwrap();
        let c = independent_effective_ceiling(None, Some(&probed), 200, 100, 85).unwrap();
        assert_eq!(c, spec(13_600, 27_200));
        // safety clamps: 0 ⇒ 1%, >100 ⇒ 100%.
        let floor = independent_effective_ceiling(None, Some(&probed), 100, 100, 0).unwrap();
        assert_eq!(floor, spec(80, 320));
        let full = independent_effective_ceiling(None, Some(&probed), 100, 100, 500).unwrap();
        assert_eq!(full, spec(8000, 32_000));
    }

    #[test]
    fn live_mem_gate_edges() {
        let probed = parse_headroom(FULL).unwrap(); // 16_000_000 KB avail = 15_625 MiB... (16384000/1024=16000)
        // 16000 MiB available × 85% = 13600 MiB budget.
        assert!(live_mem_gate_ok(&probed, 85, 13_600));
        assert!(!live_mem_gate_ok(&probed, 85, 13_601));
        assert!(live_mem_gate_ok(&probed, 100, 16_000));
        assert!(!live_mem_gate_ok(&probed, 100, 16_001));
    }

    #[test]
    fn headroom_serde_round_trip() {
        let h = parse_headroom(FULL).unwrap();
        let j = serde_json::to_string(&h).unwrap();
        assert_eq!(serde_json::from_str::<Headroom>(&j).unwrap(), h);
    }
}
