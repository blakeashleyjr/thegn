//! Per-process clients reported by Linux DRM fdinfo.

pub const ENGINE_SLOTS: usize = crate::collect::parse::fdinfo::ENGINE_SLOTS;

#[derive(Debug, Clone, PartialEq)]
pub struct GpuClientRow {
    pub pid: u32,
    pub start_time: u64,
    pub pdev: [u8; 32],
    pub pdev_len: u8,
    pub client_id: u64,
    pub engine_busy_pct: [Option<f64>; ENGINE_SLOTS],
    pub memory_resident: Option<u64>,
    pub memory_total: Option<u64>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GpuClientsPart {
    pub clients: Vec<GpuClientRow>,
    pub truncated: bool,
    pub pids_truncated: bool,
}

impl crate::collect::frame::Part for GpuClientsPart {
    fn clear(&mut self) {
        self.clients.clear();
        self.truncated = false;
        self.pids_truncated = false;
    }
}
