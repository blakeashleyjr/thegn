//! AMD GPU engine, memory, and DPM clock readings.

pub const DPM_LEVEL_CAP: usize = 16;
const GPU_SLOT_CAP: usize = 32;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuSlot {
    bytes: [u8; GPU_SLOT_CAP],
    len: u8,
}

impl GpuSlot {
    pub fn new(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > GPU_SLOT_CAP || !value.is_ascii() {
            return None;
        }
        let mut slot = Self::default();
        slot.bytes[..value.len()].copy_from_slice(value.as_bytes());
        slot.len = u8::try_from(value.len()).ok()?;
        Some(slot)
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)])
            .expect("GPU slot is validated ASCII")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DpmLevels {
    pub levels_mhz: [u32; DPM_LEVEL_CAP],
    pub len: u8,
    pub current_mhz: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuEngineRow {
    pub slot: GpuSlot,
    pub runtime_status: super::gpu::RuntimeStatus,
    pub gfx_busy_pct: Option<u8>,
    pub vcn_busy_pct: Option<u8>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    pub gtt_used: Option<u64>,
    pub gtt_total: Option<u64>,
    pub sclk: DpmLevels,
    pub mclk: DpmLevels,
}

impl Default for GpuEngineRow {
    fn default() -> Self {
        Self {
            slot: GpuSlot::default(),
            runtime_status: super::gpu::RuntimeStatus::Unknown,
            gfx_busy_pct: None,
            vcn_busy_pct: None,
            vram_used: None,
            vram_total: None,
            gtt_used: None,
            gtt_total: None,
            sclk: DpmLevels::default(),
            mclk: DpmLevels::default(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GpuEnginesPart {
    pub devices: Vec<GpuEngineRow>,
    pub truncated: bool,
}

impl crate::collect::frame::Part for GpuEnginesPart {
    fn clear(&mut self) {
        self.devices.clear();
        self.truncated = false;
    }
}
