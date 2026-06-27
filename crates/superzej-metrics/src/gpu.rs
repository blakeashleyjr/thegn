//! GPU utilization — the one metric sysinfo does not provide. Linux exposes it
//! via sysfs (`amdgpu`/`i915`: `gpu_busy_percent`) or `nvidia-smi`. On other
//! platforms the sysfs path is absent and `nvidia-smi` is typically missing, so
//! [`GpuProbe::probe`] resolves to [`GpuProbe::None`] and the widget hides —
//! the same behaviour as a Linux box with no detectable GPU.

/// How GPU utilization is read (probed once at startup).
pub(crate) enum GpuProbe {
    /// amdgpu/i915 expose a percent file in sysfs.
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

    /// Read current utilization 0–100, or `None` when unavailable. The sysfs
    /// path is a cheap file read; the NVIDIA path spawns `nvidia-smi`
    /// (ticker-thread only).
    pub(crate) fn read(&self) -> Option<u8> {
        match self {
            GpuProbe::Sysfs(path) => std::fs::read_to_string(path)
                .ok()
                .and_then(|v| v.trim().parse::<u8>().ok()),
            GpuProbe::NvidiaSmi => std::process::Command::new("nvidia-smi")
                .args([
                    "--query-gpu=utilization.gpu",
                    "--format=csv,noheader,nounits",
                ])
                .output()
                .ok()
                .and_then(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .next()
                        .and_then(|l| l.trim().parse::<u8>().ok())
                }),
            GpuProbe::None => None,
        }
    }
}
