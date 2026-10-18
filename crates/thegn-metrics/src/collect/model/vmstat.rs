//! Vmstat snapshot and counter rates.

use crate::collect::{
    delta::Counter,
    frame::Part,
    parse::vmstat::{VmField, VmstatRaw},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum VmRate {
    Pswpin,
    Pswpout,
    Pgfault,
    Pgmajfault,
    StealKswapd,
    StealDirect,
    ScanKswapd,
    ScanDirect,
    RefaultAnon,
    RefaultFile,
    CompactStall,
    Allocstall,
    OomKill,
    Zswpin,
    Zswpout,
}
impl VmRate {
    pub const COUNT: usize = 15;
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct VmstatPart {
    pub raw: VmstatRaw,
    pub rates: [Option<f64>; VmRate::COUNT],
}
impl Part for VmstatPart {
    fn clear(&mut self) {
        self.raw = VmstatRaw::default();
        self.rates.fill(None);
    }
}

#[derive(Debug, Clone)]
pub struct VmstatDelta([Counter; VmRate::COUNT]);
impl Default for VmstatDelta {
    fn default() -> Self {
        Self(std::array::from_fn(|_| Counter::new()))
    }
}
impl VmstatDelta {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn update(&mut self, raw: &VmstatRaw, now_ms: u64, part: &mut VmstatPart) {
        part.raw = *raw;
        part.rates.fill(None);
        for (rate, field) in [
            (VmRate::Pswpin, VmField::Pswpin),
            (VmRate::Pswpout, VmField::Pswpout),
            (VmRate::Pgfault, VmField::Pgfault),
            (VmRate::Pgmajfault, VmField::Pgmajfault),
            (VmRate::StealKswapd, VmField::PgstealKswapd),
            (VmRate::StealDirect, VmField::PgstealDirect),
            (VmRate::ScanKswapd, VmField::PgscanKswapd),
            (VmRate::ScanDirect, VmField::PgscanDirect),
            (VmRate::RefaultAnon, VmField::WorkingsetRefaultAnon),
            (VmRate::RefaultFile, VmField::WorkingsetRefaultFile),
            (VmRate::CompactStall, VmField::CompactStall),
            (VmRate::OomKill, VmField::OomKill),
            (VmRate::Zswpin, VmField::Zswpin),
            (VmRate::Zswpout, VmField::Zswpout),
        ] {
            if let Some(value) = raw.get(field) {
                part.rates[rate as usize] = self.0[rate as usize].rate(value, now_ms);
            } else {
                self.0[rate as usize] = Counter::new();
            }
        }
        let alloc_fields = [
            VmField::AllocstallDma,
            VmField::AllocstallDma32,
            VmField::AllocstallNormal,
            VmField::AllocstallMovable,
            VmField::AllocstallDevice,
        ];
        let mut total = 0_u64;
        let mut any = false;
        for field in alloc_fields {
            if let Some(value) = raw.get(field) {
                total = total.saturating_add(value);
                any = true;
            }
        }
        if any {
            part.rates[VmRate::Allocstall as usize] =
                self.0[VmRate::Allocstall as usize].rate(total, now_ms);
        } else {
            self.0[VmRate::Allocstall as usize] = Counter::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{VmRate, VmstatDelta, VmstatPart};
    use crate::collect::parse::vmstat::{VmstatRaw, parse};

    #[test]
    fn computes_rates_reprimes_on_reset_and_sums_present_allocstall_fields() {
        let mut delta = VmstatDelta::new();
        let mut raw = VmstatRaw::default();
        let mut part = VmstatPart::default();
        parse(
            b"pswpin 10\nallocstall_dma 2\nallocstall_normal 3\nallocstall_movable 4\n",
            &mut raw,
        );
        delta.update(&raw, 1000, &mut part);
        assert_eq!(part.rates[VmRate::Pswpin as usize], None);
        parse(
            b"pswpin 20\nallocstall_dma 4\nallocstall_normal 5\nallocstall_movable 7\n",
            &mut raw,
        );
        delta.update(&raw, 2000, &mut part);
        assert_eq!(part.rates[VmRate::Pswpin as usize], Some(10.0));
        assert_eq!(part.rates[VmRate::Allocstall as usize], Some(7.0));
        parse(b"pswpin 2\n", &mut raw);
        delta.update(&raw, 3000, &mut part);
        assert_eq!(part.rates[VmRate::Pswpin as usize], None);
        parse(b"pswpin 12\n", &mut raw);
        delta.update(&raw, 4000, &mut part);
        assert_eq!(part.rates[VmRate::Pswpin as usize], Some(10.0));
        assert_eq!(part.rates[VmRate::Allocstall as usize], None);
    }
}
