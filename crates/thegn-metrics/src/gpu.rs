//! GPU utilization — the one metric sysinfo does not provide. Linux exposes it
//! via sysfs (`amdgpu`/`i915`: `gpu_busy_percent`) or `nvidia-smi`. On other
//! platforms the sysfs path is absent and `nvidia-smi` is typically missing, so
//! [`GpuProbe::probe`] resolves to [`GpuProbe::None`] and the widget hides —
//! the same behaviour as a Linux box with no detectable GPU.

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
    None,
}

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
            GpuProbe::NvidiaSmi
        } else {
            GpuProbe::None
        }
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
            GpuProbe::None => GpuReading::default(),
        }
    }
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
