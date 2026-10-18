//! Allocation-free parser and delta math for Linux DRM fdinfo counters.

pub const ENGINE_SLOTS: usize = 8;
const SLOT_VALUES: usize = ENGINE_SLOTS * 2;
const TOTAL_OFFSET: usize = ENGINE_SLOTS;

/// Stable indices for the engine names used by amdgpu, i915, and xe.
/// The second half of `slots` holds per-class xe total-cycle counters.
pub const KEY_TABLE: [(&[u8], usize); 24] = [
    (b"drm-engine-gfx", 0),
    (b"drm-engine-compute", 1),
    (b"drm-engine-dma", 2),
    (b"drm-engine-dec", 3),
    (b"drm-engine-enc", 4),
    (b"drm-engine-enc_1", 5),
    (b"drm-engine-jpeg", 6),
    (b"drm-engine-vpe", 7),
    (b"drm-engine-render", 0),
    (b"drm-engine-copy", 2),
    (b"drm-engine-video", 3),
    (b"drm-engine-video-enhance", 6),
    (b"drm-cycles-rcs", 0),
    (b"drm-cycles-bcs", 1),
    (b"drm-cycles-vcs", 2),
    (b"drm-cycles-vecs", 3),
    (b"drm-cycles-ccs", 4),
    (b"drm-cycles-other", 5),
    (b"drm-total-cycles-rcs", TOTAL_OFFSET),
    (b"drm-total-cycles-bcs", TOTAL_OFFSET + 1),
    (b"drm-total-cycles-vcs", TOTAL_OFFSET + 2),
    (b"drm-total-cycles-vecs", TOTAL_OFFSET + 3),
    (b"drm-total-cycles-ccs", TOTAL_OFFSET + 4),
    (b"drm-total-cycles-other", TOTAL_OFFSET + 5),
];
const CAPACITY_KEY_TABLE: [(&[u8], usize); 18] = [
    (b"drm-engine-capacity-render", 0),
    (b"drm-engine-capacity-gfx", 0),
    (b"drm-engine-capacity-compute", 1),
    (b"drm-engine-capacity-dma", 2),
    (b"drm-engine-capacity-dec", 3),
    (b"drm-engine-capacity-enc", 4),
    (b"drm-engine-capacity-enc_1", 5),
    (b"drm-engine-capacity-jpeg", 6),
    (b"drm-engine-capacity-vpe", 7),
    (b"drm-engine-capacity-copy", 2),
    (b"drm-engine-capacity-video", 3),
    (b"drm-engine-capacity-video-enhance", 6),
    (b"drm-engine-capacity-rcs", 0),
    (b"drm-engine-capacity-bcs", 1),
    (b"drm-engine-capacity-vcs", 2),
    (b"drm-engine-capacity-vecs", 3),
    (b"drm-engine-capacity-ccs", 4),
    (b"drm-engine-capacity-other", 5),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fdinfo {
    pub slots: [u64; SLOT_VALUES],
    pub present: u16,
    pub cycle_present: u16,
    pub capacity: [u64; ENGINE_SLOTS],
    pub capacity_present: u16,
    pub client_id: Option<u64>,
    pub pdev: [u8; 32],
    pub pdev_len: u8,
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
        } else if let Some((_, index)) = CAPACITY_KEY_TABLE.iter().find(|(known, _)| *known == key)
            && let Some(number) = parse_quantity(value)
        {
            result.capacity[*index] = number;
            result.capacity_present |= 1 << index;
        } else if let Some((_, index)) = KEY_TABLE.iter().find(|(known, _)| *known == key)
            && let Some(number) = parse_quantity(value)
        {
            result.slots[*index] = number;
            result.present |= 1 << index;
            if key.starts_with(b"drm-cycles-") {
                result.cycle_present |= 1 << index;
            }
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

fn parse_quantity(input: &[u8]) -> Option<u64> {
    let digits = input
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let suffix = &input[digits..];
    if !suffix.is_empty() && suffix != b" ns" && suffix != b" KiB" {
        return None;
    }
    parse_u64(&input[..digits])
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Baseline {
    pub start_time: u64,
    pub wall_ns: u64,
    pub slots: [u64; SLOT_VALUES],
    pub present: u16,
    pub cycle_present: u16,
    pub capacity: [u64; ENGINE_SLOTS],
    pub capacity_present: u16,
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
            cycle_present: current.cycle_present,
            capacity: current.capacity,
            capacity_present: current.capacity_present,
        });
        return None;
    };
    if old.start_time != start_time
        || wall_ns <= old.wall_ns
        || (0..SLOT_VALUES).any(|i| current.slots[i] < old.slots[i])
        || current.cycle_present != old.cycle_present
        || (0..ENGINE_SLOTS).any(|i| current.capacity[i] < old.capacity[i])
    {
        *previous = Some(Baseline {
            start_time,
            wall_ns,
            slots: current.slots,
            present: current.present,
            cycle_present: current.cycle_present,
            capacity: current.capacity,
            capacity_present: current.capacity_present,
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
        let paired_total = index + TOTAL_OFFSET;
        let divisor = if current.cycle_present & (1 << index) != 0 {
            if current.present & (1 << paired_total) == 0 || old.present & (1 << paired_total) == 0
            {
                continue;
            }
            current.slots[paired_total] - old.slots[paired_total]
        } else {
            elapsed
        };
        if divisor == 0 {
            continue;
        }
        let capacity = if current.capacity_present & (1 << index) != 0 {
            current.capacity[index].max(1)
        } else {
            1
        };
        result.util[index] = (amount as f64 / divisor as f64 / capacity as f64).clamp(0.0, 1.0);
        result.valid |= 1 << index;
    }
    *previous = Some(Baseline {
        start_time,
        wall_ns,
        slots: current.slots,
        present: current.present,
        cycle_present: current.cycle_present,
        capacity: current.capacity,
        capacity_present: current.capacity_present,
    });
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::{Baseline, Fdinfo, delta, parse};

    #[test]
    fn parses_driver_keys_with_kernel_units_and_device_identity() {
        let parsed = parse(
            b"drm-driver:\tamdgpu\ndrm-pdev:\t0000:03:00.0\ndrm-client-id:\t42\ndrm-engine-gfx:\t900 ns\n",
        );
        assert_eq!(
            &parsed.pdev[..usize::from(parsed.pdev_len)],
            b"0000:03:00.0"
        );
        assert_eq!(parsed.client_id, Some(42));
        assert_eq!(parsed.slots[0], 900);
        assert_ne!(parsed.present & 1, 0);
        assert_eq!(parse(b"drm-engine-gfx:\t4x\n").present, 0);
    }

    #[test]
    fn regression_and_pid_reuse_reprime_before_reporting() {
        let mut baseline = None;
        let mut current = Fdinfo {
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
                present: 1,
                cycle_present: 0,
                capacity: current.capacity,
                capacity_present: current.capacity_present,
            })
        );
    }

    // Kernel shapes follow amdgpu_fdinfo.c's amdgpu_ip_name table and the
    // DRM/Xe fdinfo docs: drm-engine values use ns; xe cycle values are bare.
    #[test]
    fn parses_kernel_amdgpu_i915_and_xe_shapes_and_pairs_xe_totals() {
        let amdgpu = parse(b"drm-driver:\tamdgpu\ndrm-engine-gfx:\t12 ns\ndrm-engine-dma:\t8 ns\n");
        let i915 =
            parse(b"drm-driver:\ti915\ndrm-engine-render:\t31 ns\ndrm-engine-video:\t7 ns\n");
        let xe = parse(
            b"drm-driver:\txe\ndrm-cycles-vcs:\t50\ndrm-total-cycles-vcs:\t100\ndrm-engine-capacity-vcs:\t2\n",
        );
        assert_eq!((amdgpu.slots[0], amdgpu.slots[2]), (12, 8));
        assert_eq!((i915.slots[0], i915.slots[3]), (31, 7));
        assert_eq!((xe.slots[2], xe.slots[10]), (50, 100));
        let mut previous = None;
        assert!(delta(&mut previous, 1, 100, &xe).is_none());
        let next = parse(
            b"drm-driver:\txe\ndrm-cycles-vcs:\t70\ndrm-total-cycles-vcs:\t200\ndrm-engine-capacity-vcs:\t2\n",
        );
        let sample = delta(&mut previous, 1, 200, &next).unwrap();
        assert!((sample.util[2] - 0.1).abs() < 0.000_001);
    }
}
