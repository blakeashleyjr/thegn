//! Opt-in inference measurements on supplied PCM16 mono/16 kHz audio.
//! Never opens a microphone. See docs/voice-verification.md.
#[cfg(feature = "native")]
fn main() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use std::io::{Read, Write};
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{Duration, Instant};
    use thegn_voice::{SAMPLE_RATE, WINDOW_SAMPLES, whisper::Engine};

    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 5,
        "usage: verify MODEL PCM16_FILE LANGUAGE ITERATIONS"
    );
    let iterations: usize = args[4].parse()?;
    ensure!((1..=100).contains(&iterations), "iterations must be 1..100");
    let limit = SAMPLE_RATE * 300 * 2;
    let mut bytes = Vec::new();
    std::fs::File::open(&args[2])?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= limit && bytes.len() % 2 == 0,
        "audio must be PCM16 mono/16 kHz, at most 300 seconds"
    );
    let samples: Vec<_> = bytes
        .chunks_exact(2)
        .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0)
        .collect();
    let language = &args[3];
    Engine::validate_language(language)?;
    let began = Instant::now();
    let engine = Engine::load(std::path::Path::new(&args[1]))?;
    let mut output = std::io::stdout().lock();
    writeln!(output, "model_load_ms\t{}", began.elapsed().as_millis())?;
    let window = &samples[samples.len().saturating_sub(WINDOW_SAMPLES)..];
    writeln!(output, "pass\titeration\taudio_ms\tdecode_ms\ttranscript")?;
    for i in 0..iterations {
        let began = Instant::now();
        let text = engine.transcribe(
            window,
            language,
            Arc::new(AtomicBool::new(false)),
            Duration::from_secs(60),
        )?;
        writeln!(
            output,
            "preview\t{i}\t{}\t{}\t{text:?}",
            window.len() * 1000 / SAMPLE_RATE,
            began.elapsed().as_millis()
        )?;
    }
    let began = Instant::now();
    let text = engine.transcribe(
        &samples,
        language,
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(60),
    )?;
    writeln!(
        output,
        "final\t0\t{}\t{}\t{text:?}",
        samples.len() * 1000 / SAMPLE_RATE,
        began.elapsed().as_millis()
    )?;
    let began = Instant::now();
    ensure!(
        engine
            .transcribe(
                window,
                language,
                Arc::new(AtomicBool::new(true)),
                Duration::from_secs(60)
            )
            .is_err(),
        "cancelled decode succeeded"
    );
    writeln!(output, "pre_cancel_ms\t{}", began.elapsed().as_millis())?;
    let silent = engine.transcribe(
        &vec![0.0; SAMPLE_RATE],
        language,
        Arc::new(AtomicBool::new(false)),
        Duration::from_secs(1),
    )?;
    ensure!(silent.is_empty(), "digital silence produced a transcript");
    writeln!(output, "silence\tpassed")?;
    output.flush().context("write measurements")?;
    Ok(())
}

#[cfg(not(feature = "native"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("build the verify example with --features native (metal on Apple silicon)")
}
