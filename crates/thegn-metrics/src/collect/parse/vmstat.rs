//! Exact-key parser for Linux `/proc/vmstat` counters.

use super::{fields, parse_u64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum VmField {
    Pgpgin,
    Pgpgout,
    Pswpin,
    Pswpout,
    Pgfault,
    Pgmajfault,
    PgstealKswapd,
    PgstealDirect,
    PgscanKswapd,
    PgscanDirect,
    AllocstallDma,
    AllocstallDma32,
    AllocstallNormal,
    AllocstallMovable,
    AllocstallDevice,
    CompactStall,
    OomKill,
    WorkingsetRefaultAnon,
    WorkingsetRefaultFile,
    Zswpin,
    Zswpout,
    ThpFaultAlloc,
    ThpCollapseAlloc,
    ThpSplitPage,
    NumaHit,
    NumaMiss,
    NumaForeign,
    NrDirty,
    NrWriteback,
}
impl VmField {
    pub const COUNT: usize = 29;
}

pub const KEYS: &[(&[u8], VmField)] = &[
    (b"pgpgin", VmField::Pgpgin),
    (b"pgpgout", VmField::Pgpgout),
    (b"pswpin", VmField::Pswpin),
    (b"pswpout", VmField::Pswpout),
    (b"pgfault", VmField::Pgfault),
    (b"pgmajfault", VmField::Pgmajfault),
    (b"pgsteal_kswapd", VmField::PgstealKswapd),
    (b"pgsteal_direct", VmField::PgstealDirect),
    (b"pgscan_kswapd", VmField::PgscanKswapd),
    (b"pgscan_direct", VmField::PgscanDirect),
    (b"allocstall_dma", VmField::AllocstallDma),
    (b"allocstall_dma32", VmField::AllocstallDma32),
    (b"allocstall_normal", VmField::AllocstallNormal),
    (b"allocstall_movable", VmField::AllocstallMovable),
    (b"allocstall_device", VmField::AllocstallDevice),
    (b"compact_stall", VmField::CompactStall),
    (b"oom_kill", VmField::OomKill),
    (b"workingset_refault_anon", VmField::WorkingsetRefaultAnon),
    (b"workingset_refault_file", VmField::WorkingsetRefaultFile),
    (b"zswpin", VmField::Zswpin),
    (b"zswpout", VmField::Zswpout),
    (b"thp_fault_alloc", VmField::ThpFaultAlloc),
    (b"thp_collapse_alloc", VmField::ThpCollapseAlloc),
    (b"thp_split_page", VmField::ThpSplitPage),
    (b"numa_hit", VmField::NumaHit),
    (b"numa_miss", VmField::NumaMiss),
    (b"numa_foreign", VmField::NumaForeign),
    (b"nr_dirty", VmField::NrDirty),
    (b"nr_writeback", VmField::NrWriteback),
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VmstatRaw {
    values: [u64; VmField::COUNT],
    present: u64,
}
impl VmstatRaw {
    pub fn get(&self, field: VmField) -> Option<u64> {
        (self.present & (1_u64 << field as usize) != 0).then_some(self.values[field as usize])
    }
}

pub fn parse(buf: &[u8], out: &mut VmstatRaw) {
    *out = VmstatRaw::default();
    for line in buf.split(|byte| *byte == b'\n') {
        let mut parts = fields(line);
        let Some(key) = parts.next() else { continue };
        let Some(value) = parts.next().and_then(parse_u64) else {
            continue;
        };
        let Some((_, field)) = KEYS.iter().find(|(known, _)| *known == key) else {
            continue;
        };
        out.values[*field as usize] = value;
        out.present |= 1_u64 << *field as usize;
    }
}

pub fn field(buf: &[u8], key: &[u8]) -> Option<u64> {
    buf.split(|byte| *byte == b'\n').find_map(|line| {
        let mut parts = fields(line);
        (parts.next()? == key).then(|| parse_u64(parts.next()?))?
    })
}

#[cfg(test)]
mod tests {
    use super::{VmField, VmstatRaw, field, parse};

    #[test]
    fn exact_keys_and_old_kernel_absence() {
        let mut raw = VmstatRaw::default();
        parse(
            include_bytes!("../../../tests/fixtures/mem/vmstat"),
            &mut raw,
        );
        assert_eq!(raw.get(VmField::PgstealDirect), Some(42));
        assert_eq!(raw.get(VmField::Pswpin), Some(10));
        parse(
            include_bytes!("../../../tests/fixtures/mem/vmstat_old_kernel"),
            &mut raw,
        );
        assert_eq!(raw.get(VmField::PgstealDirect), None);
        parse(b"pgsteal_direct_dma 7\npgsteal_direct 42\n", &mut raw);
        assert_eq!(raw.get(VmField::PgstealDirect), Some(42));
        assert_eq!(field(b"pgsteal_direct_dma 7\n", b"pgsteal_direct"), None);
    }
}
