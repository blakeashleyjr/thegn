//! GPU and accelerator identity discovered from Linux sysfs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    Dgpu,
    Igpu,
    Apu,
    Virtual,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpVersion {
    pub major: u32,
    pub minor: u32,
    pub revision: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    Active,
    Suspended,
    Suspending,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    pub slot: String,
    pub card: u32,
    pub driver: Option<String>,
    pub pci_id: Option<String>,
    pub kind: GpuKind,
    pub family: Option<String>,
    pub ip_version: Option<IpVersion>,
    pub runtime_status: RuntimeStatus,
    pub vram_total: Option<u64>,
    pub gtt_total: Option<u64>,
    pub boot_vga: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accelerator {
    pub slot: String,
    pub driver: Option<String>,
    pub name: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GpuDevicesPart {
    pub devices: Vec<GpuDevice>,
    pub accels: Vec<Accelerator>,
    pub truncated: bool,
}

impl crate::collect::frame::Part for GpuDevicesPart {
    fn clear(&mut self) {
        self.devices.clear();
        self.accels.clear();
        self.truncated = false;
    }
}
