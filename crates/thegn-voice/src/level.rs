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
    let peak = levels.iter().copied().fold(FLOOR_DBFS, f32::max);
    peak >= FIXED_THRESHOLD_DBFS || longest_run(&levels, relative_threshold(&levels)) >= 6
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

    fn tone(seconds: f32, dbfs: f32) -> Vec<f32> {
        (0..(seconds * 16_000.0) as usize)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                10.0_f32.powf(dbfs / 20.0) * (std::f32::consts::TAU * 440.0 * t).sin()
            })
            .collect()
    }

    fn with_noise(signal: &mut [f32], dbfs: f32) {
        let amplitude = 10.0_f32.powf(dbfs / 20.0) * 3.0_f32.sqrt();
        let mut state = 0x1234_5678_u32;
        for sample in signal {
            // Deterministic uniform noise with the requested RMS level.
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state as f32 / u32::MAX as f32 * 2.0 - 1.0) * amplitude;
            *sample += noise;
        }
    }

    #[test]
    fn detects_level_and_relative_speech_but_rejects_silence_and_noise() {
        assert!(!has_speech(&vec![0.0; 16_000]));
        let mut noise = vec![0.0; 16_000];
        with_noise(&mut noise, -72.0);
        assert!(!has_speech(&noise));
        let mut quiet_tone = tone(0.3, -55.0);
        with_noise(&mut quiet_tone, -80.0);
        assert!(has_speech(&quiet_tone));
        let short = tone(0.1, -58.0);
        let mut short = short;
        with_noise(&mut short, -80.0);
        assert!(!has_speech(&short));
    }

    #[test]
    fn fixed_level_click_is_speech_even_without_a_sustained_run() {
        // Match the fixed -50 dBFS fast path: a 10 ms click at -20 dBFS qualifies.
        let mut click = vec![0.0; 16_000];
        click[..160].fill(0.1);
        assert!(has_speech(&click));
    }

    #[test]
    fn one_quiet_window_bridges_but_two_break_the_run() {
        let mut bridged = tone(0.24, -55.0);
        let mut broken = bridged.clone();
        bridged[1_440..1_920].fill(0.0);
        broken[1_440..2_400].fill(0.0);
        with_noise(&mut bridged, -80.0);
        with_noise(&mut broken, -80.0);
        assert!(has_speech(&bridged));
        assert!(!has_speech(&broken));
    }

    #[test]
    fn span_pads_and_clamps_to_clip_edges() {
        let speech = tone(0.3, -55.0);
        let mut clip = vec![0.0; 16_000];
        clip[4_800..9_600].copy_from_slice(&speech);
        with_noise(&mut clip, -80.0);
        let span = speech_span(&clip).unwrap();
        assert_eq!(span, 16_000 / 10..8 * 16_000 / 10);

        let mut edge = tone(0.3, -55.0);
        with_noise(&mut edge, -80.0);
        assert_eq!(speech_span(&edge).unwrap().start, 0);
        assert_eq!(speech_span(&edge).unwrap().end, edge.len());
    }

    #[test]
    fn dc_offset_does_not_count_as_speech() {
        assert!(!has_speech(&vec![0.3; 16_000]));
    }
}
