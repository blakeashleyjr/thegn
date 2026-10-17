use super::{Hue, Palette, blend_over, contrast_ratio};

/// Monitor metric families used by continuous color ramps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RampId {
    Util,
    Temp,
    Power,
    Mem,
    Rx,
    Tx,
    Read,
    Write,
    Signal,
    Heat,
}

impl RampId {
    pub const ALL: [Self; 10] = [
        Self::Util,
        Self::Temp,
        Self::Power,
        Self::Mem,
        Self::Rx,
        Self::Tx,
        Self::Read,
        Self::Write,
        Self::Signal,
        Self::Heat,
    ];

    pub fn family(self) -> Hue {
        match self {
            Self::Util => Hue::Teal,
            Self::Temp => Hue::Blue,
            Self::Power | Self::Read => Hue::Amber,
            Self::Mem => Hue::Purple,
            Self::Rx | Self::Signal => Hue::Green,
            Self::Tx => Hue::Blue,
            Self::Write => Hue::Orange,
            Self::Heat => Hue::Green,
        }
    }

    fn band_family(self) -> Hue {
        match self {
            // Signal's green normal hue and red critical hue both quantize to
            // ANSI-16 index 8; teal keeps the threshold bands distinct.
            Self::Signal => Hue::Teal,
            id => id.family(),
        }
    }

    fn stops(self) -> &'static [Hue] {
        match self {
            Self::Util => &[Hue::Teal, Hue::Green, Hue::Amber, Hue::Orange, Hue::Red],
            Self::Temp => &[Hue::Blue, Hue::Teal, Hue::Amber, Hue::Red],
            Self::Power => &[Hue::Amber, Hue::Orange, Hue::Red],
            Self::Mem => &[Hue::Purple],
            Self::Rx => &[Hue::Green],
            Self::Tx => &[Hue::Blue],
            Self::Read => &[Hue::Amber],
            Self::Write => &[Hue::Orange],
            Self::Signal => &[Hue::Red, Hue::Amber, Hue::Green],
            Self::Heat => &[],
        }
    }
}

/// Threshold state for a banded monitor metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Band {
    Normal,
    Warn,
    Crit,
}

pub fn band_of(value: f32, warn: f32, crit: f32) -> Band {
    if value.is_nan() {
        Band::Normal
    } else if value >= crit {
        Band::Crit
    } else if value >= warn {
        Band::Warn
    } else {
        Band::Normal
    }
}

/// Named monitor series, keeping framework and driver colors consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeriesRole {
    CpuUser,
    CpuNice,
    CpuSys,
    CpuIrq,
    CpuIowait,
    CpuSteal,
    CpuGuest,
    CoreP,
    CoreE,
    MemAnon,
    MemCache,
    MemBuffers,
    MemShmem,
    MemSlab,
    MemKernel,
    MemFree,
    MemSwap,
    MemZram,
    MemDirty,
    EngGfx,
    EngCompute,
    EngVideo,
    EngCopy,
    EngOther,
    RailCore,
    RailGfx,
    RailSoc,
    RailRest,
    Cat0,
    Cat1,
    Cat2,
    Cat3,
    Cat4,
    Cat5,
    Selected,
    GhostOverlay,
}

impl SeriesRole {
    pub const ALL: [Self; 36] = [
        Self::CpuUser,
        Self::CpuNice,
        Self::CpuSys,
        Self::CpuIrq,
        Self::CpuIowait,
        Self::CpuSteal,
        Self::CpuGuest,
        Self::CoreP,
        Self::CoreE,
        Self::MemAnon,
        Self::MemCache,
        Self::MemBuffers,
        Self::MemShmem,
        Self::MemSlab,
        Self::MemKernel,
        Self::MemFree,
        Self::MemSwap,
        Self::MemZram,
        Self::MemDirty,
        Self::EngGfx,
        Self::EngCompute,
        Self::EngVideo,
        Self::EngCopy,
        Self::EngOther,
        Self::RailCore,
        Self::RailGfx,
        Self::RailSoc,
        Self::RailRest,
        Self::Cat0,
        Self::Cat1,
        Self::Cat2,
        Self::Cat3,
        Self::Cat4,
        Self::Cat5,
        Self::Selected,
        Self::GhostOverlay,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateRole {
    Ok,
    Warn,
    Crit,
    Stale,
    Unknown,
    Denied,
    Estimate,
}

impl StateRole {
    pub const ALL: [Self; 7] = [
        Self::Ok,
        Self::Warn,
        Self::Crit,
        Self::Stale,
        Self::Unknown,
        Self::Denied,
        Self::Estimate,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnerRole {
    SelfUi,
    Daemon,
    Pane,
    Sandbox,
    Container,
    Slice,
    Other,
}

impl OwnerRole {
    pub const ALL: [Self; 7] = [
        Self::SelfUi,
        Self::Daemon,
        Self::Pane,
        Self::Sandbox,
        Self::Container,
        Self::Slice,
        Self::Other,
    ];
}

impl Palette {
    /// Build eight contrast-stepped colors for a monitor series.
    pub fn ramp(&self, id: RampId) -> [String; 8] {
        let stops = id.stops();
        if stops.is_empty() {
            return std::array::from_fn(|k| self.heat[k * 4 / 7].clone());
        }
        let c_max = stops
            .iter()
            .map(|&hue| contrast_ratio(self.hue(hue), &self.panel))
            .fold(f32::INFINITY, f32::min);
        let c_lo = 1.3_f32.min(c_max);
        let mut previous = self.panel.clone();
        let mut previous_contrast = 1.0_f32;
        std::array::from_fn(|k| {
            let target = c_lo + (c_max - c_lo) * (k as f32 / 7.0);
            let stop_idx = (k * (stops.len() - 1) + 3) / 7;
            let hue = self.hue(stops[stop_idx]);
            let mut color = contrast_blend(hue, &self.panel, target);
            let mut contrast = contrast_ratio(&color, &self.panel);
            if contrast < previous_contrast {
                color = previous.clone();
                contrast = previous_contrast;
            }
            previous = color.clone();
            previous_contrast = contrast;
            color
        })
    }

    /// Resolve one level from 0 through 7, clamping larger inputs to 7.
    pub fn ramp_level(&self, id: RampId, level: u8) -> String {
        self.ramp(id)[usize::from(level.min(7))].clone()
    }

    pub fn band(&self, id: RampId, band: Band) -> &str {
        match band {
            Band::Normal => match id.family() {
                Hue::Amber | Hue::Orange | Hue::Red => &self.dim,
                _ => self.hue(id.band_family()),
            },
            Band::Warn => self.hue(Hue::Amber),
            Band::Crit => self.hue(Hue::Red),
        }
    }

    pub fn series(&self, role: SeriesRole) -> &str {
        use SeriesRole as R;
        match role {
            R::CpuUser | R::CoreP | R::MemCache | R::EngGfx | R::RailCore | R::Cat0 => {
                self.hue(Hue::Teal)
            }
            R::CpuNice | R::CoreE | R::MemBuffers | R::MemSlab | R::MemSwap | R::EngCopy => {
                self.hue(Hue::Blue)
            }
            R::CpuSys => self.hue(Hue::Red),
            R::CpuIrq | R::MemShmem | R::RailGfx | R::Cat4 => self.hue(Hue::Magenta),
            R::CpuIowait | R::MemDirty | R::EngVideo | R::Cat2 => self.hue(Hue::Amber),
            R::CpuSteal | R::MemKernel => self.hue(Hue::Orange),
            R::CpuGuest | R::MemAnon | R::EngCompute | R::RailSoc | R::Cat1 => {
                self.hue(Hue::Purple)
            }
            R::MemZram | R::Cat5 => self.hue(Hue::Green),
            R::MemFree => &self.ghost,
            R::EngOther | R::RailRest => &self.dim,
            R::Cat3 => self.hue(Hue::Blue),
            R::Selected => &self.accent,
            R::GhostOverlay => &self.ghost2,
        }
    }

    pub fn state(&self, role: StateRole) -> &str {
        match role {
            StateRole::Ok => self.hue(Hue::Green),
            StateRole::Warn => self.hue(Hue::Amber),
            StateRole::Crit => self.hue(Hue::Red),
            StateRole::Stale | StateRole::Denied => &self.faint,
            StateRole::Unknown => &self.ghost,
            StateRole::Estimate => &self.dim,
        }
    }

    pub fn owner(&self, role: OwnerRole) -> &str {
        match role {
            OwnerRole::SelfUi | OwnerRole::Daemon => self.hue(Hue::Green),
            OwnerRole::Pane => self.hue(Hue::Blue),
            OwnerRole::Sandbox => self.hue(Hue::Orange),
            OwnerRole::Container => self.hue(Hue::Teal),
            OwnerRole::Slice => self.hue(Hue::Purple),
            OwnerRole::Other => &self.text,
        }
    }
}

fn contrast_blend(hue: &str, panel: &str, target: f32) -> String {
    for step in 0..=50 {
        let color = blend_over(hue, panel, step as f32 * 0.02);
        if contrast_ratio(&color, panel) >= target {
            return color;
        }
    }
    blend_over(hue, panel, 1.0)
}

#[cfg(test)]
mod tests {
    use super::{Band, OwnerRole, RampId, SeriesRole, StateRole, band_of};
    use crate::termcaps::rgb_to_16;
    use crate::theme::{PRESETS, Palette, contrast_ratio, extend_palette, preset};
    use std::collections::HashSet;

    fn palettes() -> Vec<Palette> {
        let mut all: Vec<Palette> = PRESETS.iter().filter_map(|name| preset(name)).collect();
        all.push(Palette::default());
        for palette in &mut all {
            extend_palette(palette);
        }
        all
    }

    fn rgb_index(color: &str) -> u8 {
        let mut parts = color.split(';').map(|part| part.parse::<u8>().unwrap());
        rgb_to_16(
            parts.next().unwrap(),
            parts.next().unwrap(),
            parts.next().unwrap(),
        )
    }

    #[test]
    fn ramps_are_monotone_on_every_preset() {
        for palette in palettes() {
            for id in RampId::ALL.into_iter().filter(|id| *id != RampId::Heat) {
                let ramp = palette.ramp(id);
                let contrasts: Vec<f32> = ramp
                    .iter()
                    .map(|color| contrast_ratio(color, &palette.panel))
                    .collect();
                assert!(
                    contrasts.windows(2).all(|pair| pair[0] <= pair[1]),
                    "{id:?}: {contrasts:?}"
                );
                assert_ne!(ramp[0], ramp[7], "{id:?} ramp has no range");
            }
        }
    }

    #[test]
    fn heat_ramp_reuses_the_commit_heat_stops() {
        let palette = Palette::default();
        let ramp = palette.ramp(RampId::Heat);
        assert_eq!(ramp[0], palette.heat[0]);
        assert_eq!(ramp[7], palette.heat[4]);
        assert_eq!(palette.ramp_level(RampId::Heat, 255), palette.heat[4]);
    }

    #[test]
    fn banded_ansi16_has_three_colours_for_thresholded_ramps() {
        let palette = Palette::default();
        for id in [RampId::Util, RampId::Temp, RampId::Mem, RampId::Signal] {
            let indices: HashSet<u8> = [Band::Normal, Band::Warn, Band::Crit]
                .into_iter()
                .map(|band| rgb_index(palette.band(id, band)))
                .collect();
            assert_eq!(indices.len(), 3, "{id:?}: {indices:?}");
        }
        for palette in palettes() {
            for id in RampId::ALL {
                assert_ne!(
                    palette.band(id, Band::Warn),
                    palette.band(id, Band::Crit),
                    "{id:?}"
                );
            }
        }
    }

    #[test]
    fn warm_families_band_normal_to_dim() {
        let palette = Palette::default();
        for id in [RampId::Read, RampId::Write, RampId::Power] {
            assert_eq!(palette.band(id, Band::Normal), palette.dim);
        }
    }

    #[test]
    fn band_of_thresholds() {
        assert_eq!(band_of(f32::NAN, 50.0, 80.0), Band::Normal);
        assert_eq!(band_of(49.9, 50.0, 80.0), Band::Normal);
        assert_eq!(band_of(50.0, 50.0, 80.0), Band::Warn);
        assert_eq!(band_of(80.0, 50.0, 80.0), Band::Crit);
    }

    #[test]
    fn every_role_resolves_on_every_preset() {
        assert_eq!(SeriesRole::ALL.len(), 36);
        assert_eq!(StateRole::ALL.len(), 7);
        assert_eq!(OwnerRole::ALL.len(), 7);
        assert_eq!(RampId::ALL.len(), 10);
        for palette in palettes() {
            for color in SeriesRole::ALL
                .map(|role| palette.series(role))
                .into_iter()
                .chain(StateRole::ALL.map(|role| palette.state(role)))
                .chain(OwnerRole::ALL.map(|role| palette.owner(role)))
            {
                let parts: Vec<&str> = color.split(';').collect();
                assert_eq!(parts.len(), 3, "invalid RGB fragment {color:?}");
                assert!(
                    parts.iter().all(|part| part.parse::<u8>().is_ok()),
                    "invalid RGB fragment {color:?}"
                );
            }
            assert_eq!(
                palette.owner(OwnerRole::SelfUi),
                palette.hue(crate::theme::Hue::Green)
            );
            assert_eq!(
                palette.owner(OwnerRole::Pane),
                palette.hue(crate::theme::Hue::Blue)
            );
            assert_eq!(
                palette.series(SeriesRole::CpuIrq),
                palette.hue(crate::theme::Hue::Magenta)
            );
        }
    }

    #[test]
    fn degenerate_palette_ramp_stays_monotone() {
        let mut palette = Palette {
            panel: "80;80;80".into(),
            ..Palette::default()
        };
        for hue in [
            &mut palette.hues.teal,
            &mut palette.hues.magenta,
            &mut palette.hues.purple,
            &mut palette.hues.green,
            &mut palette.hues.amber,
            &mut palette.hues.red,
            &mut palette.hues.blue,
            &mut palette.hues.orange,
        ] {
            *hue = palette.panel.clone();
        }
        for id in RampId::ALL.into_iter().filter(|id| *id != RampId::Heat) {
            let ramp = palette.ramp(id);
            let contrasts: Vec<f32> = ramp
                .iter()
                .map(|color| contrast_ratio(color, &palette.panel))
                .collect();
            assert!(contrasts.windows(2).all(|pair| pair[0] <= pair[1]));
        }
    }

    #[test]
    fn decreasing_stop_contrast_is_clamped_to_the_previous_level() {
        let mut palette = Palette {
            panel: "34;217;29".into(),
            ..Palette::default()
        };
        palette.hues.teal = "253;236;30".into();
        palette.hues.green = "205;38;210".into();
        palette.hues.amber = "73;109;116".into();
        palette.hues.orange = "98;121;183".into();
        palette.hues.red = "255;184;33".into();

        let ramp = palette.ramp(RampId::Util);
        let contrasts: Vec<f32> = ramp
            .iter()
            .map(|color| contrast_ratio(color, &palette.panel))
            .collect();
        assert!(contrasts.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(ramp[6], ramp[7]);
    }
}
