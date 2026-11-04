//! Advisory Criterion benchmarks for Observe time-series reduction and rendering.
//!
//! Inputs are generated deterministically, so runs compare the same dense,
//! spiky, flat, irregular, sparse, windowed and multi-series shapes. Timings
//! are machine-dependent and are not asserted.
//!
//! Run: `cargo bench -p gtui-render --bench timeseries`

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use gtui_core::frame::{Field, FieldType, Frame};
use gtui_render::{
    RenderColors,
    timeseries::{TimeseriesRenderer, reduce},
};
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use std::hint::black_box;

const WIDTH: usize = 240;
const HEIGHT: u16 = 60;
const SIZES: [usize; 3] = [1_000, 100_000, 1_000_000];
const SEED: u64 = 0x6a09_e667_f3bc_c909;
const BOUNDS: [f64; 4] = [0.0, 1.0, 0.0, 1.0];

struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn frame(times: Vec<f64>, values: Vec<f64>) -> Frame {
    Frame::new(vec![
        Field::new("time", FieldType::Time, times),
        Field::new("value", FieldType::Float64, values),
    ])
}

fn dense_with_seed(n: usize, seed: u64) -> Frame {
    let mut random = Lcg(seed);
    let mut value = 0.5;
    let times = (0..n).map(|index| index as f64).collect();
    let values = (0..n)
        .map(|_| {
            value = (value + (random.next_unit() - 0.5) * 0.04).clamp(0.0, 1.0);
            value
        })
        .collect();
    frame(times, values)
}

fn dense(n: usize) -> Frame {
    dense_with_seed(n, SEED)
}

fn spike(n: usize) -> Frame {
    let times = (0..n).map(|index| index as f64).collect();
    let values = (0..n)
        .map(|index| {
            if index.is_multiple_of(997) {
                1.0e6
            } else {
                0.5
            }
        })
        .collect();
    frame(times, values)
}

fn flat(n: usize) -> Frame {
    let times = (0..n).map(|index| index as f64).collect();
    let values = vec![0.5; n];
    frame(times, values)
}

fn irregular(n: usize, seed: u64) -> Frame {
    let mut random = Lcg(seed);
    let mut time = 0.0;
    let times = (0..n)
        .map(|index| {
            time += if index > 0 && index.is_multiple_of(5_000) {
                3_600.0
            } else {
                1.0 + 4.0 * random.next_unit()
            };
            time
        })
        .collect();
    let values = (0..n).map(|_| random.next_unit()).collect();
    frame(times, values)
}

// Sparse is the small irregular case: few samples relative to the panel width.
fn sparse(n: usize) -> Frame {
    irregular(n, SEED)
}

fn bench_reduce(c: &mut Criterion) {
    let mut group = c.benchmark_group("reduce");
    for shape in ["dense", "spike", "flat", "irregular"] {
        for n in SIZES {
            let data = match shape {
                "dense" => dense(n),
                "spike" => spike(n),
                "flat" => flat(n),
                "irregular" => irregular(n, SEED),
                _ => unreachable!(),
            };
            if n == 1_000_000 {
                group.sample_size(10);
            }
            group.throughput(Throughput::Elements(n as u64));
            group.bench_with_input(BenchmarkId::new(shape, n), &data, |b, data| {
                b.iter(|| black_box(reduce(black_box(data), BOUNDS, None, WIDTH)))
            });
            if n == 1_000_000 {
                group.sample_size(100);
            }
        }
    }

    let sparse_frame = sparse(SIZES[0]);
    group.throughput(Throughput::Elements(SIZES[0] as u64));
    group.bench_with_input(
        BenchmarkId::new("sparse", SIZES[0]),
        &sparse_frame,
        |b, data| b.iter(|| black_box(reduce(black_box(data), BOUNDS, None, WIDTH))),
    );

    let windowed = irregular(SIZES[1], SEED);
    let times = windowed.fields[0].floats();
    let first = times[0];
    let span = times[times.len() - 1] - first;
    let window = Some([first + span * 0.25, first + span * 0.75]);
    group.throughput(Throughput::Elements(SIZES[1] as u64));
    group.bench_with_input(
        BenchmarkId::new("windowed_irregular", SIZES[1]),
        &windowed,
        |b, data| b.iter(|| black_box(reduce(black_box(data), BOUNDS, window, WIDTH))),
    );
    group.finish();
}

fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("render");
    let area = Rect::new(0, 0, WIDTH as u16, HEIGHT);
    let colors = RenderColors::default();
    for (shape, data) in [("dense", dense(SIZES[1])), ("spike", spike(SIZES[1]))] {
        group.throughput(Throughput::Elements(SIZES[1] as u64));
        group.bench_with_input(BenchmarkId::new(shape, SIZES[1]), &data, |b, data| {
            b.iter(|| {
                let mut buffer = Buffer::empty(area);
                TimeseriesRenderer::render_in_window(data, "bench", BOUNDS, None, WIDTH, &colors)
                    .render(area, &mut buffer);
                black_box(buffer)
            })
        });
    }
    group.finish();
}

fn bench_multi_series(c: &mut Criterion) {
    let frames: Vec<Frame> = (0..8)
        .map(|series| dense_with_seed(SIZES[1], SEED.wrapping_add(series as u64)))
        .collect();
    let mut group = c.benchmark_group("multi_series");
    group.throughput(Throughput::Elements((SIZES[1] * frames.len()) as u64));
    group.bench_function("dense_8x100000", |b| {
        b.iter(|| {
            black_box(
                frames
                    .iter()
                    .map(|data| reduce(data, BOUNDS, None, WIDTH))
                    .collect::<Vec<_>>(),
            )
        })
    });
    group.finish();
}

criterion_group!(benches, bench_reduce, bench_render, bench_multi_series);
criterion_main!(benches);
