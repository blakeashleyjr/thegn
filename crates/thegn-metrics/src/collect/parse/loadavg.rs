//! Pure parser for `/proc/loadavg`.
use super::{fields, psi};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Loadavg {
    pub load1: u32,
    pub load5: u32,
    pub load15: u32,
    pub running: u32,
    pub total: u32,
    pub last_pid: u32,
}

pub fn parse(input: &[u8]) -> Option<Loadavg> {
    let mut values = fields(input);
    let load1 = u32::try_from(psi::centi(values.next()?)?).ok()?;
    let load5 = u32::try_from(psi::centi(values.next()?)?).ok()?;
    let load15 = u32::try_from(psi::centi(values.next()?)?).ok()?;
    let counts = values.next()?;
    let slash = counts.iter().position(|byte| *byte == b'/')?;
    let running = &counts[..slash];
    let total = &counts[slash + 1..];
    Some(Loadavg {
        load1,
        load5,
        load15,
        running: u32::try_from(super::parse_u64(running)?).ok()?,
        total: u32::try_from(super::parse_u64(total)?).ok()?,
        last_pid: u32::try_from(super::parse_u64(values.next()?)?).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::parse;
    #[test]
    fn parses_proc_format_and_rejects_malformed_fields() {
        let value = parse(b"5.17 5.22 6.72 2/7791 1781349\n").unwrap();
        assert_eq!((value.load1, value.load5, value.load15), (517, 522, 672));
        assert_eq!(
            (value.running, value.total, value.last_pid),
            (2, 7791, 1_781_349)
        );
        assert!(parse(b"bad").is_none());
        assert!(parse(b"1 2 3 x 4").is_none());
    }
}
