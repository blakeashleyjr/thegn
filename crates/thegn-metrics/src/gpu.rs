//! GPU utilization — the one metric sysinfo does not provide. Linux exposes it
//! via sysfs (`amdgpu`/`i915`: `gpu_busy_percent`) or `nvidia-smi`; macOS via
//! IOKit accelerator statistics, read with `ioreg` (no root, unlike
//! `powermetrics`). Where none of those answer, [`GpuProbe::probe`] resolves to
//! [`GpuProbe::None`] and the widget hides — the same behaviour as a Linux box
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

/// How GPU state is read (probed once at startup).
pub(crate) enum GpuProbe {
    /// amdgpu/i915 expose a percent file in sysfs; `.0` is
    /// `.../device/gpu_busy_percent`, whose parent holds the VRAM counters.
    Sysfs(std::path::PathBuf),
    /// NVIDIA via nvidia-smi.
    NvidiaSmi,
    /// macOS: IOKit accelerator statistics via `ioreg`. Not `cfg`-gated so the
    /// parser stays compiled — and therefore tested — on every platform; only
    /// [`GpuProbe::probe`] restricts *selecting* it to macOS.
    IoAccel,
    None,
}

/// The `ioreg` query behind [`GpuProbe::IoAccel`]: one accelerator node, one
/// level deep, which is where `PerformanceStatistics` lives.
const IOREG_ARGS: [&str; 5] = ["-r", "-d", "1", "-c", "IOAccelerator"];

impl GpuProbe {
    pub(crate) fn probe() -> GpuProbe {
        // Sysfs first (AMD/Intel — no subprocess per sample).
        if let Ok(cards) = std::fs::read_dir("/sys/class/drm") {
            for card in cards.flatten() {
                let p = card.path().join("device/gpu_busy_percent");
                if p.is_file() {
                    return GpuProbe::Sysfs(p);
                }
            }
        }
        let nvidia = std::process::Command::new("nvidia-smi")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if nvidia {
            return GpuProbe::NvidiaSmi;
        }
        // macOS: accept this backend only if the counter actually parses. A Mac
        // whose accelerator omits it stays `None` (widget hidden, today's
        // behaviour) rather than selecting a backend that always yields nothing.
        if cfg!(target_os = "macos") && read_ioaccel().is_some() {
            return GpuProbe::IoAccel;
        }
        GpuProbe::None
    }

    /// Whether reading a sample costs a subprocess.
    ///
    /// The sampler charges these to its slow tier: `sysfs` is a couple of file
    /// reads and can run every tick, but `nvidia-smi` and `ioreg` are process
    /// spawns (`ioreg` measured at 30–40ms), and paying that on every ~2s tick
    /// is real background CPU in a program whose headline invariant is ~0% idle.
    pub(crate) fn is_subprocess(&self) -> bool {
        matches!(self, GpuProbe::NvidiaSmi | GpuProbe::IoAccel)
    }

    /// Read a full GPU sample. The sysfs path is a handful of cheap file reads;
    /// the NVIDIA path spawns one `nvidia-smi` querying every field at once
    /// (ticker-thread only — no extra subprocess vs. reading util alone).
    pub(crate) fn read(&self) -> GpuReading {
        match self {
            GpuProbe::Sysfs(path) => {
                let util_pct = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|v| v.trim().parse::<u8>().ok());
                // VRAM counters live beside gpu_busy_percent in the device dir,
                // in bytes; convert to MiB. temp/power would need hwmon walking,
                // which sysfs lays out inconsistently, so leave them absent.
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
            GpuProbe::NvidiaSmi => std::process::Command::new("nvidia-smi")
                .args([
                    "--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
                    "--format=csv,noheader,nounits",
                ])
                .output()
                .ok()
                .and_then(|o| parse_nvidia(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or_default(),
            GpuProbe::IoAccel => read_ioaccel().unwrap_or_default(),
            GpuProbe::None => GpuReading::default(),
        }
    }
}

/// Run the `ioreg` accelerator query and parse it. `None` when the command is
/// missing/fails or the output carries no utilization counter.
fn read_ioaccel() -> Option<GpuReading> {
    let out = std::process::Command::new("ioreg")
        .args(IOREG_ARGS)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_ioaccel(&String::from_utf8_lossy(&out.stdout))
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
fn parse_ioaccel(out: &str) -> Option<GpuReading> {
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
fn parse_nvidia(out: &str) -> Option<GpuReading> {
    let line = out.lines().next()?;
    let f: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    let u8f = |i: usize| f.get(i).and_then(|v| v.parse::<u8>().ok());
    let u64f = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
    let f32f = |i: usize| f.get(i).and_then(|v| v.parse::<f32>().ok());
    let mem_mib = match (u64f(1), u64f(2)) {
        (Some(u), Some(t)) if t > 0 => Some((u, t)),
        _ => None,
    };
    Some(GpuReading {
        util_pct: u8f(0),
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
    fn subprocess_backends_are_flagged_for_the_slow_tier() {
        assert!(GpuProbe::NvidiaSmi.is_subprocess());
        assert!(GpuProbe::IoAccel.is_subprocess());
        // sysfs is a couple of file reads — cheap enough for every tick.
        assert!(!GpuProbe::Sysfs(std::path::PathBuf::from("/x")).is_subprocess());
        assert!(!GpuProbe::None.is_subprocess());
    }

    /// The live path on real hardware: `ioreg` runs, parses, and yields a value
    /// in range. Asserts no particular number — the GPU's load is whatever the
    /// machine happens to be doing — but catches the failures that matter: the
    /// command missing, the key renamed, or the probe declining on a Mac that
    /// does have an accelerator.
    #[cfg(target_os = "macos")]
    #[test]
    fn ioaccel_reads_this_mac() {
        let r = read_ioaccel().expect("every Mac has an IOAccelerator with the counter");
        let util = r
            .util_pct
            .expect("a parsed reading always carries utilization");
        assert!(util <= 100, "utilization out of range: {util}");
        // …and the probe actually selects it here, rather than falling to None
        // and hiding the widget.
        assert!(
            matches!(GpuProbe::probe(), GpuProbe::IoAccel),
            "probe must pick IoAccel on macOS when the counter parses"
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
}
