//! Micro-benchmarks for hot core paths.
//!
//! Theme/palette construction runs at startup and again on every theme cycle
//! (and is piped to embedded app tiles), so it's on the interactive hot path.
//! Pure and allocation-light — a good A/B target when touching `theme.rs`.
//!
//! Run: `cargo bench -p superzej-core --bench core_hot`.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use superzej_core::theme::{Palette, extend_palette, preset};

fn bench_theme(c: &mut Criterion) {
    let mut g = c.benchmark_group("theme");
    g.bench_function("palette_default", |b| b.iter(|| black_box(Palette::default())));
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

criterion_group!(benches, bench_theme);
criterion_main!(benches);
