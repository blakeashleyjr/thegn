//! Allocation-free parser and delta math for Linux DRM fdinfo counters.

pub const ENGINE_SLOTS: usize = 16;

/// Stable indices for numeric DRM fdinfo keys. Engine counters are in nanoseconds
/// unless their key says `cycles`; capacities are nanoseconds per wall second.
pub const KEY_TABLE: [(&[u8], usize); ENGINE_SLOTS] = [
    (b"drm-engine-render", 0),
    (b"drm-engine-gfx", 1),
    (b"drm-engine-compute", 2),
    (b"drm-engine-copy", 3),
    (b"drm-engine-video", 4),
    (b"drm-engine-video-enhance", 5),
    (b"drm-engine-decode", 6),
    (b"drm-engine-encode", 7),
    (b"drm-cycles-render", 8),
    (b"drm-cycles-gfx", 9),
    (b"drm-cycles-compute", 10),
    (b"drm-total-cycles", 11),
    (b"drm-engine-capacity-render", 12),
    (b"drm-engine-capacity-gfx", 13),
    (b"drm-engine-capacity-compute", 14),
    (b"drm-engine-capacity-copy", 15),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fdinfo {
    pub slots: [u64; ENGINE_SLOTS],
    pub present: u16,
    pub client_id: Option<u64>,
    pub pdev: [u8; 32],
    pub pdev_len: u8,
}

impl Default for Fdinfo {
    fn default() -> Self {
        Self {
            slots: [0; ENGINE_SLOTS],
            present: 0,
            client_id: None,
            pdev: [0; 32],
            pdev_len: 0,
        }
    }
}

/// Parse numeric DRM keys plus the byte-valued device identity used for dedupe.
pub fn parse(input: &[u8]) -> Fdinfo {
    let mut result = Fdinfo::default();
    for line in input.split(|byte| *byte == b'\n') {
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let key = &line[..colon];
        let value = trim_ascii(&line[colon + 1..]);
        if key == b"drm-pdev" && value.len() <= result.pdev.len() {
            result.pdev[..value.len()].copy_from_slice(value);
            result.pdev_len = value.len() as u8;
        } else if key == b"drm-client-id" {
            result.client_id = parse_u64(value);
        } else if let Some((_, index)) = KEY_TABLE.iter().find(|(known, _)| *known == key)
            && let Some(number) = parse_u64(value)
        {
            result.slots[*index] = number;
            result.present |= 1 << index;
        }
    }
    result
}

fn trim_ascii(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(input.len());
    let end = input
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &input[start..end]
}

fn parse_u64(input: &[u8]) -> Option<u64> {
    if input.is_empty() {
        return None;
    }
    input.iter().try_fold(0_u64, |value, byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(*byte - b'0'))
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delta {
    pub util: [f64; ENGINE_SLOTS],
    pub valid: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Baseline {
    pub start_time: u64,
    pub wall_ns: u64,
    pub slots: [u64; ENGINE_SLOTS],
    pub present: u16,
}

impl Default for Baseline {
    fn default() -> Self {
        Self {
            start_time: 0,
            wall_ns: 0,
            slots: [0; ENGINE_SLOTS],
            present: 0,
        }
    }
}

/// Calculate bounded utilization. Regression, identity change, or an absent
/// baseline primes the counters and returns no sample.
pub fn delta(
    previous: &mut Option<Baseline>,
    start_time: u64,
    wall_ns: u64,
    current: &Fdinfo,
) -> Option<Delta> {
    let Some(old) = *previous else {
        *previous = Some(Baseline {
            start_time,
            wall_ns,
            slots: current.slots,
            present: current.present,
        });
        return None;
    };
    if old.start_time != start_time
        || wall_ns <= old.wall_ns
        || (0..ENGINE_SLOTS).any(|i| current.slots[i] < old.slots[i])
    {
        *previous = Some(Baseline {
            start_time,
            wall_ns,
            slots: current.slots,
            present: current.present,
        });
        return None;
    }
    let elapsed = wall_ns - old.wall_ns;
    let mut result = Delta {
        util: [0.0; ENGINE_SLOTS],
        valid: 0,
    };
    for index in 0..ENGINE_SLOTS {
        if current.present & (1 << index) == 0 || old.present & (1 << index) == 0 {
            continue;
        }
        let amount = current.slots[index] - old.slots[index];
        let divisor = if (8..=10).contains(&index) {
            current.slots[11].saturating_sub(old.slots[11])
        } else {
            elapsed
        };
        if divisor == 0 {
            continue;
        }
        let capacity = match index {
            0 if current.slots[12] > 0 => current.slots[12],
            1 if current.slots[13] > 0 => current.slots[13],
            _ => 1,
        };
        result.util[index] = (amount as f64 / divisor as f64 / capacity as f64).clamp(0.0, 1.0);
        result.valid |= 1 << index;
    }
    *previous = Some(Baseline {
        start_time,
        wall_ns,
        slots: current.slots,
        present: current.present,
    });
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::{Baseline, Fdinfo, delta, parse};

    #[test]
    fn parses_driver_keys_without_losing_device_identity() {
        let parsed = parse(
            b"drm-driver: amdgpu\ndrm-pdev: 0000:03:00.0\ndrm-client-id: 42\ndrm-engine-gfx: 900\n",
        );
        assert_eq!(
            &parsed.pdev[..usize::from(parsed.pdev_len)],
            b"0000:03:00.0"
        );
        assert_eq!(parsed.client_id, Some(42));
        assert_eq!(parsed.slots[1], 900);
        assert_eq!(parse(b"drm-engine-render: 4x\n").present, 0);
    }

    #[test]
    fn regression_and_pid_reuse_reprime_before_reporting() {
        let mut baseline = None;
        let mut current = Fdinfo {
            slots: [0; 16],
            present: 1,
            ..Fdinfo::default()
        };
        current.slots[0] = 10;
        assert!(delta(&mut baseline, 7, 100, &current).is_none());
        current.slots[0] = 20;
        let sample = delta(&mut baseline, 7, 200, &current).unwrap();
        assert!((sample.util[0] - 0.1).abs() < 0.000_001);
        current.slots[0] = 5;
        assert!(delta(&mut baseline, 7, 300, &current).is_none());
        assert!(delta(&mut baseline, 8, 400, &current).is_none());
        assert_eq!(
            baseline,
            Some(Baseline {
                start_time: 8,
                wall_ns: 400,
                slots: current.slots,
                present: 1
            })
        );
    }

    #[test]
    fn accepts_amdgpu_i915_and_xe_engine_shapes() {
        let amdgpu = parse(b"drm-driver: amdgpu\ndrm-engine-gfx: 12\ndrm-engine-compute: 8\n");
        let i915 =
            parse(b"drm-driver: i915\ndrm-engine-render: 31\ndrm-engine-capacity-render: 2\n");
        let xe = parse(b"drm-driver: xe\ndrm-cycles-render: 50\ndrm-total-cycles: 100\n");
        assert_eq!((amdgpu.slots[1], amdgpu.slots[2]), (12, 8));
        assert_eq!((i915.slots[0], i915.slots[12]), (31, 2));
        assert_eq!((xe.slots[8], xe.slots[11]), (50, 100));
    }
}
