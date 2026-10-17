//! Pure parser for Linux system and cgroup pressure files.
use super::{fields, parse_u64};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PsiLine {
    pub avg10: u32,
    pub avg60: u32,
    pub avg300: u32,
    pub total_us: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PsiFile {
    pub some: Option<PsiLine>,
    pub full: Option<PsiLine>,
}

/// Parse a decimal percentage into hundredths, truncating excess precision.
pub fn centi(input: &[u8]) -> Option<u64> {
    if input.is_empty() {
        return None;
    }
    let dot = input.iter().position(|byte| *byte == b'.');
    let whole = dot.map_or(input, |index| &input[..index]);
    if whole.is_empty() {
        return None;
    }
    let whole = parse_u64(whole)?;
    let fraction = dot.map_or(&[][..], |index| &input[index + 1..]);
    if dot.is_some() && (fraction.is_empty() || fraction.iter().any(|b| !b.is_ascii_digit())) {
        return None;
    }
    let mut hundredths = 0_u64;
    for digit in fraction.iter().take(2) {
        hundredths = hundredths
            .checked_mul(10)?
            .checked_add(u64::from(*digit - b'0'))?;
    }
    if fraction.len() == 1 {
        hundredths = hundredths.checked_mul(10)?;
    }
    whole.checked_mul(100)?.checked_add(hundredths)
}

/// Parse complete `some` and `full` rows; incomplete rows are omitted.
pub fn parse(input: &[u8]) -> PsiFile {
    let mut result = PsiFile::default();
    for line in input.split(|byte| *byte == b'\n') {
        let mut tokens = fields(line);
        let Some(kind) = tokens.next() else {
            continue;
        };
        let mut avg10 = None;
        let mut avg60 = None;
        let mut avg300 = None;
        let mut total = None;
        for field in fields(line).skip(1) {
            let Some(separator) = field.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            let key = &field[..separator];
            let value = &field[separator + 1..];
            match key {
                b"avg10" => avg10 = centi(value).and_then(|v| u32::try_from(v).ok()),
                b"avg60" => avg60 = centi(value).and_then(|v| u32::try_from(v).ok()),
                b"avg300" => avg300 = centi(value).and_then(|v| u32::try_from(v).ok()),
                b"total" => total = parse_u64(value),
                _ => {}
            }
        }
        let (Some(avg10), Some(avg60), Some(avg300), Some(total_us)) =
            (avg10, avg60, avg300, total)
        else {
            continue;
        };
        let value = PsiLine {
            avg10,
            avg60,
            avg300,
            total_us,
        };
        match kind {
            b"some" => result.some = Some(value),
            b"full" => result.full = Some(value),
            _ => {}
        }
    }
    result
}

/// Percentage of elapsed time spent stalled, in hundredths of a percent.
pub fn stall_pct_centi(prev_total_us: u64, cur_total_us: u64, dt_ms: u64) -> Option<u32> {
    let delta = cur_total_us.checked_sub(prev_total_us)?;
    if dt_ms == 0 {
        return None;
    }
    let value = u128::from(delta).checked_mul(10_000)? / u128::from(dt_ms).checked_mul(1_000)?;
    Some(value.min(10_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::{centi, parse, stall_pct_centi};
    #[test]
    fn parses_system_cgroup_optional_and_reordered_rows_safely() {
        let system = parse(b"some avg10=1.25 avg60=2 avg300=0.5 total=50000\nfull avg10=0 avg60=0 avg300=0 total=2\n");
        assert_eq!(system.some.unwrap().avg10, 125);
        assert!(system.full.is_some());
        assert_eq!(
            parse(b"some total=7 avg300=3 avg10=1 avg60=2\n")
                .some
                .unwrap()
                .total_us,
            7
        );
        assert!(
            parse(b"full avg10=1 avg60=2 avg300=3 total=9\n")
                .some
                .is_none()
        );
        let irq = parse(b"full total=9 avg300=3 avg10=1 avg60=2\n");
        assert!(irq.some.is_none());
        assert!(irq.full.is_some());
        assert!(parse(b"some avg10=1 avg60=2\n").some.is_none());
        assert!(
            parse(b"some total=18446744073709551616 avg10=1 avg60=2 avg300=3\n")
                .some
                .is_none()
        );
        assert_eq!(parse(b"garbage\n\xff\n"), Default::default());
        assert_eq!(parse(b""), Default::default());
    }
    #[test]
    fn decimal_and_stall_edges_are_bounded() {
        assert_eq!(centi(b"12"), Some(1200));
        assert_eq!(centi(b"0.5"), Some(50));
        assert_eq!(centi(b"1.239"), Some(123));
        assert_eq!(centi(b""), None);
        assert_eq!(centi(b"1.x"), None);
        assert_eq!(stall_pct_centi(10, 50_010, 1000), Some(500));
        assert_eq!(stall_pct_centi(51, 50, 1000), None);
        assert_eq!(stall_pct_centi(0, 1, 0), None);
        assert_eq!(stall_pct_centi(0, u64::MAX, 1), Some(10_000));
    }
}
