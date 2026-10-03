//! GPU utilization — the one metric sysinfo does not provide. Linux exposes it
//! via sysfs (`amdgpu`/`i915`: `gpu_busy_percent`) or `nvidia-smi`; macOS via
//! IOKit accelerator statistics, read with `ioreg` (no root, unlike
//! `powermetrics`). Where none of those answer, the monitor
//! ([`crate::gpu_monitor`]) reports no reading and the widget hides — the same behaviour as a Linux box
//! with no detectable GPU.
//!
//! Utilization is the only field every backend fills. VRAM comes from sysfs and
//! nvidia-smi; temperature and power only from nvidia-smi. macOS reports
//! utilization alone: unified memory means there is no VRAM to speak of, and
//! temperature/power need root.

/// A GPU sample: utilization plus the extras a richer detail popup shows. Every
/// field is `Option` — a backend fills what it can (sysfs util is universal;
/// VRAM/temp/power depend on the vendor path) and the rest render as absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct GpuReading {
    /// Utilization 0–100.
    pub util_pct: Option<u8>,
    /// (used, total) VRAM in MiB.
    pub mem_mib: Option<(u64, u64)>,
    /// Core temperature in °C.
    pub temp_c: Option<f32>,
    /// Board power draw in watts.
    pub power_w: Option<f32>,
}

/// The `ioreg` query behind the macOS backend: one accelerator node, one level
/// deep, which is where `PerformanceStatistics` lives.
pub(crate) const IOREG_ARGS: [&str; 5] = ["-r", "-d", "1", "-c", "IOAccelerator"];

/// `nvidia-smi` query for every field at once (one spawn per sample).
pub(crate) const NVIDIA_QUERY_ARGS: [&str; 2] = [
    "--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
    "--format=csv,noheader,nounits",
];

/// The amdgpu/i915 `gpu_busy_percent` file, if any card exposes one. Two
/// directory reads, no subprocess — safe on the caller's thread.
pub(crate) fn find_sysfs() -> Option<std::path::PathBuf> {
    let cards = std::fs::read_dir("/sys/class/drm").ok()?;
    cards
        .flatten()
        .map(|c| c.path().join("device/gpu_busy_percent"))
        .find(|p| p.is_file())
}

/// Read a sample from sysfs: a handful of cheap file reads.
pub(crate) fn read_sysfs(path: &std::path::Path) -> GpuReading {
    let util_pct = std::fs::read_to_string(path)
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok());
    // VRAM counters live beside gpu_busy_percent in the device dir, in bytes;
    // convert to MiB. temp/power would need hwmon walking, which sysfs lays out
    // inconsistently, so leave them absent.
    let dev = path.parent();
    let vram = |name: &str| -> Option<u64> {
        let d = dev?;
        std::fs::read_to_string(d.join(name))
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
            .map(|b| b / (1024 * 1024))
    };
    let mem_mib = match (vram("mem_info_vram_used"), vram("mem_info_vram_total")) {
        (Some(u), Some(t)) if t > 0 => Some((u, t)),
        _ => None,
    };
    GpuReading {
        util_pct,
        mem_mib,
        ..Default::default()
    }
}

/// Parse `Device Utilization %` out of `ioreg -c IOAccelerator` output.
///
/// **`Device Utilization %`, specifically.** The same block carries
/// `Renderer Utilization %` and `Tiler Utilization %`, and those are the wrong
/// answer: measured on an M-series Mac, an LLM saturating the GPU showed
/// `Device` 98–100 while `Renderer` sat at 0–2 — the same 0–2 it reports when
/// the GPU is completely idle. A renderer-based gauge would therefore read ~0%
/// through a fully pinned GPU. `Device` covers compute and spans the real range
/// (idle mean 0.4, saturated mean 99.0).
///
/// The **maximum** across accelerator nodes: Apple silicon exposes one, but an
/// Intel Mac pairs an integrated and a discrete GPU, and taking the first could
/// report the idle integrated one while the discrete GPU is busy.
///
/// Memory, temperature and power stay absent. There is no VRAM to report on
/// unified memory (the block's `In use system memory` is ~28 GB of ~31 GB
/// `Alloc`, which as a used/total pair would show a permanently ~90%-full GPU),
/// and temperature/power need `powermetrics`, which needs root.
pub(crate) fn parse_ioaccel(out: &str) -> Option<GpuReading> {
    const KEY: &str = "\"Device Utilization %\"=";
    let util = out
        .match_indices(KEY)
        .filter_map(|(i, _)| {
            let rest = &out[i + KEY.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            rest[..end].parse::<u32>().ok()
        })
        // Clamp rather than let a driver-reported >100 wrap the u8.
        .map(|v| v.min(100) as u8)
        .max()?;
    Some(GpuReading {
        util_pct: Some(util),
        ..Default::default()
    })
}

/// Parse the first CSV row of the `nvidia-smi` query into a [`GpuReading`].
/// Fields are `util%, mem_used_MiB, mem_total_MiB, temp_C, power_W`; any that
/// nvidia-smi reports as `[N/A]` (unsupported) parse to `None` individually.
pub(crate) fn parse_nvidia(out: &str) -> Option<GpuReading> {
    let line = out.lines().next()?;
    let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    let u8f = |i: usize| f.get(i).and_then(|v| v.parse::<u8>().ok());
    let u64f = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
    let f32f = |i: usize| f.get(i).and_then(|v| v.parse::<f32>().ok());
    let mem_mib = match (u64f(1), u64f(2)) {
        (Some(u), Some(t)) if t > 0 => Some((u, t)),
        _ => None,
    };
    // Utilization is the one field every backend must supply; a row without it
    // is not a GPU sample (driver banner, error text) and must not overwrite
    // the last good reading.
    let util_pct = u8f(0)?;
    Some(GpuReading {
        util_pct: Some(util_pct),
        mem_mib,
        temp_c: f32f(3),
        power_w: f32f(4),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_nvidia_reads_all_fields() {
        let r = parse_nvidia("30, 2048, 8192, 54, 61.5\n").unwrap();
        assert_eq!(r.util_pct, Some(30));
        assert_eq!(r.mem_mib, Some((2048, 8192)));
        assert_eq!(r.temp_c, Some(54.0));
        assert_eq!(r.power_w, Some(61.5));
    }

    /// A verbatim `PerformanceStatistics` block captured from
    /// `ioreg -r -d 1 -c IOAccelerator` on an M-series Mac, GPU busy.
    const IOREG_BUSY: &str = r#"+-o AGXAcceleratorG17X  <class AGXAcceleratorG17X, id 0x10000076b, registered, matched, active, busy 0 (21188 ms), retain 66>
    {
      "PerformanceStatistics" = {"In use system memory (driver)"=0,"Alloc system memory"=31368167424,"Tiler Utilization %"=0,"recoveryCount"=0,"lastRecoveryTime"=0,"Renderer Utilization %"=1,"TiledSceneBytes"=1179648,"Device Utilization %"=99,"SplitSceneCount"=0,"Allocated PB Size"=75497472,"In use system memory"=28367667200}
      "IOMatchedAtBoot" = Yes
    }
"#;

    #[test]
    fn parse_ioaccel_reads_device_utilization_not_renderer() {
        let r = parse_ioaccel(IOREG_BUSY).unwrap();
        // 99, not the 1 sitting in `Renderer Utilization %` right beside it.
        // Measured on hardware: an LLM pinning the GPU shows Device 98-100 while
        // Renderer stays at the same 0-2 it reports when fully idle, so a
        // renderer-based gauge would read ~0% through a saturated GPU.
        assert_eq!(r.util_pct, Some(99));
        // Unified memory is not VRAM, and temp/power need root.
        assert_eq!(r.mem_mib, None);
        assert_eq!(r.temp_c, None);
        assert_eq!(r.power_w, None);
    }

    #[test]
    fn parse_ioaccel_takes_the_busiest_accelerator() {
        // An Intel Mac pairs an idle integrated GPU with a busy discrete one;
        // taking the first would report the idle one.
        let two = r#""Device Utilization %"=3,"x"=1
          "Device Utilization %"=87,"y"=2"#;
        assert_eq!(parse_ioaccel(two).unwrap().util_pct, Some(87));
    }

    #[test]
    fn parse_ioaccel_declines_rather_than_inventing_a_zero() {
        // No counter ⇒ None, so `probe` leaves the backend unselected and the
        // widget hides, instead of pinning a fake 0% forever.
        assert!(parse_ioaccel("").is_none());
        assert!(parse_ioaccel("no counters here").is_none());
        assert!(
            parse_ioaccel(r#""Renderer Utilization %"=42"#).is_none(),
            "the renderer counter alone must not satisfy the probe"
        );
        assert!(parse_ioaccel(r#""Device Utilization %"=""#).is_none());
        assert!(parse_ioaccel(r#""Device Utilization %"=abc"#).is_none());
    }

    #[test]
    fn parse_ioaccel_covers_the_whole_range_and_clamps() {
        for v in [0u32, 1, 50, 99, 100] {
            let s = format!("\"Device Utilization %\"={v}");
            assert_eq!(parse_ioaccel(&s).unwrap().util_pct, Some(v as u8));
        }
        // A driver reporting out of range must clamp, not wrap the u8 (256 -> 0
        // would read as an idle GPU).
        assert_eq!(
            parse_ioaccel(r#""Device Utilization %"=256"#)
                .unwrap()
                .util_pct,
            Some(100)
        );
    }

    #[test]
    fn parse_nvidia_tolerates_na_columns() {
        // Laptop dGPUs commonly report power.draw as "[N/A]".
        let r = parse_nvidia("5, 512, 4096, 45, [N/A]").unwrap();
        assert_eq!(r.util_pct, Some(5));
        assert_eq!(r.power_w, None);
        assert_eq!(r.mem_mib, Some((512, 4096)));
        // A zero total suppresses the VRAM pair rather than dividing by zero.
        assert_eq!(parse_nvidia("5, 0, 0, 45, 10").unwrap().mem_mib, None);
    }

    #[test]
    fn parse_nvidia_rejects_rows_without_utilization() {
        assert!(parse_nvidia("").is_none());
        assert!(parse_nvidia("NVIDIA-SMI has failed because it couldn't communicate").is_none());
        assert!(parse_nvidia("[N/A], 1, 2, 3, 4").is_none());
    }
}
