//! Bounded parser for Linux `/proc/net/softnet_stat` hexadecimal rows.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SoftnetCpu {
    pub cpu: u32,
    pub processed: u32,
    pub dropped: u32,
    pub time_squeeze: u32,
    pub received_rps: u32,
    pub flow_limit: u32,
    pub backlog_len: u32,
    pub input_qlen: Option<u32>,
    pub process_qlen: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SoftnetTotals {
    pub cpus: u32,
    pub processed: u64,
    pub dropped: u64,
    pub time_squeeze: u64,
    pub max_dropped: Option<(u32, u32)>,
}

pub fn for_each(input: &[u8], mut visit: impl FnMut(SoftnetCpu)) {
    for (line_index, line) in input.split(|byte| *byte == b'\n').enumerate() {
        let mut values = [0_u32; 16];
        let mut count = 0;
        let mut valid = true;
        for field in super::fields(line) {
            let parsed = std::str::from_utf8(field)
                .ok()
                .and_then(|value| u32::from_str_radix(value, 16).ok());
            let Some(value) = parsed else {
                valid = false;
                break;
            };
            if count < values.len() {
                values[count] = value;
            }
            count += 1;
        }
        if !valid || count < 11 {
            continue;
        }
        let cpu = if count >= 13 {
            values[12]
        } else {
            u32::try_from(line_index).unwrap_or(u32::MAX)
        };
        visit(SoftnetCpu {
            cpu,
            processed: values[0],
            dropped: values[1],
            time_squeeze: values[2],
            received_rps: values[8],
            flow_limit: values[10],
            backlog_len: values[7],
            input_qlen: (count >= 15).then_some(values[13]),
            process_qlen: (count >= 15).then_some(values[14]),
        });
    }
}

pub fn totals(input: &[u8]) -> SoftnetTotals {
    let mut total = SoftnetTotals::default();
    for_each(input, |row| {
        total.cpus = total.cpus.saturating_add(1);
        total.processed = total.processed.saturating_add(u64::from(row.processed));
        total.dropped = total.dropped.saturating_add(u64::from(row.dropped));
        total.time_squeeze = total
            .time_squeeze
            .saturating_add(u64::from(row.time_squeeze));
        if total
            .max_dropped
            .is_none_or(|(_, maximum)| row.dropped > maximum)
        {
            total.max_dropped = Some((row.cpu, row.dropped));
        }
    });
    total
}

#[cfg(test)]
mod tests {
    use super::{SoftnetCpu, for_each, totals};
    #[test]
    fn supports_kernel_row_widths_and_cpu_id_column() {
        let input = b"1 2 3 4 5 6 7 8 9 a b\n10 20 30 4 5 6 7 8 9 a b 99 7\n100 200 300 4 5 6 7 8 9 a b 99 3 14 15\nshort 1\nzz 2 3 4 5 6 7 8 9 a b\n";
        let mut rows = Vec::<SoftnetCpu>::new();
        for_each(input, |row| rows.push(row));
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].cpu, rows[1].cpu, rows[2].cpu), (0, 7, 3));
        assert_eq!(rows[2].input_qlen, Some(0x14));
        assert_eq!(rows[2].process_qlen, Some(0x15));
        let result = totals(input);
        assert_eq!(result.cpus, 3);
        assert_eq!(result.processed, 0x111);
        assert_eq!(result.dropped, 0x222);
        assert_eq!(result.max_dropped, Some((3, 0x200)));
    }
}
