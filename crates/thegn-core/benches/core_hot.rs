//! Micro-benchmarks for hot core paths.
//!
//! Theme/palette construction runs at startup and again on every theme cycle
//! (and is piped to embedded app tiles), so it's on the interactive hot path.
//! Pure and allocation-light — a good A/B target when touching `theme.rs`.
//!
//! Pane port-hint parsing runs on the event loop for every pane output
//! chunk; `pane_port_hints` compares a whole 64 KiB tail with the ~1 KiB
//! window the host now re-parses per chunk.
//!
//! Run: `cargo bench -p thegn-core --bench core_hot`.

use criterion::{Criterion, criterion_group, criterion_main};
use std::fmt::Write as _;
use std::hint::black_box;
use thegn_core::preview::{MAX_PORT_HINT_CHARS, parse_pane_port_hints_wrapped};
use thegn_core::theme::{Palette, extend_palette, preset};

fn bench_theme(c: &mut Criterion) {
    let mut g = c.benchmark_group("theme");
    g.bench_function("palette_default", |b| {
        b.iter(|| black_box(Palette::default()))
    });
    g.bench_function("extend_palette", |b| {
        b.iter(|| {
            let mut p = Palette::default();
            extend_palette(&mut p);
            black_box(&p);
        })
    });
    for name in ["prism", "storm", "light"] {
        g.bench_function(format!("preset_{name}"), |b| {
            b.iter(|| black_box(preset(black_box(name))))
        });
    }
    g.finish();
}

/// Spinner-shaped pane output (`\r⠋ building… 12s`) after one dev-server URL,
/// truncated to `len` bytes from the end on a char boundary.
fn spinner_tail(len: usize) -> String {
    let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let mut text = String::from("ready http://localhost:5173/\n");
    let mut second = 0usize;
    while text.len() < MAX_PORT_HINT_CHARS {
        let frame = frames[second % frames.len()];
        write!(text, "\r{frame} building… {second}s {}", "·".repeat(24))
            .expect("writing to String cannot fail");
        second += 1;
    }
    let mut start = text.len().saturating_sub(len);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

fn bench_pane_port_hints(c: &mut Criterion) {
    let mut g = c.benchmark_group("pane_port_hints");
    for (name, len) in [("tail_64k", MAX_PORT_HINT_CHARS), ("window_1k", 1024)] {
        let input = spinner_tail(len);
        g.bench_function(name, |b| {
            b.iter(|| black_box(parse_pane_port_hints_wrapped(black_box(&input), Some(120))))
        });
    }
    g.finish();
}

criterion_group!(benches, bench_theme, bench_pane_port_hints);
criterion_main!(benches);
