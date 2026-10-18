//! Header-keyed parser for Linux `/proc/meminfo`.

use super::{fields, parse_u64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum MemField {
    MemTotal,
    MemFree,
    MemAvailable,
    Buffers,
    Cached,
    SwapCached,
    Active,
    Inactive,
    ActiveAnon,
    InactiveAnon,
    ActiveFile,
    InactiveFile,
    Unevictable,
    Mlocked,
    SwapTotal,
    SwapFree,
    Zswap,
    Zswapped,
    Dirty,
    Writeback,
    AnonPages,
    Mapped,
    Shmem,
    KReclaimable,
    Slab,
    SReclaimable,
    SUnreclaim,
    KernelStack,
    PageTables,
    SecPageTables,
    CommitLimit,
    CommittedAs,
    VmallocUsed,
    Percpu,
    HardwareCorrupted,
    AnonHugePages,
    ShmemHugePages,
    FileHugePages,
    Balloon,
    GpuActive,
    GpuReclaim,
    HugePagesTotal,
    HugePagesFree,
    HugePagesRsvd,
    HugePagesSurp,
    Hugepagesize,
    DirectMap4k,
    DirectMap2m,
    DirectMap1g,
}

impl MemField {
    pub const COUNT: usize = 49;
}

pub const KEYS: &[(&[u8], MemField)] = &[
    (b"MemTotal", MemField::MemTotal),
    (b"MemFree", MemField::MemFree),
    (b"MemAvailable", MemField::MemAvailable),
    (b"Buffers", MemField::Buffers),
    (b"Cached", MemField::Cached),
    (b"SwapCached", MemField::SwapCached),
    (b"Active", MemField::Active),
    (b"Inactive", MemField::Inactive),
    (b"Active(anon)", MemField::ActiveAnon),
    (b"Inactive(anon)", MemField::InactiveAnon),
    (b"Active(file)", MemField::ActiveFile),
    (b"Inactive(file)", MemField::InactiveFile),
    (b"Unevictable", MemField::Unevictable),
    (b"Mlocked", MemField::Mlocked),
    (b"SwapTotal", MemField::SwapTotal),
    (b"SwapFree", MemField::SwapFree),
    (b"Zswap", MemField::Zswap),
    (b"Zswapped", MemField::Zswapped),
    (b"Dirty", MemField::Dirty),
    (b"Writeback", MemField::Writeback),
    (b"AnonPages", MemField::AnonPages),
    (b"Mapped", MemField::Mapped),
    (b"Shmem", MemField::Shmem),
    (b"KReclaimable", MemField::KReclaimable),
    (b"Slab", MemField::Slab),
    (b"SReclaimable", MemField::SReclaimable),
    (b"SUnreclaim", MemField::SUnreclaim),
    (b"KernelStack", MemField::KernelStack),
    (b"PageTables", MemField::PageTables),
    (b"SecPageTables", MemField::SecPageTables),
    (b"CommitLimit", MemField::CommitLimit),
    (b"Committed_AS", MemField::CommittedAs),
    (b"VmallocUsed", MemField::VmallocUsed),
    (b"Percpu", MemField::Percpu),
    (b"HardwareCorrupted", MemField::HardwareCorrupted),
    (b"AnonHugePages", MemField::AnonHugePages),
    (b"ShmemHugePages", MemField::ShmemHugePages),
    (b"FileHugePages", MemField::FileHugePages),
    (b"Balloon", MemField::Balloon),
    (b"GPUActive", MemField::GpuActive),
    (b"GPUReclaim", MemField::GpuReclaim),
    (b"HugePages_Total", MemField::HugePagesTotal),
    (b"HugePages_Free", MemField::HugePagesFree),
    (b"HugePages_Rsvd", MemField::HugePagesRsvd),
    (b"HugePages_Surp", MemField::HugePagesSurp),
    (b"Hugepagesize", MemField::Hugepagesize),
    (b"DirectMap4k", MemField::DirectMap4k),
    (b"DirectMap2M", MemField::DirectMap2m),
    (b"DirectMap1G", MemField::DirectMap1g),
];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MeminfoRaw {
    values: [u64; MemField::COUNT],
    present: u64,
}

impl MeminfoRaw {
    pub fn get(&self, field: MemField) -> Option<u64> {
        (self.present & (1_u64 << field as usize) != 0).then_some(self.values[field as usize])
    }
}

pub fn parse(buf: &[u8], out: &mut MeminfoRaw) {
    *out = MeminfoRaw::default();
    for (key, value) in super::kv_lines(buf) {
        let Some((_, field)) = KEYS.iter().find(|(known, _)| *known == key) else {
            continue;
        };
        let mut parts = fields(value);
        let Some(number) = parts.next().and_then(parse_u64) else {
            continue;
        };
        let unit = parts.next();
        if parts.next().is_some() {
            continue;
        }
        let stored = if unit == Some(&b"kB"[..]) {
            let Some(bytes) = number.checked_mul(1024) else {
                continue;
            };
            bytes
        } else {
            if unit.is_some()
                || !matches!(
                    *field,
                    MemField::HugePagesTotal
                        | MemField::HugePagesFree
                        | MemField::HugePagesRsvd
                        | MemField::HugePagesSurp
                )
            {
                continue;
            }
            number
        };
        out.values[*field as usize] = stored;
        out.present |= 1_u64 << *field as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::{MemField, MeminfoRaw, parse};

    #[test]
    fn parses_header_keys_units_and_absence() {
        let mut raw = MeminfoRaw::default();
        parse(
            include_bytes!("../../../tests/fixtures/mem/meminfo_gpu"),
            &mut raw,
        );
        assert_eq!(raw.get(MemField::MemTotal), Some(10_240));
        assert_eq!(raw.get(MemField::MemAvailable), Some(4_096));
        assert_eq!(raw.get(MemField::Zswap), Some(2_048));
        assert_eq!(raw.get(MemField::GpuActive), Some(3_072));
        assert_eq!(raw.get(MemField::HugePagesTotal), Some(7));
        parse(
            include_bytes!("../../../tests/fixtures/mem/meminfo_no_zswap"),
            &mut raw,
        );
        assert_eq!(raw.get(MemField::Zswap), None);
    }

    #[test]
    fn overflow_malformed_and_prefix_inputs_are_absent_and_safe() {
        let mut raw = MeminfoRaw::default();
        parse(b"MemTotal: 18014398509481984 kB\n", &mut raw);
        assert_eq!(raw.get(MemField::MemTotal), None);
        for end in 0..=b"MemTotal: 9 kB\n".len() {
            parse(&b"MemTotal: 9 kB\n"[..end], &mut raw);
        }
        parse(b"Foo:\nMemTotal: x kB\n\xff", &mut raw);
    }
}
