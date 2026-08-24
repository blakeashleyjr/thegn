//! Temperature sensors on Apple silicon — the one metric `sysinfo` cannot see
//! there.
//!
//! `sysinfo::Components` reads SMC keys that exist on Intel Macs and are gone on
//! M-series, so it returns an empty list and the temperature row silently hides.
//! `ioreg` does not rescue it either: the sensors *are* in the registry
//! (`AppleARMPMUTempSensor`, `AppleEmbeddedNVMeTemperatureSensor`), but they are
//! `IOHIDEventService` nodes that publish **no temperature property** — verified
//! by dumping their properties on macOS 26. The reading only exists as a HID
//! *event*, which means opening the service.
//!
//! So this is the `IOHIDEventSystemClient` route: match every service on the
//! Apple-vendor temperature usage page, copy a temperature event from each, and
//! read its float. Unentitled, in-process, no subprocess, no root — the same
//! bar `gpu.rs` set when it rejected `powermetrics`.
//!
//! **`IOHIDEventSystemClientCreate` is not in a public header**, so every symbol
//! is resolved with `dlsym` at probe time rather than linked. That is the whole
//! safety story: if a future macOS removes or renames them, this reports "no
//! sensors" and the widget hides, exactly as today — whereas link-time binding
//! would produce a binary that will not launch at all. A private symbol is worth
//! using only if its absence is survivable.
//!
//! Structure mirrors [`crate::gpu::GpuProbe`]: probe once, verify the backend
//! actually yields a value before selecting it, and charge sampling to the slow
//! tier.

/// How temperatures are read (probed once at startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThermalProbe {
    /// macOS: Apple-vendor HID temperature sensors via `IOHIDEventSystemClient`.
    ///
    /// Only ever CONSTRUCTED under `cfg(macos)` (see `probe`), though it is
    /// matched and compared everywhere so the enum stays one shape across
    /// platforms — hence the expectation rather than a cfg on the variant.
    #[cfg_attr(not(any(test, target_os = "macos")), expect(dead_code))]
    AppleHid,
    /// sysinfo's `Components` — correct on Linux and on Intel Macs. Also the
    /// fallback when nothing is readable: it simply yields an empty list and the
    /// widget hides, which is the same outcome a dedicated `None` variant would
    /// produce with an extra arm to keep in sync.
    Components,
}

impl ThermalProbe {
    pub(crate) fn probe() -> ThermalProbe {
        // Only select the HID backend if it actually produced a reading — a Mac
        // (or a future macOS) where the symbols or the sensors are gone must
        // fall through rather than select a backend that always yields nothing.
        // Same rule as `GpuProbe::probe`'s `read_ioaccel().is_some()` check.
        #[cfg(target_os = "macos")]
        if !apple_hid::read().is_empty() {
            return ThermalProbe::AppleHid;
        }
        ThermalProbe::Components
    }

    /// `(label, °C)` pairs. Empty when nothing is readable.
    pub(crate) fn read(&self) -> Vec<(String, f32)> {
        match self {
            #[cfg(target_os = "macos")]
            ThermalProbe::AppleHid => apple_hid::read(),
            #[cfg(not(target_os = "macos"))]
            ThermalProbe::AppleHid => Vec::new(),
            ThermalProbe::Components => Vec::new(),
        }
    }
}

/// Plausibility filter for a sensor reading, shared by every backend.
///
/// Kept `pub(crate)` and platform-independent so it stays compiled — and
/// therefore tested — on Linux CI, the same trick `gpu.rs` uses for its parser.
/// Apple's HID sensors include voltage/current channels on the same usage page
/// that report 0, and a disconnected sensor reports garbage; neither should
/// render as a temperature.
#[cfg_attr(not(any(test, target_os = "macos")), expect(dead_code))]
pub(crate) fn plausible_celsius(v: f32) -> bool {
    v.is_finite() && v > 1.0 && v < 150.0
}

#[cfg(target_os = "macos")]
mod apple_hid {
    //! The `IOHIDEventSystemClient` bridge. Every symbol is `dlsym`'d; see the
    //! module doc for why.

    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;

    /// `kHIDPage_AppleVendor`. The page Apple's own sensors live on.
    const PAGE_APPLE_VENDOR: i32 = 0xff00;
    /// `kHIDUsage_AppleVendor_TemperatureSensor`.
    const USAGE_TEMPERATURE_SENSOR: i32 = 0x0005;
    /// `kIOHIDEventTypeTemperature`.
    const EVENT_TYPE_TEMPERATURE: i64 = 15;
    /// `kIOHIDEventFieldTemperatureLevel` = `IOHIDEventFieldBase(kIOHIDEventTypeTemperature)`,
    /// i.e. the event type shifted into the high half and **nothing added**.
    ///
    /// The `| 1` that a field-index-style selector would suggest is wrong here,
    /// and wrong *silently*: `IOHIDEventGetFloatValue` returns `0` for an
    /// unknown field rather than failing, so every sensor read as 0°C, was
    /// rejected as implausible, and the backend deselected itself. Verified
    /// on-device — with this selector the same sensors report 37.9 / 51.8 / 30.0.
    const FIELD_TEMPERATURE_LEVEL: u32 = (EVENT_TYPE_TEMPERATURE as u32) << 16;

    type ClientCreate = unsafe extern "C" fn(*const c_void) -> CFTypeRef;
    type ClientSetMatching = unsafe extern "C" fn(CFTypeRef, CFDictionaryRef);
    type ClientCopyServices = unsafe extern "C" fn(CFTypeRef) -> CFArrayRef;
    type ServiceCopyProperty = unsafe extern "C" fn(CFTypeRef, CFStringRef) -> CFTypeRef;
    type ServiceCopyEvent = unsafe extern "C" fn(CFTypeRef, i64, i32, i64) -> CFTypeRef;
    type EventGetFloatValue = unsafe extern "C" fn(CFTypeRef, u32) -> f64;

    struct Syms {
        create: ClientCreate,
        set_matching: ClientSetMatching,
        copy_services: ClientCopyServices,
        copy_property: ServiceCopyProperty,
        copy_event: ServiceCopyEvent,
        float_value: EventGetFloatValue,
    }

    /// Resolve the private IOKit entry points once. `None` on any macOS that no
    /// longer exports them — which is a supported outcome, not an error.
    fn syms() -> Option<&'static Syms> {
        static SYMS: std::sync::OnceLock<Option<Syms>> = std::sync::OnceLock::new();
        SYMS.get_or_init(|| {
            // SAFETY: `dlopen` of a system framework by absolute path, then
            // `dlsym` of NUL-terminated names. Every result is null-checked
            // before being transmuted to a fn pointer with the signature Apple
            // documents for it; a null means "not available" and we bail.
            unsafe {
                let h = libc::dlopen(
                    c"/System/Library/Frameworks/IOKit.framework/IOKit".as_ptr(),
                    libc::RTLD_LAZY,
                );
                if h.is_null() {
                    return None;
                }
                let get = |name: &std::ffi::CStr| {
                    let p = libc::dlsym(h, name.as_ptr());
                    (!p.is_null()).then_some(p)
                };
                Some(Syms {
                    create: std::mem::transmute::<*mut c_void, ClientCreate>(get(
                        c"IOHIDEventSystemClientCreate",
                    )?),
                    set_matching: std::mem::transmute::<*mut c_void, ClientSetMatching>(get(
                        c"IOHIDEventSystemClientSetMatching",
                    )?),
                    copy_services: std::mem::transmute::<*mut c_void, ClientCopyServices>(get(
                        c"IOHIDEventSystemClientCopyServices",
                    )?),
                    copy_property: std::mem::transmute::<*mut c_void, ServiceCopyProperty>(get(
                        c"IOHIDServiceClientCopyProperty",
                    )?),
                    copy_event: std::mem::transmute::<*mut c_void, ServiceCopyEvent>(get(
                        c"IOHIDServiceClientCopyEvent",
                    )?),
                    float_value: std::mem::transmute::<*mut c_void, EventGetFloatValue>(get(
                        c"IOHIDEventGetFloatValue",
                    )?),
                })
            }
        })
        .as_ref()
    }

    /// The client + matched services + their names, built once per thread.
    ///
    /// Creating the client, installing the matching dictionary and copying the
    /// service list never has to be repeated — the sensor set is fixed hardware.
    /// Thread-local rather than a global because CoreFoundation refs are not
    /// `Send`; the sampler is single-threaded, so in practice this is built once.
    struct Sensors {
        client: CFTypeRef,
        services: Vec<(CFTypeRef, String)>,
    }

    impl Drop for Sensors {
        fn drop(&mut self) {
            // SAFETY: `client` came from a Create-rule call and is released
            // exactly once here. The service refs are owned by the set the
            // client holds, so they are not released individually.
            unsafe {
                if !self.client.is_null() {
                    CFRelease(self.client);
                }
            }
        }
    }

    thread_local! {
        static SENSORS: Option<Sensors> = build_sensors();
    }

    /// One-time discovery: client, matching, services, and each sensor's name.
    fn build_sensors() -> Option<Sensors> {
        let s = syms()?;
        // SAFETY: CoreFoundation Copy-rule calls; every ref is null-checked, and
        // the client is retained in `Sensors` (released in its `Drop`). The
        // service array is kept alive by the client, which is why the raw
        // service refs stay valid for as long as `Sensors` lives.
        unsafe {
            let client = (s.create)(std::ptr::null());
            if client.is_null() {
                return None;
            }
            let matching = CFDictionary::from_CFType_pairs(&[
                (
                    CFString::new("PrimaryUsagePage").as_CFType(),
                    CFNumber::from(PAGE_APPLE_VENDOR).as_CFType(),
                ),
                (
                    CFString::new("PrimaryUsage").as_CFType(),
                    CFNumber::from(USAGE_TEMPERATURE_SENSOR).as_CFType(),
                ),
            ]);
            (s.set_matching)(client, matching.as_concrete_TypeRef());

            let services = (s.copy_services)(client);
            if services.is_null() {
                CFRelease(client);
                return None;
            }
            let list: CFArray<*const c_void> = CFArray::wrap_under_create_rule(services);
            let mut out = Vec::with_capacity(list.len() as usize);
            for i in 0..list.len() {
                let Some(svc) = list.get(i) else { continue };
                let svc = *svc as CFTypeRef;
                if svc.is_null() {
                    continue;
                }
                // "Product" is the human sensor name ("PMU tdie1", "NAND CH0
                // temp") and never changes, so it is read once here rather than
                // on every sample.
                let name_ref =
                    (s.copy_property)(svc, CFString::new("Product").as_concrete_TypeRef());
                let label = if name_ref.is_null() {
                    format!("sensor {i}")
                } else {
                    CFString::wrap_under_create_rule(name_ref as CFStringRef).to_string()
                };
                out.push((svc, label));
            }
            Some(Sensors {
                client,
                services: curate(out),
            })
        }
    }

    /// Narrow the discovered services to the ones worth polling, deduplicated
    /// by name.
    ///
    /// This machine reports **77 services under 17 distinct names** — mostly
    /// `PMU tdie1..14` repeated. Polling all of them costs ~80ms (each
    /// `IOHIDServiceClientCopyEvent` is ~1ms) on a slow tier that runs every
    /// ~10s: 0.8% of a core, forever, for a temperature readout — against a
    /// measured idle of ~0.06 cores that would be a 14% regression. And 52
    /// near-identical rows is not a readout a person can use.
    ///
    /// Kept: `tdie` (the die sensors — what a CPU temperature actually is),
    /// anything ending in `temp` (NAND/SSD), and `battery`. Dropped: `PMU tcal`,
    /// a calibration reference that reads ~15C above the die and is not a
    /// temperature of anything.
    ///
    /// If the filter matches nothing — a Mac whose sensors are named
    /// differently — everything is kept (capped), so an unfamiliar machine
    /// degrades to "noisy but present" rather than "no thermals".
    fn curate(all: Vec<(CFTypeRef, String)>) -> Vec<(CFTypeRef, String)> {
        /// Enough for every die on an M-series plus the storage/battery
        /// sensors, and a hard bound on the per-sample cost.
        const MAX_SENSORS: usize = 24;
        let useful = |name: &str| {
            let n = name.to_ascii_lowercase();
            n.contains("tdie") || n.ends_with("temp") || n.contains("battery")
        };
        let mut seen = std::collections::HashSet::new();
        let mut keep: Vec<(CFTypeRef, String)> = all
            .iter()
            .filter(|(_, n)| useful(n))
            .filter(|(_, n)| seen.insert(n.clone()))
            .cloned()
            .collect();
        if keep.is_empty() {
            let mut seen = std::collections::HashSet::new();
            keep = all
                .into_iter()
                .filter(|(_, n)| seen.insert(n.clone()))
                .collect();
        }
        keep.truncate(MAX_SENSORS);
        keep
    }

    /// Every readable Apple-vendor temperature sensor, as `(product name, °C)`.
    pub(super) fn read() -> Vec<(String, f32)> {
        let Some(s) = syms() else {
            return Vec::new();
        };
        SENSORS.with(|cached| {
            let Some(sensors) = cached.as_ref() else {
                return Vec::new();
            };
            let mut out = Vec::with_capacity(sensors.services.len());
            for (svc, label) in &sensors.services {
                // SAFETY: `svc` is a service ref owned by the live client in
                // `sensors`; the event is a Copy-rule ref released right after
                // its value is read.
                let celsius = unsafe {
                    let event = (s.copy_event)(*svc, EVENT_TYPE_TEMPERATURE, 0, 0);
                    if event.is_null() {
                        continue;
                    }
                    let v = (s.float_value)(event, FIELD_TEMPERATURE_LEVEL) as f32;
                    CFRelease(event);
                    v
                };
                // Apple publishes unpopulated channels on this page that read
                // ~-9201; they are not temperatures.
                if !super::plausible_celsius(celsius) {
                    continue;
                }
                out.push((label.clone(), celsius));
            }
            out
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implausible_readings_are_rejected() {
        // Apple publishes voltage/current channels on the same usage page that
        // read 0, and a detached sensor reports garbage — neither is a
        // temperature, and rendering one is worse than rendering nothing.
        assert!(!plausible_celsius(0.0));
        assert!(!plausible_celsius(-40.0));
        assert!(!plausible_celsius(f32::NAN));
        assert!(!plausible_celsius(f32::INFINITY));
        assert!(!plausible_celsius(1000.0));
        // Real readings, idle through thermal-throttle.
        for v in [20.0, 35.5, 55.0, 95.0, 105.0] {
            assert!(plausible_celsius(v), "{v}");
        }
    }

    #[test]
    fn probe_selects_a_backend_that_actually_answers() {
        // The contract, on every platform: probing must not panic, and a
        // selected backend must be one `read()` can service. `AppleHid` is only
        // ever selected after a non-empty read (see `probe`).
        let p = ThermalProbe::probe();
        if p == ThermalProbe::AppleHid {
            assert!(
                !p.read().is_empty(),
                "AppleHid was selected, so it must yield readings"
            );
        }
    }
}
