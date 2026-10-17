//! Portable host facts supplied to collectors; discovery is owned by F3b.
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Env {
    pub os: String,
    pub in_container: bool,
    pub cgroup_root: Option<PathBuf>,
    pub cpus_effective: Option<u32>,
    pub cpu_max: Option<f64>,
    pub cap_perfmon: bool,
    pub cap_sys_admin: bool,
    pub page_size: u64,
    pub clk_tck: u64,
    pub cores: u32,
}
impl Default for Env {
    fn default() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            in_container: false,
            cgroup_root: None,
            cpus_effective: None,
            cpu_max: None,
            cap_perfmon: false,
            cap_sys_admin: false,
            page_size: 4096,
            clk_tck: 100,
            cores: 0,
        }
    }
}

pub fn parse_cap_eff(buf: &[u8]) -> Option<u64> {
    let line = buf
        .split(|byte| *byte == b'\n')
        .find(|line| line.starts_with(b"CapEff:"))?;
    let value = line.get(b"CapEff:".len()..)?.trim_ascii();
    let value = value
        .strip_prefix(b"0x")
        .or_else(|| value.strip_prefix(b"0X"))
        .unwrap_or(value);
    if value.is_empty() {
        return None;
    }
    value.iter().try_fold(0_u64, |acc, digit| {
        let nibble = match digit {
            b'0'..=b'9' => u64::from(*digit - b'0'),
            b'a'..=b'f' => u64::from(*digit - b'a' + 10),
            b'A'..=b'F' => u64::from(*digit - b'A' + 10),
            _ => return None,
        };
        acc.checked_mul(16)?.checked_add(nibble)
    })
}
pub const fn has_cap(mask: u64, bit: u8) -> bool {
    bit < 64 && mask & (1_u64 << bit) != 0
}
pub const CAP_SYS_ADMIN: u8 = 21;
pub const CAP_PERFMON: u8 = 38;

pub fn self_cgroup_v2(buf: &[u8]) -> Option<&[u8]> {
    buf.split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"0::"))
}

#[cfg(test)]
mod tests {
    use super::{CAP_PERFMON, CAP_SYS_ADMIN, Env, has_cap, parse_cap_eff, self_cgroup_v2};
    #[test]
    fn parses_capabilities_and_unified_cgroup_line() {
        assert_eq!(
            parse_cap_eff(b"CapInh:\t0\nCapEff:\t0000000000200000\n"),
            Some(1 << 21)
        );
        assert!(has_cap(
            (1 << CAP_SYS_ADMIN) | (1 << CAP_PERFMON),
            CAP_SYS_ADMIN
        ));
        assert!(has_cap(
            (1 << CAP_SYS_ADMIN) | (1 << CAP_PERFMON),
            CAP_PERFMON
        ));
        assert_eq!(
            self_cgroup_v2(b"2:cpu:/legacy\n0::/user.slice\n"),
            Some(&b"/user.slice"[..])
        );
        assert_eq!(self_cgroup_v2(b"2:cpu:/legacy\n"), None);
        assert_eq!(Env::default().page_size, 4096);
        assert_eq!(Env::default().clk_tck, 100);
    }
}
