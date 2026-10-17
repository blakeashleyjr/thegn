//! Pure, bounds-checked decoder for the Linux amdgpu `gpu_metrics` ABI.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub structure_size: u16,
    pub format: u8,
    pub content: u8,
}

pub fn parse_header(buf: &[u8]) -> Option<Header> {
    let bytes: [u8; 4] = buf.get(..4)?.try_into().ok()?;
    Some(Header {
        structure_size: u16::from_le_bytes([bytes[0], bytes[1]]),
        format: bytes[2],
        content: bytes[3],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoded {
    Full,
    Truncated,
    Unsupported,
    NoHeader,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Unverified {
    pub verified: bool,
    pub dram_reads: Option<u16>,
    pub dram_writes: Option<u16>,
    pub ipu_reads: Option<u16>,
    pub ipu_writes: Option<u16>,
    pub throttle_residency: [Option<u32>; 7],
}

/// Normalized fields; temperatures are centi-Celsius, power is milliwatts,
/// clocks are MHz, and activity fields retain the kernel's percentage units.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpuMetrics {
    pub header: Option<Header>,
    pub layout_verified: bool,
    pub temperature_gfx: Option<u16>,
    pub temperature_soc: Option<u16>,
    pub temperature_skin: Option<u16>,
    pub temperature_edge: Option<u16>,
    pub temperature_hotspot: Option<u16>,
    pub temperature_mem: Option<u16>,
    pub temperature_vr_gfx: Option<u16>,
    pub temperature_vr_soc: Option<u16>,
    pub temperature_vr_mem: Option<u16>,
    pub temperature_core: [Option<u16>; 16],
    pub temperature_l3: [Option<u16>; 2],
    pub temperature_hbm: [Option<u16>; 4],
    pub activity_gfx: Option<u16>,
    pub activity_mm: Option<u16>,
    pub activity_vcn: Option<u16>,
    pub activity_umc: Option<u16>,
    pub activity_ipu: [Option<u16>; 8],
    pub activity_core_c0: [Option<u16>; 16],
    pub power_socket: Option<u32>,
    pub power_apu: Option<u32>,
    pub power_cpu: Option<u32>,
    pub power_soc: Option<u32>,
    pub power_gfx: Option<u32>,
    pub power_dgpu: Option<u32>,
    pub power_ipu: Option<u32>,
    pub power_all_core: Option<u32>,
    pub power_sys: Option<u32>,
    pub power_stapm_limit: Option<u32>,
    pub power_current_stapm_limit: Option<u32>,
    pub power_core: [Option<u32>; 16],
    pub avg_gfxclk: Option<u16>,
    pub avg_socclk: Option<u16>,
    pub avg_uclk: Option<u16>,
    pub avg_fclk: Option<u16>,
    pub avg_vclk: Option<u16>,
    pub avg_dclk: Option<u16>,
    pub current_gfxclk: Option<u16>,
    pub current_socclk: Option<u16>,
    pub current_uclk: Option<u16>,
    pub current_fclk: Option<u16>,
    pub current_vclk: Option<u16>,
    pub current_dclk: Option<u16>,
    pub current_vpeclk: Option<u16>,
    pub current_ipuclk: Option<u16>,
    pub current_coreclk: [Option<u16>; 16],
    pub current_l3clk: [Option<u16>; 2],
    pub avg_vpeclk: Option<u16>,
    pub avg_ipuclk: Option<u16>,
    pub core_maxfreq: Option<u16>,
    pub gfx_maxfreq: Option<u16>,
    pub throttle_status: Option<u32>,
    pub indep_throttle_status: Option<u64>,
    pub fan_rpm: Option<u16>,
    pub fan_pwm: Option<u16>,
    pub pcie_link_width: Option<u16>,
    pub pcie_link_speed: Option<u16>,
    pub voltage_soc: Option<u16>,
    pub voltage_gfx: Option<u16>,
    pub voltage_mem: Option<u16>,
    pub system_clock_counter: Option<u64>,
    pub alpha: Option<u32>,
    pub unverified: Unverified,
}

fn rd_u16(buf: &[u8], limit: usize, off: usize) -> Option<u16> {
    let end = off.checked_add(2)?;
    let bytes: [u8; 2] = buf.get(off..end.min(limit))?.try_into().ok()?;
    let value = u16::from_le_bytes(bytes);
    (value != u16::MAX).then_some(value)
}
fn rd_current_coreclk(buf: &[u8], limit: usize, off: usize) -> Option<u16> {
    rd_u16(buf, limit, off).filter(|value| *value != 0)
}
fn rd_u8(buf: &[u8], limit: usize, off: usize) -> Option<u8> {
    if off < limit {
        buf.get(off).copied()
    } else {
        None
    }
}
fn rd_u32(buf: &[u8], limit: usize, off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let bytes: [u8; 4] = buf.get(off..end.min(limit))?.try_into().ok()?;
    let value = u32::from_le_bytes(bytes);
    (value != u32::MAX).then_some(value)
}
fn rd_u64(buf: &[u8], limit: usize, off: usize) -> Option<u64> {
    let end = off.checked_add(8)?;
    let bytes: [u8; 8] = buf.get(off..end.min(limit))?.try_into().ok()?;
    let value = u64::from_le_bytes(bytes);
    (value != u64::MAX).then_some(value)
}
fn temp(buf: &[u8], limit: usize, off: usize, scale: u16) -> Option<u16> {
    rd_u16(buf, limit, off)?.checked_mul(scale)
}
fn power_u16(buf: &[u8], limit: usize, off: usize, scale: u32) -> Option<u32> {
    u32::from(rd_u16(buf, limit, off)?).checked_mul(scale)
}

/// Decode one `gpu_metrics` read. The target is reset even for unsupported or
/// malformed input, so values from a prior revision can never leak forward.
pub fn decode(buf: &[u8], target: &mut GpuMetrics) -> Decoded {
    *target = GpuMetrics::default();
    let Some(header) = parse_header(buf) else {
        return Decoded::NoHeader;
    };
    target.header = Some(header);
    let limit = usize::from(header.structure_size).min(buf.len());
    let truncated = usize::from(header.structure_size) > buf.len();
    let result = match (header.format, header.content) {
        (1, 0) => decode_v1_0(buf, limit, target),
        (1, 1..=3) => decode_v1(buf, limit, header.content, target),
        (2, 0) => decode_v2_0(buf, limit, target),
        (2, 1..=2) => decode_v2_1(buf, limit, header.format, header.content, target),
        (3, 0) => decode_v3(buf, limit, target),
        _ => return Decoded::Unsupported,
    };
    if !result {
        return Decoded::Unsupported;
    }
    if truncated {
        Decoded::Truncated
    } else {
        Decoded::Full
    }
}

fn decode_v1_0(buf: &[u8], limit: usize, m: &mut GpuMetrics) -> bool {
    m.layout_verified = false;
    m.system_clock_counter = rd_u64(buf, limit, 8);
    m.temperature_edge = temp(buf, limit, 16, 100);
    m.temperature_hotspot = temp(buf, limit, 18, 100);
    m.temperature_mem = temp(buf, limit, 20, 100);
    m.temperature_vr_gfx = temp(buf, limit, 22, 100);
    m.temperature_vr_soc = temp(buf, limit, 24, 100);
    m.temperature_vr_mem = temp(buf, limit, 26, 100);
    m.activity_gfx = rd_u16(buf, limit, 28);
    m.activity_umc = rd_u16(buf, limit, 30);
    m.activity_mm = rd_u16(buf, limit, 32);
    m.power_socket = power_u16(buf, limit, 34, 1000);
    m.avg_gfxclk = rd_u16(buf, limit, 40);
    m.avg_socclk = rd_u16(buf, limit, 42);
    m.avg_uclk = rd_u16(buf, limit, 44);
    m.avg_vclk = rd_u16(buf, limit, 46).or_else(|| rd_u16(buf, limit, 50));
    m.avg_dclk = rd_u16(buf, limit, 48).or_else(|| rd_u16(buf, limit, 52));
    m.current_gfxclk = rd_u16(buf, limit, 54);
    m.current_socclk = rd_u16(buf, limit, 56);
    m.current_uclk = rd_u16(buf, limit, 58);
    m.current_vclk = rd_u16(buf, limit, 60).or_else(|| rd_u16(buf, limit, 64));
    m.current_dclk = rd_u16(buf, limit, 62).or_else(|| rd_u16(buf, limit, 66));
    m.throttle_status = rd_u32(buf, limit, 68);
    m.fan_rpm = rd_u16(buf, limit, 72);
    m.pcie_link_width = rd_u8(buf, limit, 74).map(u16::from);
    m.pcie_link_speed = rd_u8(buf, limit, 75).map(u16::from);
    true
}

fn decode_v1(buf: &[u8], limit: usize, revision: u8, m: &mut GpuMetrics) -> bool {
    m.layout_verified = false;
    m.temperature_edge = temp(buf, limit, 4, 100);
    m.temperature_hotspot = temp(buf, limit, 6, 100);
    m.temperature_mem = temp(buf, limit, 8, 100);
    m.temperature_vr_gfx = temp(buf, limit, 10, 100);
    m.temperature_vr_soc = temp(buf, limit, 12, 100);
    m.temperature_vr_mem = temp(buf, limit, 14, 100);
    m.activity_gfx = rd_u16(buf, limit, 16);
    m.activity_umc = rd_u16(buf, limit, 18);
    m.activity_mm = rd_u16(buf, limit, 20);
    m.power_socket = power_u16(buf, limit, 22, 1000);
    m.system_clock_counter = rd_u64(buf, limit, 32);
    m.avg_gfxclk = rd_u16(buf, limit, 40);
    m.avg_socclk = rd_u16(buf, limit, 42);
    m.avg_uclk = rd_u16(buf, limit, 44);
    m.avg_vclk = rd_u16(buf, limit, 46).or_else(|| rd_u16(buf, limit, 50));
    m.avg_dclk = rd_u16(buf, limit, 48).or_else(|| rd_u16(buf, limit, 52));
    m.current_gfxclk = rd_u16(buf, limit, 54);
    m.current_socclk = rd_u16(buf, limit, 56);
    m.current_uclk = rd_u16(buf, limit, 58);
    m.current_vclk = rd_u16(buf, limit, 60).or_else(|| rd_u16(buf, limit, 64));
    m.current_dclk = rd_u16(buf, limit, 62).or_else(|| rd_u16(buf, limit, 66));
    m.throttle_status = rd_u32(buf, limit, 68);
    m.fan_rpm = rd_u16(buf, limit, 72);
    m.pcie_link_width = rd_u16(buf, limit, 74);
    m.pcie_link_speed = rd_u16(buf, limit, 76);
    if revision >= 1 {
        for (index, value) in m.temperature_hbm.iter_mut().enumerate() {
            *value = temp(buf, limit, 88 + index * 2, 100);
        }
    }
    if revision >= 3 {
        m.voltage_soc = rd_u16(buf, limit, 104);
        m.voltage_gfx = rd_u16(buf, limit, 106);
        m.voltage_mem = rd_u16(buf, limit, 108);
        m.indep_throttle_status = rd_u64(buf, limit, 112);
    }
    true
}

fn decode_v2_0(buf: &[u8], limit: usize, m: &mut GpuMetrics) -> bool {
    m.layout_verified = false;
    m.system_clock_counter = rd_u64(buf, limit, 8);
    m.temperature_gfx = rd_u16(buf, limit, 16);
    m.temperature_soc = rd_u16(buf, limit, 18);
    for (i, value) in m.temperature_core[..8].iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 20 + i * 2);
    }
    for (i, value) in m.temperature_l3.iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 36 + i * 2);
    }
    m.activity_gfx = rd_u16(buf, limit, 40);
    m.activity_mm = rd_u16(buf, limit, 42);
    m.power_socket = rd_u16(buf, limit, 44).map(u32::from);
    m.power_cpu = rd_u16(buf, limit, 46).map(u32::from);
    m.power_soc = rd_u16(buf, limit, 48).map(u32::from);
    m.power_gfx = rd_u16(buf, limit, 50).map(u32::from);
    for (i, value) in m.power_core[..8].iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 52 + i * 2).map(u32::from);
    }
    m.avg_gfxclk = rd_u16(buf, limit, 68);
    m.avg_socclk = rd_u16(buf, limit, 70);
    m.avg_uclk = rd_u16(buf, limit, 72);
    m.avg_fclk = rd_u16(buf, limit, 74);
    m.avg_vclk = rd_u16(buf, limit, 76);
    m.avg_dclk = rd_u16(buf, limit, 78);
    m.current_gfxclk = rd_u16(buf, limit, 80);
    m.current_socclk = rd_u16(buf, limit, 82);
    m.current_uclk = rd_u16(buf, limit, 84);
    m.current_fclk = rd_u16(buf, limit, 86);
    m.current_vclk = rd_u16(buf, limit, 88);
    m.current_dclk = rd_u16(buf, limit, 90);
    for (i, value) in m.current_coreclk[..8].iter_mut().enumerate() {
        *value = rd_current_coreclk(buf, limit, 92 + i * 2);
    }
    for (i, value) in m.current_l3clk.iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 108 + i * 2);
    }
    m.throttle_status = rd_u32(buf, limit, 112);
    m.fan_pwm = rd_u16(buf, limit, 116);
    true
}

fn decode_v2_1(buf: &[u8], limit: usize, _format: u8, content: u8, m: &mut GpuMetrics) -> bool {
    m.layout_verified = false;
    m.temperature_gfx = rd_u16(buf, limit, 4);
    m.temperature_soc = rd_u16(buf, limit, 6);
    for (i, value) in m.temperature_core[..8].iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 8 + i * 2);
    }
    for (i, value) in m.temperature_l3.iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 24 + i * 2);
    }
    m.activity_gfx = rd_u16(buf, limit, 28);
    m.activity_mm = rd_u16(buf, limit, 30);
    m.system_clock_counter = rd_u64(buf, limit, 32);
    m.power_socket = rd_u16(buf, limit, 40).map(u32::from);
    m.power_cpu = rd_u16(buf, limit, 42).map(u32::from);
    m.power_soc = rd_u16(buf, limit, 44).map(u32::from);
    m.power_gfx = rd_u16(buf, limit, 46).map(u32::from);
    for (i, value) in m.power_core[..8].iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 48 + i * 2).map(u32::from);
    }
    m.avg_gfxclk = rd_u16(buf, limit, 64);
    m.avg_socclk = rd_u16(buf, limit, 66);
    m.avg_uclk = rd_u16(buf, limit, 68);
    m.avg_fclk = rd_u16(buf, limit, 70);
    m.avg_vclk = rd_u16(buf, limit, 72);
    m.avg_dclk = rd_u16(buf, limit, 74);
    m.current_gfxclk = rd_u16(buf, limit, 76);
    m.current_socclk = rd_u16(buf, limit, 78);
    m.current_uclk = rd_u16(buf, limit, 80);
    m.current_fclk = rd_u16(buf, limit, 82);
    m.current_vclk = rd_u16(buf, limit, 84);
    m.current_dclk = rd_u16(buf, limit, 86);
    for (i, value) in m.current_coreclk[..8].iter_mut().enumerate() {
        *value = rd_current_coreclk(buf, limit, 88 + i * 2);
    }
    for (i, value) in m.current_l3clk.iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 104 + i * 2);
    }
    m.throttle_status = rd_u32(buf, limit, 108);
    m.fan_pwm = rd_u16(buf, limit, 112);
    if content == 2 {
        m.indep_throttle_status = rd_u64(buf, limit, 120);
    }
    true
}

fn decode_v3(buf: &[u8], limit: usize, m: &mut GpuMetrics) -> bool {
    m.layout_verified = true;
    m.temperature_gfx = rd_u16(buf, limit, 4);
    m.temperature_soc = rd_u16(buf, limit, 6);
    for (i, value) in m.temperature_core.iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 8 + i * 2);
    }
    m.temperature_skin = rd_u16(buf, limit, 40);
    m.activity_gfx = rd_u16(buf, limit, 42);
    m.activity_vcn = rd_u16(buf, limit, 44);
    for (i, value) in m.activity_ipu.iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 46 + i * 2);
    }
    for (i, value) in m.activity_core_c0.iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 62 + i * 2);
    }
    m.unverified.dram_reads = rd_u16(buf, limit, 94);
    m.unverified.dram_writes = rd_u16(buf, limit, 96);
    m.unverified.ipu_reads = rd_u16(buf, limit, 98);
    m.unverified.ipu_writes = rd_u16(buf, limit, 100);
    m.system_clock_counter = rd_u64(buf, limit, 104);
    m.power_socket = rd_u32(buf, limit, 112);
    m.power_ipu = rd_u16(buf, limit, 116).map(u32::from);
    m.power_apu = rd_u32(buf, limit, 120);
    m.power_gfx = rd_u32(buf, limit, 124);
    m.power_dgpu = rd_u32(buf, limit, 128);
    m.power_all_core = rd_u32(buf, limit, 132);
    for (i, value) in m.power_core.iter_mut().enumerate() {
        *value = rd_u16(buf, limit, 136 + i * 2).map(u32::from);
    }
    m.power_sys = rd_u16(buf, limit, 168).map(u32::from);
    m.power_stapm_limit = rd_u16(buf, limit, 170).map(u32::from);
    m.power_current_stapm_limit = rd_u16(buf, limit, 172).map(u32::from);
    m.avg_gfxclk = rd_u16(buf, limit, 174);
    m.avg_socclk = rd_u16(buf, limit, 176);
    m.avg_vpeclk = rd_u16(buf, limit, 178);
    m.avg_ipuclk = rd_u16(buf, limit, 180);
    m.avg_fclk = rd_u16(buf, limit, 182);
    m.avg_vclk = rd_u16(buf, limit, 184);
    m.avg_uclk = rd_u16(buf, limit, 186);
    m.current_coreclk = [None; 16];
    for (i, value) in m.current_coreclk.iter_mut().enumerate() {
        *value = rd_current_coreclk(buf, limit, 190 + i * 2);
    }
    m.core_maxfreq = rd_u16(buf, limit, 222);
    m.gfx_maxfreq = rd_u16(buf, limit, 224);
    for (i, value) in m.unverified.throttle_residency.iter_mut().enumerate() {
        *value = rd_u32(buf, limit, 228 + i * 4);
    }
    m.alpha = rd_u32(buf, limit, 256);
    true
}

#[cfg(test)]
mod tests {
    use super::{Decoded, GpuMetrics, decode};
    const V3_0_BLOB: [u8; 264] = [
        0x08, 0x01, 0x03, 0x00, 0x20, 0x1e, 0xbe, 0x21, 0x28, 0x21, 0xef, 0x1f, 0xfc, 0x21, 0xef,
        0x1f, 0x1c, 0x23, 0xef, 0x1f, 0x2e, 0x22, 0xef, 0x1f, 0xca, 0x21, 0xaa, 0x20, 0x66, 0x21,
        0xa4, 0x21, 0x08, 0x22, 0x6c, 0x22, 0x98, 0x21, 0xea, 0x22, 0x05, 0x14, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x50, 0x00, 0x00, 0x00, 0x50, 0x00, 0x00, 0x00, 0x53, 0x00, 0x00, 0x00, 0x54,
        0x00, 0x00, 0x00, 0x41, 0x00, 0x35, 0x00, 0x3b, 0x00, 0x3e, 0x00, 0x47, 0x00, 0x39, 0x00,
        0x41, 0x00, 0x33, 0x00, 0xf1, 0x2a, 0x91, 0x0e, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0x67,
        0xd9, 0xac, 0x02, 0x91, 0x4c, 0x02, 0x00, 0x11, 0x51, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff,
        0x11, 0x51, 0x00, 0x00, 0x99, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x97, 0x2e, 0x00,
        0x00, 0xa1, 0x03, 0x00, 0x00, 0x57, 0x03, 0x00, 0x00, 0x60, 0x03, 0x00, 0x00, 0xfd, 0x02,
        0x00, 0x00, 0x21, 0x06, 0x58, 0x01, 0x24, 0x03, 0xfc, 0x05, 0x4d, 0x04, 0x60, 0x02, 0xb2,
        0x01, 0xaa, 0x04, 0xb5, 0x91, 0xff, 0xff, 0xff, 0xff, 0x6b, 0x02, 0x5e, 0x02, 0x20, 0x03,
        0x00, 0x00, 0xa7, 0x07, 0x20, 0x03, 0xf0, 0x0a, 0x00, 0x00, 0x58, 0x02, 0x00, 0x00, 0x58,
        0x02, 0x00, 0x00, 0x58, 0x02, 0x00, 0x00, 0x58, 0x02, 0x00, 0x00, 0xf0, 0x0a, 0xf0, 0x0a,
        0xf0, 0x0a, 0xf0, 0x0a, 0xf0, 0x0a, 0xf0, 0x0a, 0xf0, 0x0a, 0xf0, 0x0a, 0xb0, 0x04, 0x54,
        0x0b, 0xff, 0xff, 0xcd, 0x00, 0x00, 0x00, 0xdb, 0x8e, 0x7f, 0x00, 0xef, 0x6c, 0x5a, 0x00,
        0x6d, 0x82, 0x04, 0x00, 0x06, 0x00, 0xbc, 0x0d, 0x74, 0x00, 0x03, 0x00, 0xab, 0xef, 0x19,
        0x00, 0x40, 0x42, 0x0f, 0x00, 0xff, 0xff, 0xff, 0xff,
    ];

    #[test]
    fn captured_v3_blob_decodes_values_and_ignores_padding() {
        let mut metrics = GpuMetrics::default();
        assert_eq!(decode(&V3_0_BLOB, &mut metrics), Decoded::Full);
        assert!(metrics.layout_verified);
        assert_eq!(metrics.temperature_gfx, Some(7712));
        assert_eq!(metrics.temperature_soc, Some(8638));
        assert_eq!(metrics.temperature_skin, Some(5125));
        assert_eq!(
            metrics.temperature_core,
            [
                Some(8488),
                Some(8175),
                Some(8700),
                Some(8175),
                Some(8988),
                Some(8175),
                Some(8750),
                Some(8175),
                Some(8650),
                Some(8362),
                Some(8550),
                Some(8612),
                Some(8712),
                Some(8812),
                Some(8600),
                Some(8938)
            ]
        );
        assert_eq!(metrics.power_socket, Some(20753));
        assert_eq!(metrics.power_apu, Some(20753));
        assert_eq!(metrics.power_gfx, Some(409));
        assert_eq!(metrics.power_all_core, Some(11927));
        assert_eq!(metrics.power_sys, Some(37301));
        assert_eq!(metrics.power_core[0], Some(929));
        assert_eq!(metrics.power_core[8], Some(1569));
        assert_eq!(metrics.power_stapm_limit, None);
        assert_eq!(metrics.power_current_stapm_limit, None);
        assert_eq!(metrics.avg_gfxclk, Some(619));
        assert_eq!(metrics.avg_fclk, Some(1959));
        assert_eq!(metrics.avg_uclk, Some(2800));
        assert_eq!(metrics.current_gfxclk, None);
        assert_eq!(metrics.gfx_maxfreq, Some(2900));
        assert_eq!(metrics.core_maxfreq, Some(1200));
        assert_eq!(metrics.current_coreclk[8], Some(2800));
        assert_eq!(metrics.unverified.dram_reads, Some(10993));
        assert_eq!(metrics.unverified.dram_writes, Some(3729));
        assert_eq!(metrics.unverified.throttle_residency[0], Some(205));
        assert_eq!(metrics.unverified.throttle_residency[4], Some(230424582));
        assert_eq!(metrics.system_clock_counter, Some(647135652272487));
        assert_eq!(metrics.alpha, Some(1_000_000));
        assert!(!metrics.unverified.verified);
        assert_eq!(metrics.header.unwrap().format, 3);
        let expected = metrics.clone();
        let mut padded = V3_0_BLOB;
        for offset in [102, 118, 226, 260, 261, 262, 263] {
            padded[offset] = 0x12;
        }
        assert_eq!(decode(&padded, &mut metrics), Decoded::Full);
        assert_eq!(metrics, expected);
    }

    #[test]
    fn truncation_unknown_headers_and_all_prefixes_are_safe() {
        let mut metrics = GpuMetrics::default();
        assert_eq!(decode(&V3_0_BLOB[..100], &mut metrics), Decoded::Truncated);
        assert_eq!(metrics.temperature_gfx, Some(7712));
        assert_eq!(metrics.power_socket, None);
        assert_eq!(metrics.alpha, None);
        for prefix in 0..=V3_0_BLOB.len() {
            let _decoded = decode(&V3_0_BLOB[..prefix], &mut metrics);
        }
        assert_eq!(decode(&V3_0_BLOB[..3], &mut metrics), Decoded::NoHeader);
        let mut unknown = V3_0_BLOB;
        unknown[2] = 9;
        assert_eq!(decode(&unknown, &mut metrics), Decoded::Unsupported);
        let header = metrics.header;
        assert_eq!(header.unwrap().format, 9);
        metrics.header = None;
        assert_eq!(metrics, GpuMetrics::default());
        let mut future = V3_0_BLOB;
        future[3] = 1;
        assert_eq!(decode(&future, &mut metrics), Decoded::Unsupported);
        metrics.header = None;
        assert_eq!(metrics, GpuMetrics::default());
    }

    fn put16(buf: &mut [u8], off: usize, value: u16) {
        buf[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put64(buf: &mut [u8], off: usize, value: u64) {
        buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn legacy_revisions_normalize_units_and_reset_previous_fields() {
        let mut v13 = [0_u8; 120];
        v13[0..4].copy_from_slice(&[120, 0, 1, 3]);
        put16(&mut v13, 4, 65);
        put16(&mut v13, 22, 180);
        put64(&mut v13, 32, 77);
        put16(&mut v13, 72, 1234);
        put16(&mut v13, 104, 900);
        put64(&mut v13, 112, 0x1234);
        let mut metrics = GpuMetrics::default();
        assert_eq!(decode(&v13, &mut metrics), Decoded::Full);
        assert!(!metrics.layout_verified);
        assert_eq!(metrics.temperature_edge, Some(6500));
        assert_eq!(metrics.power_socket, Some(180000));
        assert_eq!(metrics.fan_rpm, Some(1234));
        assert_eq!(metrics.indep_throttle_status, Some(0x1234));
        assert_eq!(metrics.voltage_soc, Some(900));
        assert_eq!(decode(&V3_0_BLOB, &mut metrics), Decoded::Full);
        assert_eq!(decode(&v13, &mut metrics), Decoded::Full);
        assert_eq!(metrics.temperature_skin, None);
        assert_eq!(metrics.temperature_core[0], None);
        let mut v10 = [0_u8; 80];
        v10[0..4].copy_from_slice(&[80, 0, 1, 0]);
        put16(&mut v10, 4, 999);
        put64(&mut v10, 8, 0x1234);
        put16(&mut v10, 16, 65);
        v10[74] = u8::MAX;
        v10[75] = 7;
        assert_eq!(decode(&v10, &mut metrics), Decoded::Full);
        assert_eq!(metrics.temperature_edge, Some(6500));
        assert_eq!(metrics.system_clock_counter, Some(0x1234));
        assert_ne!(metrics.temperature_edge, Some(999));
        assert_eq!(metrics.pcie_link_width, Some(255));
        assert_eq!(metrics.pcie_link_speed, Some(7));
        assert_eq!(metrics.temperature_hbm, [None; 4]);
        assert_eq!(metrics.voltage_soc, None);
        let mut v22 = [0_u8; 128];
        v22[0..4].copy_from_slice(&[128, 0, 2, 2]);
        put16(&mut v22, 4, 55);
        put16(&mut v22, 40, 700);
        put64(&mut v22, 120, 0x55);
        assert_eq!(decode(&v22, &mut metrics), Decoded::Full);
        assert_eq!(metrics.temperature_gfx, Some(55));
        assert_eq!(metrics.power_socket, Some(700));
        assert_eq!(metrics.indep_throttle_status, Some(0x55));
    }

    #[test]
    fn l3_clocks_are_separate_and_v3_frequencies_are_averages() {
        let mut v20 = [0_u8; 128];
        v20[0..4].copy_from_slice(&[128, 0, 2, 0]);
        put16(&mut v20, 108, 1400);
        put16(&mut v20, 110, 1500);
        let mut metrics = GpuMetrics::default();
        assert_eq!(decode(&v20, &mut metrics), Decoded::Full);
        assert_eq!(metrics.current_l3clk, [Some(1400), Some(1500)]);
        assert_eq!(metrics.current_coreclk, [None; 16]);

        let mut v21 = [0_u8; 120];
        v21[0..4].copy_from_slice(&[120, 0, 2, 1]);
        put16(&mut v21, 104, 1600);
        put16(&mut v21, 106, 1700);
        assert_eq!(decode(&v21, &mut metrics), Decoded::Full);
        assert_eq!(metrics.current_l3clk, [Some(1600), Some(1700)]);
        assert_eq!(metrics.current_coreclk, [None; 16]);

        assert_eq!(decode(&V3_0_BLOB, &mut metrics), Decoded::Full);
        assert_eq!(metrics.avg_gfxclk, Some(619));
        assert_eq!(metrics.avg_vpeclk, Some(800));
        assert_eq!(metrics.avg_ipuclk, Some(0));
        assert_eq!(metrics.avg_fclk, Some(1959));
        assert_eq!(metrics.avg_vclk, Some(800));
        assert_eq!(metrics.avg_uclk, Some(2800));
        assert_eq!(metrics.current_gfxclk, None);
    }
}
