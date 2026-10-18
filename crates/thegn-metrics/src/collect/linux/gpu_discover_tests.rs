use super::*;
use crate::collect::model::gpu::GpuKind;
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, PermissionsExt};
use tempfile::Builder;

struct Opened {
    path: PathBuf,
    file: File,
}

#[derive(Default)]
struct FakeFs {
    touched: Vec<PathBuf>,
}

impl GpuFs for FakeFs {
    type Handle = Opened;

    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        self.touched.push(path.to_owned());
        fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
        self.touched.push(path.to_owned());
        let bytes = fs::read(path)?;
        let length = bytes.len().min(buffer.len());
        buffer[..length].copy_from_slice(&bytes[..length]);
        Ok(length)
    }

    fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
        self.touched.push(path.to_owned());
        Ok(Opened {
            path: path.to_owned(),
            file: File::open(path)?,
        })
    }

    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        self.touched.push(handle.path.clone());
        handle.file.read_at(buffer, 0)
    }
}

fn fixture() -> (tempfile::TempDir, FakeFs, PathBuf, PathBuf) {
    let root = Builder::new().prefix("gpu-discover-").tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let drm = root.path().join("drm");
    let accel = root.path().join("accel");
    fs::create_dir_all(&drm).unwrap();
    fs::create_dir_all(&accel).unwrap();
    (root, FakeFs::default(), drm, accel)
}

fn put(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn add_card(drm: &Path, name: &str, slot: &str, driver: &str) {
    let card = drm.join(name).join("device");
    put(
        &card.join("uevent"),
        &format!("DRIVER={driver}\nPCI_SLOT_NAME={slot}\n"),
    );
    put(&card.join("power/runtime_status"), "active\n");
}

#[test]
fn discovers_the_captured_box_gpu_and_accel_and_skips_connectors() {
    let (_root, fs, drm, accel) = fixture();
    add_card(&drm, "card1", "0000:c1:00.0", "amdgpu");
    // Captured from this box's /sys/class/drm/card1 metadata on 2026-10-18.
    let device = drm.join("card1/device");
    put(
        &device.join("uevent"),
        "DRIVER=amdgpu\nPCI_ID=1002:150E\nPCI_SLOT_NAME=0000:c1:00.0\n",
    );
    put(&device.join("ip_discovery/die/0/GC/0/major"), "11\n");
    put(&device.join("ip_discovery/die/0/GC/0/minor"), "5\n");
    put(&device.join("ip_discovery/die/0/GC/0/revision"), "0\n");
    put(&device.join("mem_info_vram_total"), "536870912\n");
    put(&device.join("mem_info_gtt_total"), "50244972544\n");
    for connector in ["card1-DP-1", "card1-eDP-1", "card1-Writeback-1"] {
        fs::create_dir_all(drm.join(connector)).unwrap();
    }
    fs::create_dir_all(drm.join("renderD128")).unwrap();
    fs::create_dir_all(device.join("uma")).unwrap();
    let accel_uevent = accel.join("accel0/device/uevent");
    put(
        &accel_uevent,
        "DRIVER=amdxdna\nPCI_ID=1022:17F0\nPCI_SLOT_NAME=0000:c2:00.1\n",
    );
    let mut collector = GpuDevicesCollector::with_fs(fs, &drm, &accel);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert_eq!(frame.parts.gpu_devices.devices.len(), 1);
    let gpu = &frame.parts.gpu_devices.devices[0];
    assert_eq!(gpu.slot, "0000:c1:00.0");
    assert_eq!(gpu.kind, GpuKind::Apu);
    assert_eq!(gpu.family.as_deref(), Some("RDNA3.5"));
    assert_eq!(gpu.boot_vga, None);
    assert_eq!(
        gpu.ip_version,
        Some(IpVersion {
            major: 11,
            minor: 5,
            revision: 0
        })
    );
    assert_eq!(frame.parts.gpu_devices.accels.len(), 1);
    assert_eq!(frame.parts.gpu_devices.accels[0].slot, "0000:c2:00.1");
    assert_eq!(
        frame.parts.gpu_devices.accels[0].driver.as_deref(),
        Some("amdxdna")
    );
}

#[test]
fn runtime_sampling_only_reads_the_held_power_status_handle() {
    let (_root, fake, drm, accel) = fixture();
    add_card(&drm, "card0", "0000:c1:00.0", "nvidia");
    let status = drm.join("card0/device/power/runtime_status");
    fs::write(&status, "suspended\n").unwrap();
    put(
        &accel.join("accel0/device/uevent"),
        "DRIVER=amdxdna\nPCI_SLOT_NAME=0000:c2:00.1\n",
    );
    let mut collector = GpuDevicesCollector::with_fs(fake, &drm, &accel);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    assert!(
        collector
            .fs
            .touched
            .iter()
            .all(|path| { !path.starts_with(drm.join("card0/device")) || path == &status })
    );
    let touched_after_discovery = collector.fs.touched.len();
    let mut frame = MonitorFrame::default();
    for (sample, now_ms) in [
        0, 5_000, 10_000, 15_000, 20_000, 25_000, 30_000, 35_000, 40_000, 45_000, 50_000, 55_000,
        60_001,
    ]
    .into_iter()
    .enumerate()
    {
        collector
            .sample(
                &TickCtx {
                    tick: sample as u64,
                    now_ms,
                },
                &mut frame,
            )
            .unwrap();
    }
    let touched = &collector.fs.touched[touched_after_discovery..];
    assert!(
        touched
            .iter()
            .all(|path| { !path.starts_with(drm.join("card0/device")) || path == &status })
    );
    assert_eq!(touched.iter().filter(|path| *path == &status).count(), 13);
    assert_eq!(
        frame.parts.gpu_devices.devices[0].runtime_status,
        RuntimeStatus::Suspended
    );
}

#[test]
fn suspended_device_identity_is_filled_after_it_becomes_active() {
    let (_root, fake, drm, accel) = fixture();
    add_card(&drm, "card0", "0000:c1:00.0", "amdgpu");
    let device = drm.join("card0/device");
    let status = device.join("power/runtime_status");
    fs::write(&status, "suspended\n").unwrap();
    put(&device.join("mem_info_vram_total"), "2147483648\n");
    put(&device.join("mem_info_gtt_total"), "8589934592\n");
    let mut collector = GpuDevicesCollector::with_fs(fake, &drm, &accel);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert_eq!(frame.parts.gpu_devices.devices[0].driver, None);
    assert_eq!(frame.parts.gpu_devices.devices[0].kind, GpuKind::Unknown);

    fs::write(&status, "active\n").unwrap();
    collector
        .sample(
            &TickCtx {
                tick: 1,
                now_ms: 500,
            },
            &mut frame,
        )
        .unwrap();
    let gpu = &frame.parts.gpu_devices.devices[0];
    assert_eq!(gpu.slot, "0000:c1:00.0");
    assert_eq!(gpu.driver.as_deref(), Some("amdgpu"));
    assert_eq!(gpu.kind, GpuKind::Apu);
}

#[test]
fn unsupported_runtime_power_still_discovers_identity() {
    let (_root, fake, drm, accel) = fixture();
    add_card(&drm, "card0", "0000:c1:00.0", "amdgpu");
    let device = drm.join("card0/device");
    fs::write(device.join("power/runtime_status"), "unsupported\n").unwrap();
    put(&device.join("mem_info_vram_total"), "2147483648\n");
    put(&device.join("mem_info_gtt_total"), "8589934592\n");
    let mut collector = GpuDevicesCollector::with_fs(fake, &drm, &accel);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();

    assert_eq!(frame.parts.gpu_devices.devices.len(), 1);
    let gpu = &frame.parts.gpu_devices.devices[0];
    assert_eq!(gpu.slot, "0000:c1:00.0");
    assert_eq!(gpu.driver.as_deref(), Some("amdgpu"));
    assert_eq!(gpu.kind, GpuKind::Apu);
    assert_eq!(gpu.runtime_status, RuntimeStatus::Unsupported);
}

#[test]
fn active_card_without_pci_slot_is_excluded_and_not_reinitialized() {
    let (_root, fake, drm, accel) = fixture();
    let device = drm.join("card0/device");
    put(&device.join("uevent"), "DRIVER=panfrost\n");
    put(&device.join("power/runtime_status"), "active\n");
    let status = device.join("power/runtime_status");
    let mut collector = GpuDevicesCollector::with_fs(fake, &drm, &accel);
    assert_eq!(
        collector.discover(&Env::default()),
        Health::Absent(Absent::NoHardware)
    );
    let touched_after_discovery = collector.fs.touched.len();
    let mut frame = MonitorFrame::default();
    for tick in 0..10 {
        collector
            .sample(
                &TickCtx {
                    tick,
                    now_ms: tick * 1_000,
                },
                &mut frame,
            )
            .unwrap();
    }

    assert!(frame.parts.gpu_devices.devices.is_empty());
    let touched = &collector.fs.touched[touched_after_discovery..];
    assert_eq!(touched.len(), 10);
    assert!(touched.iter().all(|path| path == &status));
}

#[test]
fn deferred_initialization_deduplicates_against_already_known_slots() {
    let (_root, fake, drm, accel) = fixture();
    add_card(&drm, "card0", "0000:c1:00.0", "amdgpu");
    add_card(&drm, "card1", "0000:c1:00.0", "amdgpu");
    let delayed_status = drm.join("card0/device/power/runtime_status");
    fs::write(&delayed_status, "suspended\n").unwrap();
    let mut collector = GpuDevicesCollector::with_fs(fake, &drm, &accel);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);

    fs::write(&delayed_status, "active\n").unwrap();
    let mut frame = MonitorFrame::default();
    collector
        .sample(
            &TickCtx {
                tick: 1,
                now_ms: 500,
            },
            &mut frame,
        )
        .unwrap();

    assert_eq!(frame.parts.gpu_devices.devices.len(), 1);
    assert_eq!(frame.parts.gpu_devices.devices[0].card, 1);
}

#[test]
fn apu_thresholds_classify_without_uma_directory() {
    let (_root, fake, drm, accel) = fixture();
    add_card(&drm, "card0", "0000:c1:00.0", "amdgpu");
    let device = drm.join("card0/device");
    put(&device.join("mem_info_vram_total"), "2147483648\n");
    put(&device.join("mem_info_gtt_total"), "8589934592\n");
    let mut collector = GpuDevicesCollector::with_fs(fake, &drm, &accel);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert_eq!(frame.parts.gpu_devices.devices[0].kind, GpuKind::Apu);
}

#[test]
fn deduplicates_slots_and_classifies_intel_virtual_and_unknown_gc() {
    let (_root, fake, drm, accel) = fixture();
    add_card(&drm, "card0", "0000:00:02.0", "i915");
    add_card(&drm, "card1", "0000:00:02.0", "i915");
    add_card(&drm, "card2", "0000:c2:00.1", "virtio_gpu");
    add_card(&drm, "card3", "0000:c1:00.0", "amdgpu");
    let unknown = drm.join("card3/device/ip_discovery/die/0/GC/0");
    put(&unknown.join("major"), "12\n");
    put(&unknown.join("minor"), "0\n");
    put(&unknown.join("revision"), "0\n");
    let mut collector = GpuDevicesCollector::with_fs(fake, &drm, &accel);
    assert_eq!(collector.discover(&Env::default()), Health::Ok);
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    let devices = &frame.parts.gpu_devices.devices;
    assert_eq!(devices.len(), 3);
    assert_eq!(devices[0].kind, GpuKind::Igpu);
    assert_eq!(devices[1].kind, GpuKind::Virtual);
    assert_eq!(devices[2].family, None);
}
