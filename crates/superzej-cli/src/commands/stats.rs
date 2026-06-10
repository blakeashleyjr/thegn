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
use sysinfo::{System, MINIMUM_CPU_UPDATE_INTERVAL};

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

    if let Ok(wt) = std::env::var("SUPERZEJ_WORKTREE") {
        if let Ok(db) = crate::db::Db::open() {
            if let Ok(Some(sb)) = db.worktree_sandbox(&wt) {
                let backend = match sb.as_str() {
                    "podman" => superzej_core::sandbox::Backend::Podman,
                    "docker" => superzej_core::sandbox::Backend::Docker,
                    _ => superzej_core::sandbox::Backend::None,
                };

                if backend != superzej_core::sandbox::Backend::None {
                    let sb_stats =
                        superzej_core::sandbox::stats(&superzej_core::sandbox::SandboxSpec {
                            backend,
                            transport: superzej_core::sandbox::Transport::Local,
                            image: None,
                            worktree: std::path::PathBuf::from(&wt),
                            mounts: vec![],
                            env: vec![],
                            network: superzej_core::config::Network::Nat,
                            ports: vec![],
                            gpu: None,
                            limits: superzej_core::sandbox::SandboxLimits::default(),
                            volumes: vec![],
                            compose: None,
                            init_script: None,
                            devenv: false,
                            name: superzej_core::sandbox::container_name(&wt),
                        });

                    if let Some(st) = sb_stats {
                        fields.push(format!("sb_cpu={}", st.cpu.trim_end_matches('%')));
                        fields.push(format!("sb_mem={}", st.mem));
                    }
                }
            }
        }
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
