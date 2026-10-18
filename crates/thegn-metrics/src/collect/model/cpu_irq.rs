//! Per-core IRQ and softirq rates.
use super::super::frame::Part;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CoreIrqSample {
    pub id: u16,
    pub softirq_per_s: [Option<f64>; 3],
    pub res_per_s: Option<f64>,
    pub cal_per_s: Option<f64>,
    pub tlb_per_s: Option<f64>,
    pub loc_per_s: Option<f64>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SoftirqTop {
    pub name_hash: u64,
    pub total_per_s: Option<f64>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NumericIrqTop {
    pub irq: u32,
    pub per_s: Option<f64>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct CpuIrqPart {
    pub cores: Vec<CoreIrqSample>,
    pub softirq_top: [SoftirqTop; 3],
    pub thr_per_s: Option<f64>,
    pub mce_per_s: Option<f64>,
    pub dfr_per_s: Option<f64>,
    pub numeric_top: Vec<NumericIrqTop>,
}
impl Default for CpuIrqPart {
    fn default() -> Self {
        Self {
            cores: Vec::new(),
            softirq_top: [SoftirqTop::default(); 3],
            thr_per_s: None,
            mce_per_s: None,
            dfr_per_s: None,
            numeric_top: Vec::with_capacity(5),
        }
    }
}
impl Part for CpuIrqPart {
    fn clear(&mut self) {
        self.cores.clear();
        self.softirq_top = [SoftirqTop::default(); 3];
        self.thr_per_s = None;
        self.mce_per_s = None;
        self.dfr_per_s = None;
        self.numeric_top.clear();
    }
}
