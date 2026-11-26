//! Level-based speech detection for 16 kHz mono PCM.
use std::ops::Range;

const WINDOW_SAMPLES: usize = 480;
const FLOOR_DBFS: f32 = -120.0;
const FIXED_THRESHOLD_DBFS: f32 = -50.0;
const PADDING_SAMPLES: usize = 200 * 16;

/// Yield DC-removed RMS levels for successive 30 ms windows, in dBFS.
pub fn window_levels_dbfs(samples: &[f32]) -> impl Iterator<Item = f32> + '_ {
    samples.chunks(WINDOW_SAMPLES).map(|window| {
        if window.is_empty() {
            return FLOOR_DBFS;
        }
        let mean = window.iter().map(|&sample| sample as f64).sum::<f64>() / window.len() as f64;
        let mean_square = window
            .iter()
            .map(|&sample| {
                let centered = sample as f64 - mean;
                centered * centered
            })
            .sum::<f64>()
            / window.len() as f64;
        if mean_square <= 0.0 {
            FLOOR_DBFS
        } else {
            (10.0 * mean_square.log10() as f32).max(FLOOR_DBFS)
        }
    })
}

fn relative_threshold(levels: &[f32]) -> f32 {
    let mut noise: Vec<_> = levels
        .iter()
        .copied()
        .filter(|level| *level > FLOOR_DBFS)
        .collect();
    if noise.is_empty() {
        return -66.0;
    }
    noise.sort_by(f32::total_cmp);
    let percentile = noise[(noise.len() - 1) / 10];
    (percentile + 12.0).max(-66.0)
}

fn longest_run(levels: &[f32], threshold: f32) -> usize {
    let (mut longest, mut active, mut quiet) = (0, 0, 0);
    for &level in levels {
        if level >= threshold {
            active += 1;
            quiet = 0;
            longest = longest.max(active);
        } else if active > 0 {
            quiet += 1;
            if quiet > 1 {
                active = 0;
                quiet = 0;
            }
        }
    }
    longest
}

fn speech_threshold(levels: &[f32]) -> Option<f32> {
    let peak = levels.iter().copied().fold(FLOOR_DBFS, f32::max);
    if peak >= FIXED_THRESHOLD_DBFS {
        Some(FIXED_THRESHOLD_DBFS)
    } else {
        let relative = relative_threshold(levels);
        (longest_run(levels, relative) >= 6).then_some(relative)
    }
}

/// Detect speech from a fixed-level transient or a sustained level above the noise floor.
pub fn has_speech(samples: &[f32]) -> bool {
    let levels: Vec<_> = window_levels_dbfs(samples).collect();
    speech_threshold(&levels).is_some()
}

/// Return the detected speech windows with 200 ms of context on either side.
pub fn speech_span(samples: &[f32]) -> Option<Range<usize>> {
    let levels: Vec<_> = window_levels_dbfs(samples).collect();
    let threshold = speech_threshold(&levels)?;
    let first = levels.iter().position(|level| *level >= threshold)? * WINDOW_SAMPLES;
    let last = levels.iter().rposition(|level| *level >= threshold)?;
    Some(
        first.saturating_sub(PADDING_SAMPLES)
            ..((last + 1) * WINDOW_SAMPLES + PADDING_SAMPLES).min(samples.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: usize = 16_000;

    /// A clip of deterministic uniform noise whose RMS level is `dbfs`.
    fn noise(samples: usize, dbfs: f32) -> Vec<f32> {
        let amplitude = 10.0_f32.powf(dbfs / 20.0) * 3.0_f32.sqrt();
        let mut state = 0x1234_5678_u32;
        (0..samples)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state as f32 / u32::MAX as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    /// Add a 440 Hz tone whose RMS level is `dbfs` over `range`.
    fn add_tone(clip: &mut [f32], range: Range<usize>, dbfs: f32) {
        let amplitude = 10.0_f32.powf(dbfs / 20.0) * 2.0_f32.sqrt();
        for (sample, i) in clip[range.clone()].iter_mut().zip(range) {
            let t = i as f32 / RATE as f32;
            *sample += amplitude * (std::f32::consts::TAU * 440.0 * t).sin();
        }
    }

    #[test]
    fn digital_zeros_are_not_speech() {
        assert!(!has_speech(&vec![0.0; RATE]));
        assert_eq!(speech_span(&vec![0.0; RATE]), None);
    }

    #[test]
    fn steady_low_noise_is_not_speech() {
        assert!(!has_speech(&noise(RATE, -72.0)));
    }

    #[test]
    fn quiet_tone_over_noise_floor_is_speech_by_the_relative_path() {
        let mut clip = noise(RATE, -80.0);
        add_tone(&mut clip, 4_800..9_600, -55.0);
        assert!(has_speech(&clip));
    }

    #[test]
    fn short_quiet_tone_is_too_short() {
        let mut clip = noise(RATE, -80.0);
        add_tone(&mut clip, 4_800..6_400, -58.0);
        assert!(!has_speech(&clip));
    }

    #[test]
    fn fixed_level_click_is_speech_even_without_a_sustained_run() {
        // Like Hex, any window at or above -50 dBFS passes the fixed gate, so a
        // single 10 ms click at -20 dBFS counts as speech. This is deliberate:
        // the gate only rejects clips that are certainly empty.
        let mut click = vec![0.0; RATE];
        click[..160].fill(0.1);
        assert!(has_speech(&click));
    }

    #[test]
    fn one_quiet_window_bridges_but_two_break_the_run() {
        // Six tone windows split 3 + 3 by one or two noise-only windows.
        let w = WINDOW_SAMPLES;
        let mut bridged = noise(RATE, -80.0);
        add_tone(&mut bridged, 10 * w..13 * w, -55.0);
        add_tone(&mut bridged, 14 * w..17 * w, -55.0);
        assert!(has_speech(&bridged));

        let mut broken = noise(RATE, -80.0);
        add_tone(&mut broken, 10 * w..13 * w, -55.0);
        add_tone(&mut broken, 15 * w..18 * w, -55.0);
        assert!(!has_speech(&broken));
    }

    #[test]
    fn leading_quiet_windows_do_not_use_up_the_bridge() {
        assert_eq!(longest_run(&[-80.0, -80.0, -50.0, -80.0, -50.0], -60.0), 2);
        assert_eq!(longest_run(&[-50.0, -80.0, -80.0, -50.0], -60.0), 1);
    }

    #[test]
    fn span_pads_by_200_ms_and_clamps_to_clip_edges() {
        let mut clip = noise(RATE, -80.0);
        add_tone(&mut clip, 4_800..9_600, -55.0);
        assert_eq!(speech_span(&clip), Some(1_600..12_800));

        // Speech touching both edges cannot be padded past the clip.
        let len = 27 * WINDOW_SAMPLES;
        let mut edge = noise(len, -80.0);
        add_tone(&mut edge, 0..6 * WINDOW_SAMPLES, -55.0);
        add_tone(&mut edge, len - 6 * WINDOW_SAMPLES..len, -55.0);
        assert_eq!(speech_span(&edge), Some(0..len));
    }

    #[test]
    fn dc_offset_does_not_count_as_speech() {
        assert!(!has_speech(&vec![0.3; RATE]));
        let mut offset = noise(RATE, -80.0);
        offset.iter_mut().for_each(|sample| *sample += 0.3);
        assert!(!has_speech(&offset));
    }
}
