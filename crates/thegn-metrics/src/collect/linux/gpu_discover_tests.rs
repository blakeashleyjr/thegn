use super::*;
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, PermissionsExt};
use tempfile::Builder;

struct Opened {
    path: PathBuf,
    file: File,
}

#[derive(Default)]
struct FakeFs {
    opens: Vec<PathBuf>,
    preads: Vec<PathBuf>,
}

impl GpuFs for FakeFs {
    type Handle = Opened;

    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes = fs::read(path)?;
        let length = bytes.len().min(buffer.len());
        buffer[..length].copy_from_slice(&bytes[..length]);
        Ok(length)
    }

    fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
        self.opens.push(path.to_owned());
        Ok(Opened {
            path: path.to_owned(),
            file: File::open(path)?,
        })
    }

    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        self.preads.push(handle.path.clone());
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
    put(&device.join("boot_vga"), "0\n");
    for connector in ["card1-DP-1", "card1-eDP-1", "card1-Writeback-1"] {
        fs::create_dir_all(drm.join(connector)).unwrap();
    }
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
    let opens_after_discovery = collector.fs.opens.clone();
    collector.fs.opens.clear();
    let mut frame = MonitorFrame::default();
    collector.sample(&TickCtx::default(), &mut frame).unwrap();
    assert!(collector.fs.opens.is_empty());
    assert_eq!(collector.fs.preads, vec![status]);
    assert_eq!(collector.fs.opens, Vec::<PathBuf>::new());
    assert_eq!(opens_after_discovery.len(), 1);
    assert_eq!(
        frame.parts.gpu_devices.devices[0].runtime_status,
        RuntimeStatus::Suspended
    );
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
