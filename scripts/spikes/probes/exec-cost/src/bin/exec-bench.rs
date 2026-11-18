//! WS03-P0-02 measurement loop: interleaved spawn+wait wall times.
//!
//!   exec-bench RUNS WARMUP 'label=cmd arg arg' 'label=cmd arg arg' ...
//!
//! Subjects run round-robin (so frequency drift hits all equally). Prints one
//! `RESULT label=.. n=.. p50_us=.. p95_us=.. min_us=.. max_us=..` line each.
//! Every child is waited on (no zombies).

use std::process::{Command, Stdio};
use std::time::Instant;

fn pct(sorted: &[u128], p: f64) -> u128 {
    let n = sorted.len();
    let idx = ((p / 100.0) * n as f64).ceil() as usize;
    sorted[idx.clamp(1, n) - 1]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: exec-bench RUNS WARMUP 'label=cmd args' ...");
        std::process::exit(2);
    }
    let runs: usize = args[1].parse().expect("RUNS");
    let warmup: usize = args[2].parse().expect("WARMUP");
    let mut subjects: Vec<(String, Vec<String>, Vec<u128>)> = Vec::new();
    for s in &args[3..] {
        let (label, cmd) = s.split_once('=').expect("label=cmd");
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        subjects.push((label.to_string(), argv, Vec::with_capacity(runs)));
    }
    for i in 0..(warmup + runs) {
        for (_, argv, times) in subjects.iter_mut() {
            let t = Instant::now();
            let status = Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("spawn");
            let dt = t.elapsed().as_micros();
            if !status.success() {
                eprintln!("subject {:?} exited {status}", argv);
                std::process::exit(3);
            }
            if i >= warmup {
                times.push(dt);
            }
        }
    }
    for (label, _, mut times) in subjects {
        times.sort_unstable();
        println!(
            "RESULT label={label} n={} p50_us={} p95_us={} min_us={} max_us={}",
            times.len(),
            pct(&times, 50.0),
            pct(&times, 95.0),
            times[0],
            times[times.len() - 1]
        );
    }
}
