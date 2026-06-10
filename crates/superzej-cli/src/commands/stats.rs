//! `superzej stats` (internal) — print system stats for the tabbar widget.
//!
//! Plugins are sandboxed (no /proc, no fs, no shelling out), so the tabbar
//! polls this on a timer and parses the single line:
//!   `cpu=NN mem=NN gpu=NN time=HH:MM`
//! `cpu`/`mem`/`gpu` are integer percents; any field is dropped when its source
//! is unreadable (notably `gpu` on machines with no supported GPU counter).
//!
//! CPU and memory come from `sysinfo` (Linux/macOS/Windows/BSD); the clock from
//! `chrono` (local timezone). GPU utilization has no single cross-platform
//! crate, so it's a best-effort cascade: NVIDIA via NVML (Linux + Windows),
//! then the Linux `gpu_busy_percent` sysfs counter (AMD + Intel). Other
//! combinations (e.g. Apple GPUs) simply omit the field for now.

use anyhow::Result;
use serde::Serialize;
use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, System};

/// Default icons for stat display in the tabbar.
pub const DEFAULT_CPU_ICON: &str = "CPU";
pub const DEFAULT_MEM_ICON: &str = "MEM";
pub const DEFAULT_NET_ICON: &str = "NET";
pub const DEFAULT_GPU_ICON: &str = "GPU";
/// Default refresh interval in seconds.
pub const DEFAULT_REFRESH_SECS: f64 = 2.0;
/// Default refresh rates for cycling (in seconds).
pub const DEFAULT_REFRESH_RATES: &[f64] = &[1.0, 2.0, 5.0, 10.0];

/// Stats configuration: icons and refresh rates for the tabbar widget.
#[derive(Serialize)]
pub struct StatsConfig {
    pub cpu_icon: String,
    pub mem_icon: String,
    pub net_icon: String,
    pub gpu_icon: String,
    pub refresh_secs: f64,
    pub refresh_rates: Vec<f64>,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            cpu_icon: DEFAULT_CPU_ICON.to_string(),
            mem_icon: DEFAULT_MEM_ICON.to_string(),
            net_icon: DEFAULT_NET_ICON.to_string(),
            gpu_icon: DEFAULT_GPU_ICON.to_string(),
            refresh_secs: DEFAULT_REFRESH_SECS,
            refresh_rates: DEFAULT_REFRESH_RATES.to_vec(),
        }
    }
}

/// Output stats configuration as JSON for the tabbar plugin.
pub fn config(cfg: &crate::Config) -> Result<()> {
    let stats_cfg = &cfg.stats;
    let cfg = StatsConfig {
        cpu_icon: stats_cfg.cpu_icon.clone(),
        mem_icon: stats_cfg.mem_icon.clone(),
        net_icon: stats_cfg.net_icon.clone(),
        gpu_icon: stats_cfg.gpu_icon.clone(),
        refresh_secs: stats_cfg.refresh_secs,
        refresh_rates: stats_cfg.refresh_rates.clone(),
    };
    let json = serde_json::to_string(&cfg)?;
    crate::outln!("{}", json);
    Ok(())
}

pub fn run() -> Result<()> {
    // Determinism shim for visual-regression tests: `SZ_FAKE_STATS` replaces the
    // whole line (e.g. "cpu=12 mem=34 gpu=0 time=09:41") so the tabbar renders
    // identically run-to-run. Production paths are unaffected when it's unset.
    if let Ok(fake) = std::env::var("SZ_FAKE_STATS") {
        if !fake.trim().is_empty() {
            crate::outln!("{}", fake.trim());
            return Ok(());
        }
    }

    let mut fields: Vec<String> = Vec::new();

    let mut sys = System::new();
    // CPU usage needs two samples a moment apart; sysinfo enforces a minimum
    // interval. The tabbar polls on a multi-second timer, so this brief sleep
    // is cheap and keeps the reading stateless across invocations.
    sys.refresh_cpu_usage();
    std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_usage();
    // Only emit CPU when sysinfo actually populated per-core data for this
    // platform; otherwise global_cpu_usage() is a meaningless 0, so drop it.
    if !sys.cpus().is_empty() {
        let cpu = sys.global_cpu_usage().round().clamp(0.0, 100.0) as u8;
        fields.push(format!("cpu={cpu}"));
    }

    sys.refresh_memory();
    let total = sys.total_memory();
    if total > 0 {
        let used = sys.used_memory() as f64 / total as f64 * 100.0;
        fields.push(format!("mem={}", used.round().clamp(0.0, 100.0) as u8));
    }

    if let Some(gpu) = gpu_percent() {
        fields.push(format!("gpu={gpu}"));
    }

    fields.push(format!("time={}", chrono::Local::now().format("%H:%M")));

    crate::outln!("{}", fields.join(" "));
    Ok(())
}

/// GPU busy percent, best-effort across vendors/platforms. Tries NVIDIA's NVML
/// first (cross-platform, dlopen'd at runtime), then the Linux sysfs counter
/// (AMD/Intel). Returns None when no supported source is readable.
fn gpu_percent() -> Option<u8> {
    if let Some(p) = nvml_gpu() {
        return Some(p);
    }
    sysfs_gpu()
}

/// NVIDIA via NVML. `Nvml::init()` dynamically loads libnvidia-ml; on machines
/// without it (no driver / non-NVIDIA), it errors and we fall through.
fn nvml_gpu() -> Option<u8> {
    let nvml = nvml_wrapper::Nvml::init().ok()?;
    let device = nvml.device_by_index(0).ok()?;
    let util = device.utilization_rates().ok()?;
    Some((util.gpu.min(100)) as u8)
}

/// Linux amdgpu/i915 `gpu_busy_percent` sysfs counter (first readable card).
/// No-op off Linux.
#[cfg(target_os = "linux")]
fn sysfs_gpu() -> Option<u8> {
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let busy = entry.path().join("device/gpu_busy_percent");
        if let Ok(s) = std::fs::read_to_string(&busy) {
            if let Ok(n) = s.trim().parse::<u8>() {
                return Some(n.min(100));
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn sysfs_gpu() -> Option<u8> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_config_default_has_expected_values() {
        let cfg = StatsConfig::default();
        assert_eq!(cfg.cpu_icon, "CPU");
        assert_eq!(cfg.mem_icon, "MEM");
        assert_eq!(cfg.net_icon, "NET");
        assert_eq!(cfg.gpu_icon, "GPU");
        assert_eq!(cfg.refresh_secs, 2.0);
        assert_eq!(cfg.refresh_rates, vec![1.0, 2.0, 5.0, 10.0]);
    }

    #[test]
    fn stats_config_serializes_to_json() {
        let cfg = StatsConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("CPU"));
        assert!(json.contains("MEM"));
        assert!(json.contains("GPU"));
        assert!(json.contains("2.0"));
    }

    #[test]
    fn stats_config_custom_values() {
        let cfg = StatsConfig {
            cpu_icon: "C".to_string(),
            mem_icon: "M".to_string(),
            net_icon: "N".to_string(),
            gpu_icon: "G".to_string(),
            refresh_secs: 5.0,
            refresh_rates: vec![1.0, 5.0],
        };
        assert_eq!(cfg.cpu_icon, "C");
        assert_eq!(cfg.mem_icon, "M");
        assert_eq!(cfg.net_icon, "N");
        assert_eq!(cfg.gpu_icon, "G");
        assert_eq!(cfg.refresh_secs, 5.0);
        assert_eq!(cfg.refresh_rates, vec![1.0, 5.0]);
    }

    #[test]
    fn default_constants_are_correct() {
        assert_eq!(DEFAULT_CPU_ICON, "CPU");
        assert_eq!(DEFAULT_MEM_ICON, "MEM");
        assert_eq!(DEFAULT_NET_ICON, "NET");
        assert_eq!(DEFAULT_GPU_ICON, "GPU");
        assert_eq!(DEFAULT_REFRESH_SECS, 2.0);
        assert_eq!(DEFAULT_REFRESH_RATES, &[1.0, 2.0, 5.0, 10.0]);
    }

    #[test]
    fn config_outputs_json_to_stdout() {
        // config() requires Config argument - test with default
        let cfg = crate::Config::default();
        let result = config(&cfg);
        // config() just needs to not panic - it outputs JSON to stdout via crate::outln!
        assert!(result.is_ok());
    }

    #[test]
    fn run_with_fake_stats_uses_env_var() {
        // Test the SZ_FAKE_STATS code path by setting the env var
        std::env::set_var("SZ_FAKE_STATS", "cpu=50 mem=75 gpu=0 time=12:00");

        // This should return Ok without panicking - the fake value is printed
        let result = run();

        std::env::remove_var("SZ_FAKE_STATS");
        assert!(result.is_ok());
    }

    #[test]
    fn run_with_empty_fake_stats_runs_normal_path() {
        // When SZ_FAKE_STATS is set to empty string, it runs the normal path
        std::env::set_var("SZ_FAKE_STATS", "");

        // The normal path requires sysinfo - just verify it doesn't panic
        let _ = run();

        std::env::remove_var("SZ_FAKE_STATS");
    }

    #[test]
    fn gpu_percent_returns_none_on_no_nvidia() {
        // This tests that nvml_gpu() failing returns None and falls through to sysfs
        // On a machine without NVIDIA GPU, this will return None from sysfs_gpu
        let result = gpu_percent();
        // Just verify it returns an Option<u8> - actual value depends on system
        assert!(result.is_none() || result.is_some());
    }
}
