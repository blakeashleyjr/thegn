//! WS10-P2-04 throwaway probe (never merged): NDJSON + base64 `stream.data`
//! framing against a child echo process, versus a raw-pipe baseline.
//!
//!   plugin-framing echo-child
//!       Rust echo child: reads NDJSON lines, decodes `data`, re-encodes, writes back.
//!   plugin-framing bench LABEL MODE CMD [ARGS...]
//!       MODE = framed | raw. Spawns CMD as the echo child and prints
//!         COLD   label=.. mode=.. cold_p50_us=..
//!         RESULT label=.. mode=.. size=.. lat_p50_us=.. lat_p99_us=.. mbps=..
//!       (size 1048576 is sent as 32 frames of 32 KiB; its latency is burst completion.)
//! `raw` MODE sends the payload + '\n' with no JSON or base64 (child should be `cat`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

use base64::{Engine, engine::general_purpose::STANDARD as B64};

const CHUNK: usize = 32 * 1024;

fn echo_child() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.expect("stdin");
        let mut v: serde_json::Value = serde_json::from_str(&line).expect("json");
        let raw = B64
            .decode(v["data"].as_str().expect("data"))
            .expect("base64");
        v["data"] = serde_json::Value::String(B64.encode(&raw));
        serde_json::to_writer(&mut out, &v).expect("write");
        out.write_all(b"\n").expect("write");
        out.flush().expect("flush");
    }
}

fn frame(framed: bool, seq: u64, payload: &[u8]) -> Vec<u8> {
    if framed {
        let v = serde_json::json!({
            "stream": 3, "seq": seq, "data": B64.encode(payload), "ch": "out"
        });
        let mut b = serde_json::to_vec(&v).expect("json");
        b.push(b'\n');
        b
    } else {
        let mut b = payload.to_vec();
        b.push(b'\n');
        b
    }
}

fn read_one(framed: bool, r: &mut BufReader<ChildStdout>, line: &mut String) -> usize {
    line.clear();
    let n = r.read_line(line).expect("read");
    assert!(n > 0, "child closed stdout");
    if framed {
        let v: serde_json::Value = serde_json::from_str(line).expect("json");
        B64.decode(v["data"].as_str().expect("data"))
            .expect("base64")
            .len()
    } else {
        n - 1
    }
}

struct Peer {
    child: Child,
    w: ChildStdin,
    r: BufReader<ChildStdout>,
}

fn spawn(cmd: &[String]) -> Peer {
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");
    let w = child.stdin.take().expect("stdin");
    let r = BufReader::with_capacity(1 << 20, child.stdout.take().expect("stdout"));
    Peer { child, w, r }
}

fn finish(p: Peer) {
    let Peer { mut child, w, r } = p;
    drop(w);
    drop(r);
    child.wait().expect("wait"); // never leave a zombie
}

fn pct(v: &mut [u128], p: f64) -> u128 {
    v.sort_unstable();
    let n = v.len();
    v[(((p / 100.0) * n as f64).ceil() as usize).clamp(1, n) - 1]
}

fn pipelined(peer: &mut Peer, framed: bool, size: usize, count: usize) -> f64 {
    let payload = vec![b'a'; size];
    let start = Instant::now();
    let Peer { w, r, .. } = peer;
    std::thread::scope(|s| {
        s.spawn(|| {
            for i in 0..count {
                w.write_all(&frame(framed, i as u64, &payload)).expect("w");
            }
            w.flush().expect("flush");
        });
        let mut line = String::new();
        for _ in 0..count {
            read_one(framed, r, &mut line);
        }
    });
    (size * count) as f64 / start.elapsed().as_secs_f64() / 1e6
}

fn bench(label: &str, mode: &str, cmd: &[String]) {
    let framed = mode == "framed";
    // cold start: spawn to first echo
    let mut cold = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        let mut p = spawn(cmd);
        p.w.write_all(&frame(framed, 0, b"x")).expect("w");
        p.w.flush().expect("flush");
        read_one(framed, &mut p.r, &mut String::new());
        cold.push(t.elapsed().as_micros());
        finish(p);
    }
    println!(
        "COLD label={label} mode={mode} cold_p50_us={}",
        pct(&mut cold, 50.0)
    );

    for size in [1usize, 1024, CHUNK, 1024 * 1024] {
        let mut p = spawn(cmd);
        let mut line = String::new();
        let chunk = size.min(CHUNK);
        let frames_per_burst = size / chunk;
        let payload = vec![b'a'; chunk];
        let (iters, warm) = if size > CHUNK { (30, 3) } else { (2000, 100) };
        let mut lat = Vec::with_capacity(iters);
        for i in 0..(iters + warm) {
            let t = Instant::now();
            if frames_per_burst == 1 {
                p.w.write_all(&frame(framed, i as u64, &payload))
                    .expect("w");
                p.w.flush().expect("flush");
                read_one(framed, &mut p.r, &mut line);
            } else {
                // A burst larger than the pipe must be read while it is written,
                // or writer and child deadlock on full pipes.
                let Peer { w, r, .. } = &mut p;
                std::thread::scope(|s| {
                    s.spawn(|| {
                        for k in 0..frames_per_burst {
                            let seq = (i * frames_per_burst + k) as u64;
                            w.write_all(&frame(framed, seq, &payload)).expect("w");
                        }
                        w.flush().expect("flush");
                    });
                    for _ in 0..frames_per_burst {
                        read_one(framed, r, &mut line);
                    }
                });
            }
            if i >= warm {
                lat.push(t.elapsed().as_micros());
            }
        }
        let count = if size > CHUNK {
            frames_per_burst * 20
        } else if size == CHUNK {
            2000
        } else {
            5000
        };
        let mbps = pipelined(&mut p, framed, chunk, count);
        println!(
            "RESULT label={label} mode={mode} size={size} lat_p50_us={} lat_p99_us={} mbps={mbps:.1}",
            pct(&mut lat, 50.0),
            pct(&mut lat, 99.0)
        );
        finish(p);
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("echo-child") => echo_child(),
        Some("bench") if a.len() >= 5 => bench(&a[2], &a[3], &a[4..]),
        _ => {
            eprintln!("usage: plugin-framing echo-child | bench LABEL framed|raw CMD [ARGS...]");
            std::process::exit(2);
        }
    }
}
