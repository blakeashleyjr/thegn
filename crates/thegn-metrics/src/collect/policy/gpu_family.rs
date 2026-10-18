//! Conservative GPU family and device-kind classification.

use crate::collect::model::gpu::{GpuKind, IpVersion};

pub const VRAM_APU_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
pub const GTT_APU_MINIMUM: u64 = 8 * 1024 * 1024 * 1024;

pub fn family_for(
    driver: Option<&str>,
    ip_version: Option<IpVersion>,
    pci_id: Option<&str>,
) -> Option<&'static str> {
    match driver? {
        "amdgpu" => match ip_version? {
            IpVersion {
                major: 11,
                minor: 5,
                ..
            } => Some("RDNA3.5"),
            IpVersion {
                major: 11,
                minor: 0,
                ..
            } => Some("RDNA3"),
            IpVersion {
                major: 10,
                minor: 3,
                ..
            } => Some("RDNA2"),
            IpVersion { major: 9, .. } => Some("GCN5/Vega"),
            _ => None,
        },
        "i915" | "xe" => Some(intel_family(pci_id)),
        "nvidia" => Some("NVIDIA"),
        "virtio_gpu" => Some("virtual"),
        _ => None,
    }
}

fn intel_family(_pci_id: Option<&str>) -> &'static str {
    // The table is intentionally empty until device ranges have a verified generation mapping.
    "Intel"
}

pub fn kind_for(
    driver: Option<&str>,
    has_uma: bool,
    vram_total: Option<u64>,
    gtt_total: Option<u64>,
    slot: Option<&str>,
) -> GpuKind {
    match driver {
        Some("virtio_gpu") => GpuKind::Virtual,
        Some("nvidia") => GpuKind::Dgpu,
        Some("i915" | "xe") if slot.is_some_and(|slot| slot.ends_with("00:02.0")) => GpuKind::Igpu,
        Some("amdgpu")
            if has_uma
                || (vram_total.is_some_and(|vram| vram <= VRAM_APU_LIMIT)
                    && gtt_total.is_some_and(|gtt| gtt >= GTT_APU_MINIMUM)) =>
        {
            GpuKind::Apu
        }
        Some("amdgpu" | "i915" | "xe") => GpuKind::Dgpu,
        _ => GpuKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::{GTT_APU_MINIMUM, VRAM_APU_LIMIT, family_for, kind_for};
    use crate::collect::model::gpu::{GpuKind, IpVersion};

    #[test]
    fn family_table_is_conservative() {
        for (version, expected) in [
            (
                IpVersion {
                    major: 11,
                    minor: 5,
                    revision: 0,
                },
                Some("RDNA3.5"),
            ),
            (
                IpVersion {
                    major: 11,
                    minor: 0,
                    revision: 0,
                },
                Some("RDNA3"),
            ),
            (
                IpVersion {
                    major: 10,
                    minor: 3,
                    revision: 0,
                },
                Some("RDNA2"),
            ),
            (
                IpVersion {
                    major: 9,
                    minor: 0,
                    revision: 0,
                },
                Some("GCN5/Vega"),
            ),
            (
                IpVersion {
                    major: 12,
                    minor: 0,
                    revision: 0,
                },
                None,
            ),
        ] {
            assert_eq!(family_for(Some("amdgpu"), Some(version), None), expected);
        }
        assert_eq!(family_for(Some("i915"), None, None), Some("Intel"));
        assert_eq!(family_for(Some("xe"), None, None), Some("Intel"));
        assert_eq!(family_for(Some("nvidia"), None, None), Some("NVIDIA"));
        assert_eq!(family_for(Some("virtio_gpu"), None, None), Some("virtual"));
        assert_eq!(family_for(Some("unknown"), None, None), None);
    }

    #[test]
    fn kind_rules_include_exact_memory_thresholds() {
        assert_eq!(
            kind_for(
                Some("amdgpu"),
                false,
                Some(VRAM_APU_LIMIT),
                Some(GTT_APU_MINIMUM),
                None
            ),
            GpuKind::Apu
        );
        assert_eq!(
            kind_for(
                Some("amdgpu"),
                false,
                Some(VRAM_APU_LIMIT + 1),
                Some(GTT_APU_MINIMUM),
                None
            ),
            GpuKind::Dgpu
        );
        assert_eq!(
            kind_for(
                Some("amdgpu"),
                false,
                Some(VRAM_APU_LIMIT),
                Some(GTT_APU_MINIMUM - 1),
                None
            ),
            GpuKind::Dgpu
        );
        assert_eq!(
            kind_for(Some("amdgpu"), true, None, None, None),
            GpuKind::Apu
        );
        assert_eq!(
            kind_for(Some("i915"), false, None, None, Some("0000:00:02.0")),
            GpuKind::Igpu
        );
        assert_eq!(
            kind_for(Some("virtio_gpu"), false, None, None, None),
            GpuKind::Virtual
        );
        assert_eq!(
            kind_for(Some("nvidia"), false, None, None, None),
            GpuKind::Dgpu
        );
        assert_eq!(
            kind_for(Some("other"), false, None, None, None),
            GpuKind::Unknown
        );
    }
}
