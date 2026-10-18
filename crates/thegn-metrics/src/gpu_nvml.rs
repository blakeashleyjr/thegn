//! Optional Linux NVML binding. The library and every entry point are resolved
//! at runtime, so installations without the NVIDIA driver remain supported.

use std::ffi::{CStr, c_void};
use std::sync::mpsc;
use std::time::Duration;

use crate::gpu::GpuReading;

type Device = *mut c_void;

/// Minimal, testable surface used by the masthead. Handles are opaque integer
/// tokens so fake implementations do not need to construct vendor pointers.
pub(crate) trait NvmlApi: Send {
    fn init(&mut self) -> Result<(), ()>;
    fn device_count(&mut self) -> Result<u32, ()>;
    fn handle_by_index(&mut self, index: u32) -> Result<usize, ()>;
    fn utilization(&mut self, device: usize) -> Option<u32>;
    fn memory_info(&mut self, device: usize) -> Option<(u64, u64)>;
    fn temperature(&mut self, device: usize) -> Option<f32>;
    fn power_usage(&mut self, device: usize) -> Option<f32>;
    fn shutdown(&mut self);
}

/// Build and initialize NVML on a named custody thread. If init exceeds the
/// watchdog, that thread and its library remain owned by the detached thread;
/// the caller marks NVML attempted and never starts another init this process.
pub(crate) fn init_with_watchdog(
    mut api: Box<dyn NvmlApi>,
    timeout: Duration,
) -> Result<Box<dyn NvmlApi>, InitFailure> {
    let (tx, rx) = mpsc::sync_channel(1);
    thegn_core::thread_name::spawn_named("thegn-nvml-init", move || {
        let result = api.init();
        if tx.send((api, result)).is_err() {
            // The watchdog already gave up. The worker owns and drops the API.
        }
    });
    match rx.recv_timeout(timeout) {
        Ok((api, Ok(()))) => Ok(api),
        Ok((_api, Err(()))) => Err(InitFailure::Unavailable),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(InitFailure::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(InitFailure::Unavailable),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitFailure {
    Unavailable,
    Timeout,
}

/// Sample every visible device. The masthead reports the busiest device and
/// sums power and memory across devices where those symbols are available.
pub(crate) fn read(api: &mut dyn NvmlApi) -> Option<GpuReading> {
    let count = match api.device_count() {
        Ok(count) => count,
        Err(()) => return None,
    };
    let mut reading = GpuReading::default();
    for index in 0..count {
        let Ok(handle) = api.handle_by_index(index) else {
            continue;
        };
        if let Some(util) = api.utilization(handle) {
            let util = util.min(100) as u8;
            reading.util_pct = Some(reading.util_pct.unwrap_or(0).max(util));
        }
        if let Some((used, total)) = api.memory_info(handle)
            && total > 0
        {
            let prior = reading.mem_mib.unwrap_or((0, 0));
            reading.mem_mib = Some((prior.0.saturating_add(used), prior.1.saturating_add(total)));
        }
        if let Some(temp) = api.temperature(handle) {
            reading.temp_c = Some(reading.temp_c.unwrap_or(0.0).max(temp));
        }
        if let Some(power) = api.power_usage(handle) {
            reading.power_w = Some(reading.power_w.unwrap_or(0.0) + power);
        }
    }
    // NVML reports bytes and milliwatts; GpuReading stores MiB and watts.
    reading.mem_mib = reading
        .mem_mib
        .map(|(used, total)| (used / (1024 * 1024), total / (1024 * 1024)));
    reading.power_w = reading.power_w.map(|power| power / 1000.0);
    reading.util_pct.map(|_| reading)
}

type InitFn = unsafe extern "C" fn() -> i32;
type ShutdownFn = unsafe extern "C" fn() -> i32;
type CountFn = unsafe extern "C" fn(*mut u32) -> i32;
type HandleFn = unsafe extern "C" fn(u32, *mut Device) -> i32;
type UtilFn = unsafe extern "C" fn(Device, *mut Utilization) -> i32;
type MemoryFn = unsafe extern "C" fn(Device, *mut MemoryInfo) -> i32;
type TemperatureFn = unsafe extern "C" fn(Device, u32, *mut u32) -> i32;
type PowerFn = unsafe extern "C" fn(Device, *mut u32) -> i32;

// Layouts copied from NVIDIA's nvml.h: nvmlUtilization_t has two unsigned int
// fields (gpu, memory); nvmlMemory_t has three unsigned long long fields
// (total, free, used). Source: https://github.com/NVIDIA/gpu-monitoring-tools/
// blob/master/bindings/go/nvml/nvml.h
#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
#[derive(Default)]
struct MemoryInfo {
    total: u64,
    free: u64,
    used: u64,
}

struct DynamicNvml {
    library: *mut c_void,
    init: Option<InitFn>,
    shutdown: Option<ShutdownFn>,
    count: Option<CountFn>,
    handle: Option<HandleFn>,
    utilization: Option<UtilFn>,
    memory: Option<MemoryFn>,
    temperature: Option<TemperatureFn>,
    power: Option<PowerFn>,
    initialized: bool,
}

// SAFETY: NVML's library handle and function pointers are process-global
// references; calls are serialized by GpuMonitor's probe worker and drop occurs
// only after that worker releases the API.
unsafe impl Send for DynamicNvml {}

impl DynamicNvml {
    fn load() -> Option<Self> {
        // SAFETY: the soname is NUL terminated; a null result means absent.
        let library = unsafe {
            libc::dlopen(
                c"libnvidia-ml.so.1".as_ptr(),
                libc::RTLD_LAZY | libc::RTLD_LOCAL,
            )
        };
        if library.is_null() {
            return None;
        }
        // SAFETY: names are NUL terminated and dlsym pointers are copied into
        // function pointers with signatures from NVIDIA's published nvml.h.
        unsafe {
            let symbol = |name: &CStr| {
                let ptr = libc::dlsym(library, name.as_ptr());
                (!ptr.is_null()).then_some(ptr)
            };
            Some(Self {
                library,
                init: symbol(c"nvmlInit_v2").map(|p| std::mem::transmute::<*mut c_void, InitFn>(p)),
                shutdown: symbol(c"nvmlShutdown")
                    .map(|p| std::mem::transmute::<*mut c_void, ShutdownFn>(p)),
                count: symbol(c"nvmlDeviceGetCount_v2")
                    .map(|p| std::mem::transmute::<*mut c_void, CountFn>(p)),
                handle: symbol(c"nvmlDeviceGetHandleByIndex_v2")
                    .map(|p| std::mem::transmute::<*mut c_void, HandleFn>(p)),
                utilization: symbol(c"nvmlDeviceGetUtilizationRates")
                    .map(|p| std::mem::transmute::<*mut c_void, UtilFn>(p)),
                memory: symbol(c"nvmlDeviceGetMemoryInfo")
                    .map(|p| std::mem::transmute::<*mut c_void, MemoryFn>(p)),
                temperature: symbol(c"nvmlDeviceGetTemperature")
                    .map(|p| std::mem::transmute::<*mut c_void, TemperatureFn>(p)),
                power: symbol(c"nvmlDeviceGetPowerUsage")
                    .map(|p| std::mem::transmute::<*mut c_void, PowerFn>(p)),
                initialized: false,
            })
        }
    }
}

impl NvmlApi for DynamicNvml {
    fn init(&mut self) -> Result<(), ()> {
        // An absent init entry point makes the entire backend unavailable.
        let init = self.init.ok_or(())?;
        // SAFETY: `init` is the NVML function resolved from this live library.
        if unsafe { init() } == 0 {
            self.initialized = true;
            Ok(())
        } else {
            Err(())
        }
    }
    fn device_count(&mut self) -> Result<u32, ()> {
        let count = self.count.ok_or(())?;
        let mut value = 0;
        // SAFETY: output pointer is valid for the duration of the call.
        (unsafe { count(&mut value) } == 0)
            .then_some(value)
            .ok_or(())
    }
    fn handle_by_index(&mut self, index: u32) -> Result<usize, ()> {
        let handle = self.handle.ok_or(())?;
        let mut value = std::ptr::null_mut();
        // SAFETY: output pointer is valid and NVML writes an opaque device handle.
        (unsafe { handle(index, &mut value) } == 0 && !value.is_null())
            .then_some(value as usize)
            .ok_or(())
    }
    fn utilization(&mut self, device: usize) -> Option<u32> {
        let f = self.utilization?;
        let mut value = Utilization::default();
        // SAFETY: device came from NVML and value has nvmlUtilization_t layout.
        (unsafe { f(device as Device, &mut value) } == 0).then_some(value.gpu)
    }
    fn memory_info(&mut self, device: usize) -> Option<(u64, u64)> {
        let f = self.memory?;
        let mut value = MemoryInfo::default();
        // SAFETY: device came from NVML and value has nvmlMemory_t layout.
        (unsafe { f(device as Device, &mut value) } == 0).then_some((value.used, value.total))
    }
    fn temperature(&mut self, device: usize) -> Option<f32> {
        let f = self.temperature?;
        let mut value = 0;
        // SAFETY: NVML temperature type 0 is GPU core temperature; output is valid.
        (unsafe { f(device as Device, 0, &mut value) } == 0).then_some(value as f32)
    }
    fn power_usage(&mut self, device: usize) -> Option<f32> {
        let f = self.power?;
        let mut value = 0;
        // SAFETY: output pointer is valid; NVML returns milliwatts.
        (unsafe { f(device as Device, &mut value) } == 0).then_some(value as f32)
    }
    fn shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown {
            // SAFETY: shutdown is resolved from this library; call is best effort.
            let _status = unsafe { shutdown() };
        }
    }
}

impl Drop for DynamicNvml {
    fn drop(&mut self) {
        if self.initialized {
            self.shutdown();
        }
        // SAFETY: every handle returned by dlopen is closed exactly once.
        unsafe { libc::dlclose(self.library) };
    }
}

pub(crate) fn load() -> Option<Box<dyn NvmlApi>> {
    DynamicNvml::load().map(|api| Box::new(api) as Box<dyn NvmlApi>)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        hang: bool,
    }

    impl NvmlApi for Fake {
        fn init(&mut self) -> Result<(), ()> {
            if self.hang {
                std::thread::sleep(Duration::from_secs(2));
            }
            Ok(())
        }
        fn device_count(&mut self) -> Result<u32, ()> {
            Ok(1)
        }
        fn handle_by_index(&mut self, _: u32) -> Result<usize, ()> {
            Ok(1)
        }
        fn utilization(&mut self, _: usize) -> Option<u32> {
            Some(73)
        }
        fn memory_info(&mut self, _: usize) -> Option<(u64, u64)> {
            Some((512 * 1024 * 1024, 2 * 1024 * 1024 * 1024))
        }
        fn temperature(&mut self, _: usize) -> Option<f32> {
            Some(61.0)
        }
        fn power_usage(&mut self, _: usize) -> Option<f32> {
            Some(85_000.0)
        }
        fn shutdown(&mut self) {}
    }

    #[test]
    fn reading_maps_available_metrics_into_masthead_fields() {
        let mut fake = Fake { hang: false };
        assert_eq!(
            read(&mut fake),
            Some(GpuReading {
                util_pct: Some(73),
                mem_mib: Some((512, 2048)),
                mem_shared: false,
                temp_c: Some(61.0),
                power_w: Some(85.0),
            })
        );
    }

    #[test]
    fn missing_metric_symbols_degrade_only_their_field() {
        struct Missing;
        impl NvmlApi for Missing {
            fn init(&mut self) -> Result<(), ()> {
                Ok(())
            }
            fn device_count(&mut self) -> Result<u32, ()> {
                Ok(1)
            }
            fn handle_by_index(&mut self, _: u32) -> Result<usize, ()> {
                Ok(1)
            }
            fn utilization(&mut self, _: usize) -> Option<u32> {
                Some(12)
            }
            fn memory_info(&mut self, _: usize) -> Option<(u64, u64)> {
                None
            }
            fn temperature(&mut self, _: usize) -> Option<f32> {
                None
            }
            fn power_usage(&mut self, _: usize) -> Option<f32> {
                None
            }
            fn shutdown(&mut self) {}
        }
        let mut fake = Missing;
        let reading = read(&mut fake).expect("utilization keeps the reading live");
        assert_eq!(reading.util_pct, Some(12));
        assert_eq!(reading.mem_mib, None);
        assert_eq!(reading.temp_c, None);
        assert_eq!(reading.power_w, None);
    }

    #[test]
    fn init_watchdog_returns_without_waiting_for_a_stuck_driver() {
        let started = std::time::Instant::now();
        let result = init_with_watchdog(Box::new(Fake { hang: true }), Duration::from_millis(20));
        assert!(matches!(result, Err(InitFailure::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn live_nvml_probe_skips_when_library_or_device_is_absent() {
        let Some(api) = load() else {
            return;
        };
        let Ok(mut api) = init_with_watchdog(api, Duration::from_secs(5)) else {
            return;
        };
        if let Some(reading) = read(api.as_mut()) {
            assert!(reading.util_pct.is_some_and(|value| value <= 100));
        }
    }
}
