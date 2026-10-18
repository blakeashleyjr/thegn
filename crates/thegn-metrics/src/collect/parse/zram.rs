//! Parser for zram's whitespace-separated `mm_stat` counters.

use super::{fields, parse_u64};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZramMmStat {
    pub orig_data_size: u64,
    pub compr_data_size: u64,
    pub mem_used_total: u64,
    pub mem_limit: u64,
    pub mem_used_max: u64,
    pub same_pages: u64,
    pub pages_compacted: u64,
    pub huge_pages: Option<u64>,
    pub huge_pages_since: Option<u64>,
}

impl ZramMmStat {
    pub fn ratio_milli(self) -> Option<u64> {
        (self.compr_data_size != 0).then(|| {
            self.orig_data_size
                .checked_mul(1000)
                .map(|value| value / self.compr_data_size)
        })?
    }
}

pub fn parse_mm_stat(buf: &[u8]) -> Option<ZramMmStat> {
    let mut values = [0_u64; 9];
    let mut count = 0;
    for field in fields(buf).take(values.len()) {
        values[count] = parse_u64(field)?;
        count += 1;
    }
    if !(7..=9).contains(&count) {
        return None;
    }
    Some(ZramMmStat {
        orig_data_size: values[0],
        compr_data_size: values[1],
        mem_used_total: values[2],
        mem_limit: values[3],
        mem_used_max: values[4],
        same_pages: values[5],
        pages_compacted: values[6],
        huge_pages: (count >= 8).then_some(values[7]),
        huge_pages_since: (count >= 9).then_some(values[8]),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_mm_stat;

    #[test]
    fn supports_kernel_field_variants_and_zero_compressed_size() {
        let seven = parse_mm_stat(include_bytes!("../../../tests/fixtures/mem/mm_stat_7")).unwrap();
        assert_eq!(seven.huge_pages, None);
        let eight = parse_mm_stat(include_bytes!("../../../tests/fixtures/mem/mm_stat_8")).unwrap();
        assert_eq!(eight.huge_pages, Some(4));
        assert_eq!(eight.huge_pages_since, None);
        let nine = parse_mm_stat(include_bytes!("../../../tests/fixtures/mem/mm_stat_9")).unwrap();
        assert_eq!(nine.huge_pages_since, Some(5));
        let ten = parse_mm_stat(b"100 50 60 0 70 2 3 4 5 6").unwrap();
        assert_eq!(ten.huge_pages, Some(4));
        assert_eq!(ten.huge_pages_since, Some(5));
        assert_eq!(parse_mm_stat(b"1 2 3 4 5 6"), None);
        assert_eq!(parse_mm_stat(b"1 2 3 4 5 6 nope"), None);
        assert_eq!(parse_mm_stat(b"1 0 3 4 5 6 7").unwrap().ratio_milli(), None);
    }
}
