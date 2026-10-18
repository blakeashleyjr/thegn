//! Allocation-free parser for the `pp_dpm_sclk` and `pp_dpm_mclk` tables.

use crate::collect::model::gpu_engines::{DPM_LEVEL_CAP, DpmLevels};

/// Parses numbered MHz rows and their optional active `*` marker. The special
/// `S:` deep-sleep row is not a DPM level, but its marked frequency is current.
/// A ladder with no marked row is valid but has no current frequency.
pub fn parse(bytes: &[u8]) -> Option<DpmLevels> {
    let mut result = DpmLevels::default();
    let mut saw_level = false;
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = trim(line);
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"S:") {
            let mut fields = trim(rest)
                .split(u8::is_ascii_whitespace)
                .filter(|part| !part.is_empty());
            let frequency = fields.next()?;
            let mhz = parse_frequency(frequency)?;
            match fields.next() {
                Some(marker) if marker == b"*" && fields.next().is_none() => {
                    result.current_mhz = Some(mhz);
                }
                None => {}
                _ => return None,
            }
            continue;
        }
        let colon = line.iter().position(|byte| *byte == b':')?;
        let (index, with_colon) = line.split_at(colon);
        let rest = &with_colon[1..];
        if index.is_empty() || !index.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let rest = trim(rest);
        let mut fields = rest
            .split(u8::is_ascii_whitespace)
            .filter(|part| !part.is_empty());
        let frequency = fields.next()?;
        let mhz = parse_frequency(frequency)?;
        let active = match fields.next() {
            None => false,
            Some(marker) if marker == b"*" && fields.next().is_none() => true,
            _ => return None,
        };
        let count = usize::from(result.len);
        if count == DPM_LEVEL_CAP {
            return None;
        }
        result.levels_mhz[count] = mhz;
        result.len += 1;
        result.current_mhz = active.then_some(mhz).or(result.current_mhz);
        saw_level = true;
    }
    saw_level.then_some(result)
}

fn parse_frequency(bytes: &[u8]) -> Option<u32> {
    let suffix = bytes.get(bytes.len().checked_sub(3)?..)?;
    if !suffix.eq_ignore_ascii_case(b"mhz") {
        return None;
    }
    parse_u32(&bytes[..bytes.len() - 3])
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0_u32, |value, byte| {
        if byte.is_ascii_digit() {
            value.checked_mul(10)?.checked_add(u32::from(*byte - b'0'))
        } else {
            None
        }
    })
}

fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_marked_ladder_and_ignores_deep_sleep_row() {
        // Kernel amdgpu_pm.c documents `S: 19Mhz *` and numbered `Mhz`
        // rows at:
        // https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/amd/pm/amdgpu_pm.c#L3831-L3845.
        let dpm = parse(b"S: 0Mhz \n0: 600Mhz \n1: 764Mhz *\n2: 2900Mhz \n").unwrap();
        assert_eq!(dpm.len, 3);
        assert_eq!(&dpm.levels_mhz[..3], &[600, 764, 2900]);
        assert_eq!(dpm.current_mhz, Some(764));
    }

    #[test]
    fn parses_exact_kernel_deep_sleep_and_unmarked_row_shape() {
        let exact_rows = parse(b"0: 600Mhz \n1: 764Mhz *\n").unwrap();
        assert_eq!(exact_rows.len, 2);
        assert_eq!(exact_rows.current_mhz, Some(764));

        let dpm = parse(b"S: 19Mhz *\n0: 600Mhz \n1: 764MHz *\n").unwrap();
        assert_eq!(dpm.len, 2);
        assert_eq!(dpm.current_mhz, Some(764));

        let deep_sleep = parse(b"S: 19Mhz *\n0: 600Mhz \n1: 764Mhz \n").unwrap();
        assert_eq!(deep_sleep.current_mhz, Some(19));
    }

    #[test]
    fn ladder_without_active_marker_has_no_current_level() {
        let dpm = parse(b"0: 600Mhz \n1: 764Mhz \nS: 0Mhz \n").unwrap();
        assert_eq!(dpm.current_mhz, None);
        assert_eq!(parse(b"S: 0Mhz\n"), None);
        assert_eq!(parse(b"0: 600Mhz * trailing\n"), None);
    }
}
