//! Pure parser for `/proc/uptime`.
use super::{fields, psi};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Uptime {
    pub up_centi: u64,
    pub idle_centi: u64,
}

pub fn parse(input: &[u8]) -> Option<Uptime> {
    let mut values = fields(input);
    Some(Uptime {
        up_centi: psi::centi(values.next()?)?,
        idle_centi: psi::centi(values.next()?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::parse;
    #[test]
    fn parses_proc_format_and_rejects_malformed_fields() {
        assert_eq!(
            parse(b"647225.38 12219762.77\n").unwrap().up_centi,
            64_722_538
        );
        assert!(parse(b"not uptime").is_none());
        assert!(parse(b"1").is_none());
    }
}
