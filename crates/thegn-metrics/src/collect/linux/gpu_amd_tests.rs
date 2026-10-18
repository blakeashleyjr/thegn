use super::{GpuAmdCollector, GpuAmdFs};
use crate::collect::model::gpu::RuntimeStatus;
use crate::collect::{
    Collector, CollectorId, TickCtx,
    env::Env,
    frame::MonitorFrame,
    health::{Absent, Health},
};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone)]
struct FakeFs {
    files: HashMap<PathBuf, Vec<u8>>,
    preads: Vec<PathBuf>,
    small_reads: Vec<PathBuf>,
}

impl FakeFs {
    fn apu(status: &[u8]) -> (Self, PathBuf) {
        let root = PathBuf::from("/sys/class/drm");
        let device = root.join("card0/device");
        let mut fs = Self {
            files: HashMap::new(),
            preads: Vec::new(),
            small_reads: Vec::new(),
        };
        fs.put(
            device.join("uevent"),
            b"PCI_SLOT_NAME=0000:05:00.0\nDRIVER=amdgpu\n",
        );
        fs.put(device.join("power/runtime_status"), status);
        fs.put(device.join("gpu_busy_percent"), b"42\n");
        fs.put(device.join("vcn_busy_percent"), b"7\n");
        fs.put(device.join("mem_info_vram_used"), b"1048576\n");
        fs.put(device.join("mem_info_vram_total"), b"8388608\n");
        fs.put(device.join("mem_info_gtt_used"), b"2097152\n");
        fs.put(device.join("mem_info_gtt_total"), b"16777216\n");
        fs.put(
            device.join("pp_dpm_sclk"),
            b"0: 600Mhz\n1: 764Mhz *\nS: 0Mhz\n",
        );
        fs.put(device.join("pp_dpm_mclk"), b"0: 400Mhz\n1: 800Mhz *\n");
        (fs, root)
    }

    fn put(&mut self, path: PathBuf, bytes: &[u8]) {
        self.files.insert(path, bytes.to_vec());
    }
}

impl GpuAmdFs for FakeFs {
    type Handle = PathBuf;

    fn list(&mut self, path: &Path) -> io::Result<Vec<PathBuf>> {
        if path == Path::new("/sys/class/drm") {
            let mut entries = self
                .files
                .keys()
                .filter(|file| file.file_name().and_then(|name| name.to_str()) == Some("uevent"))
                .filter_map(|file| file.parent()?.parent().map(Path::to_path_buf))
                .collect::<Vec<_>>();
            entries.extend([path.join("card0-DP-1"), path.join("renderD128")]);
            entries.sort();
            entries.dedup();
            Ok(entries)
        } else {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }
    }

    fn read_small(&mut self, path: &Path, buffer: &mut [u8]) -> io::Result<usize> {
        self.small_reads.push(path.to_owned());
        let bytes = self
            .files
            .get(path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let length = bytes.len().min(buffer.len());
        buffer[..length].copy_from_slice(&bytes[..length]);
        Ok(length)
    }

    fn open(&mut self, path: &Path) -> io::Result<Self::Handle> {
        self.files
            .contains_key(path)
            .then(|| path.to_owned())
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn pread(&mut self, handle: &Self::Handle, buffer: &mut [u8]) -> io::Result<usize> {
        self.preads.push(handle.clone());
        let bytes = self
            .files
            .get(handle)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let length = bytes.len().min(buffer.len());
        buffer[..length].copy_from_slice(&bytes[..length]);
        Ok(length)
    }
}

#[test]
fn apu_sampling_stays_under_ten_preads_and_reads_totals_only_at_discovery() {
    let (fs, root) = FakeFs::apu(b"active\n");
    let mut collector = GpuAmdCollector::with_fs(fs, root);
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let total_reads = collector
        .fs
        .small_reads
        .iter()
        .filter(|path| {
            matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("mem_info_vram_total" | "mem_info_gtt_total")
            )
        })
        .count();
    assert_eq!(total_reads, 2);

    let mut frame = MonitorFrame::default();
    assert!(
        collector
            .sample(&TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .is_ok()
    );
    assert!(
        collector.fs.preads.len() <= 10,
        "observed {} preads",
        collector.fs.preads.len()
    );
    let row = &frame.parts.gpu_engines.devices[0];
    assert_eq!(row.slot.as_str(), "0000:05:00.0");
    assert_eq!(row.gfx_busy_pct, Some(42));
    assert_eq!(row.vcn_busy_pct, Some(7));
    assert_eq!(row.vram_used, Some(1_048_576));
    assert_eq!(row.vram_total, Some(8_388_608));
    assert_eq!(row.gtt_used, Some(2_097_152));
    assert_eq!(row.gtt_total, Some(16_777_216));
    assert_eq!(row.sclk.current_mhz, Some(764));
    assert_eq!(row.mclk.current_mhz, Some(800));
    assert!(
        collector
            .fs
            .small_reads
            .iter()
            .filter(|path| {
                matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("mem_info_vram_total" | "mem_info_gtt_total")
                )
            })
            .count()
            == total_reads
    );

    collector.fs.preads.clear();
    assert!(
        collector
            .sample(&TickCtx { tick: 1, now_ms: 1 }, &mut frame)
            .is_ok()
    );
    assert_eq!(collector.fs.preads.len(), 5);
}

#[test]
fn suspended_card_reads_only_runtime_status_and_deduplicates_pci_slot() {
    let (mut fs, root) = FakeFs::apu(b"suspended\n");
    let second = root.join("card1/device");
    fs.put(
        second.join("uevent"),
        b"PCI_SLOT_NAME=0000:05:00.0\nDRIVER=amdgpu\n",
    );
    fs.put(second.join("power/runtime_status"), b"suspended\n");
    let mut collector = GpuAmdCollector::with_fs(fs, root);
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    let mut frame = MonitorFrame::default();
    assert!(
        collector
            .sample(&TickCtx { tick: 0, now_ms: 0 }, &mut frame)
            .is_ok()
    );
    assert_eq!(frame.parts.gpu_engines.devices.len(), 1);
    assert_eq!(
        frame.parts.gpu_engines.devices[0].runtime_status,
        RuntimeStatus::Suspended
    );
    assert_eq!(collector.fs.preads.len(), 1);
    assert!(collector.fs.preads[0].ends_with("power/runtime_status"));
}

#[test]
fn no_amdgpu_is_absent_no_hardware_and_collector_id_is_registered() {
    let mut fs = FakeFs::apu(b"active\n");
    fs.put(
        PathBuf::from("/sys/class/drm/card0/device/uevent"),
        b"PCI_SLOT_NAME=0000:05:00.0\nDRIVER=i915\n",
    );
    let mut collector = GpuAmdCollector::with_fs(fs, "/sys/class/drm");
    assert_eq!(collector.id(), CollectorId::GpuEngines);
    assert_eq!(
        collector.discover(&Env::default()),
        Health::Absent(Absent::NoHardware)
    );
}

#[test]
fn removed_attribute_marks_the_only_device_gone() {
    let (fs, root) = FakeFs::apu(b"active\n");
    let mut collector = GpuAmdCollector::with_fs(fs, root);
    assert_eq!(collector.discover(&Env::default()), Health::Warming);
    collector.fs.files.remove(&PathBuf::from(
        "/sys/class/drm/card0/device/mem_info_vram_used",
    ));
    let mut frame = MonitorFrame::default();
    assert_eq!(
        collector.sample(&TickCtx { tick: 1, now_ms: 1 }, &mut frame),
        Err(crate::collect::CollectError::Gone)
    );
}
