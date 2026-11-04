//! Native audio/inference leaf. No core, host, terminal, or database dependency.
//! Native handles belong exclusively to the disposable voice worker process.
#[cfg(feature = "native")]
pub mod capture;
#[cfg(feature = "native")]
pub mod whisper;

pub const SAMPLE_RATE: usize = 16_000;
pub const WINDOW_SAMPLES: usize = 8 * SAMPLE_RATE;
pub const PREVIEW_STEP: usize = SAMPLE_RATE;

/// Keep the newest words visible during long utterances. The host applies its
/// cell-width/control filtering after this UTF-8-safe byte bound.
pub fn preview_snippet(text: &str) -> &str {
    let mut start = text.len().saturating_sub(160);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Streaming area resampler. Carries fractional frames across callbacks;
/// averages channels and integrates each input frame over 16 kHz output bins.
/// Memory and output rate are independent of the hardware sample rate.
pub struct Normalizer {
    rate: u32,
    channels: usize,
    filled: u32,
    sum: f64,
}
impl Normalizer {
    pub fn new(rate: u32, channels: usize) -> Self {
        assert!(rate > 0 && channels > 0);
        Self {
            rate,
            channels,
            filled: 0,
            sum: 0.0,
        }
    }
    pub fn push(&mut self, input: &[f32], output: &mut Vec<f32>, limit: usize) {
        for frame in input.chunks_exact(self.channels) {
            let value = frame
                .iter()
                .map(|x| {
                    if x.is_finite() {
                        x.clamp(-1.0, 1.0) as f64
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
                / self.channels as f64;
            let mut remaining = SAMPLE_RATE as u32;
            while remaining > 0 {
                let n = remaining.min(self.rate - self.filled);
                self.sum += value * n as f64;
                self.filled += n;
                remaining -= n;
                if self.filled == self.rate {
                    if output.len() < limit {
                        output.push((self.sum / self.rate as f64) as f32);
                    }
                    self.filled = 0;
                    self.sum = 0.0;
                }
            }
        }
    }
}

/// PCM16 mono WAV, generated in memory; never writes a recording to disk.
pub fn wav(samples: &[f32]) -> Vec<u8> {
    let size = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(size as usize + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(size + 36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&32_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&size.to_le_bytes());
    for x in samples {
        out.extend_from_slice(&((x.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

/// Revisions to the same window replace its tail. When a window advances,
/// merge the longest word overlap, preserving the committed prefix.
#[derive(Default)]
pub struct RollingTranscript {
    prefix: String,
    tail: String,
    start: usize,
}
impl RollingTranscript {
    pub fn update(&mut self, start: usize, text: &str) -> String {
        let text = preview_tail(text);
        if start != self.start {
            let old: Vec<_> = self.tail.split_whitespace().collect();
            let new: Vec<_> = text.split_whitespace().collect();
            let overlap = (1..=old.len().min(new.len()).min(256))
                .rev()
                .find(|&n| {
                    old[old.len() - n..]
                        .iter()
                        .zip(&new[..n])
                        .all(|(a, b)| a.eq_ignore_ascii_case(b))
                })
                .unwrap_or(0);
            // Languages without word separators need character overlap. Keep
            // the search finite even when a remote provider returns huge text.
            if overlap == 0 && old.len() == 1 && new.len() == 1 && !self.tail.is_ascii() {
                let keep = self
                    .tail
                    .char_indices()
                    .rev()
                    .take(512)
                    .filter(|(i, _)| text.starts_with(&self.tail[*i..]))
                    .map(|(i, _)| i)
                    .last();
                if let Some(keep) = keep {
                    self.prefix.push_str(&self.tail[..keep]);
                    self.prefix = preview_tail(&self.prefix).into();
                    self.tail = text.into();
                    self.start = start;
                    return format!("{}{}", self.prefix, self.tail);
                }
            }
            let stable = old[..old.len() - overlap].join(" ");
            if !stable.is_empty() {
                if !self.prefix.is_empty() && !self.prefix.ends_with(' ') {
                    self.prefix.push(' ');
                }
                self.prefix.push_str(&stable);
                self.prefix.push(' ');
            }
            self.prefix = preview_tail(&self.prefix).into();
            self.start = start;
        }
        self.tail = text.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("{}{}", self.prefix, self.tail).trim().into()
    }
}

fn preview_tail(text: &str) -> &str {
    let mut start = text.len().saturating_sub(32 * 1024);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_previews_keep_new_speech_visible_without_splitting_unicode() {
        let text = format!("{} latest spoken words", "界".repeat(200));
        let snippet = preview_snippet(&text);
        assert!(snippet.len() <= 160);
        assert!(snippet.ends_with("latest spoken words"));
        assert!(!snippet.starts_with('\u{fffd}'));
    }
    #[test]
    fn resampling_is_bounded_and_callback_independent() {
        for rate in [8000, 16000, 44100, 48000, 96000] {
            let input = vec![0.25; rate * 2];
            let mut a = Vec::new();
            let mut b = Vec::new();
            Normalizer::new(rate as u32, 2).push(&input, &mut a, SAMPLE_RATE);
            let mut n = Normalizer::new(rate as u32, 2);
            for chunk in input.chunks(14) {
                n.push(chunk, &mut b, SAMPLE_RATE);
            }
            assert_eq!(a, b);
            assert_eq!(a.len(), SAMPLE_RATE);
            assert!(a.iter().all(|x| (*x - 0.25).abs() < 0.0001));
        }
    }
    #[test]
    fn wav_has_exact_pcm_header() {
        let w = wav(&[0.0, 1.0, -1.0]);
        assert_eq!(w.len(), 50);
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(&w[40..44], &6u32.to_le_bytes());
    }
    #[test]
    fn overlapping_windows_and_revisions_do_not_duplicate_words() {
        let mut r = RollingTranscript::default();
        assert_eq!(r.update(0, "one too"), "one too");
        assert_eq!(r.update(0, "one two three"), "one two three");
        assert_eq!(r.update(1, "two three four"), "one two three four");
        assert_eq!(r.update(1, "two three five"), "one two three five");
    }
    #[test]
    fn preview_handles_non_segmented_languages_and_bounds_provider_text() {
        let mut r = RollingTranscript::default();
        assert_eq!(r.update(0, "你好世界"), "你好世界");
        assert_eq!(r.update(1, "世界再见"), "你好世界再见");
        assert_eq!(r.update(1, "世界另见"), "你好世界另见");
        let huge = "界".repeat(100_000);
        assert!(r.update(2, &huge).len() <= 64 * 1024 + 1);
    }
}
