//! Micro-benchmarks for hot core paths.
//!
//! Theme/palette construction runs at startup and again on every theme cycle
//! (and is piped to embedded app tiles), so it's on the interactive hot path.
//! Pure and allocation-light — a good A/B target when touching `theme.rs`.
//!
//! Run: `cargo bench -p thegn-core --bench core_hot`.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
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

/// What one `Db::open()` costs on the warm path.
///
/// The host opens the DB from ~311 call sites, ~40 of them on the event-loop
/// thread, so the per-open cost is multiplied hard. It used to be dominated on
/// Windows by two `icacls.exe` spawns from `fsperm` (~80ms per open — see
/// `docs/windows-native-audit.md`); with those memoized this measures what is
/// actually left: `sqlite3_open` plus three pragmas and the `user_version`
/// fast-path query.
///
/// This is the number that decides whether a connection pool is worth the
/// churn of touching those call sites. Benched against an explicit path so it
/// never touches the developer's real `XDG_STATE_HOME`.
fn bench_db_open(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench.db");
    // Create + migrate once, so the loop measures the WARM path (schema already
    // at `user_version`), which is what the host actually pays.
    drop(thegn_core::db::Db::open_at(&path).expect("seed"));

    let mut g = c.benchmark_group("db");
    g.bench_function("open_at_warm", |b| {
        b.iter(|| {
            let db = thegn_core::db::Db::open_at(black_box(&path)).expect("open");
            black_box(&db);
        })
    });
    g.finish();
}

criterion_group!(benches, bench_theme, bench_db_open);
criterion_main!(benches);
