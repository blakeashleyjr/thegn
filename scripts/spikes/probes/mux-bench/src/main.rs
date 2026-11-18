//! WS09-P0-04 throwaway probe (never merged): minimal mux framing with a
//! per-channel credit window, versus per-pane ssh over a ControlMaster, and
//! optimistic OPEN. Frame layout copies plan 3.8: [tag u8][chan u32 BE][len u32 BE][payload].
//!
//!   mux-bench agent                                   remote end (stdin/stdout)
//!   mux-bench run mux|cm TARGET AGENT WINDOW IDLE_S FLOOD_S
//!       7 echo channels + 1 flood channel; prints
//!       RESULT mode= window= idle_p50_us= idle_p99_us= flood_p50_us= flood_p99_us= flood_mbps=
//!   mux-bench unary TARGET AGENT N
//!       prints UNARY optimistic=0|1 p50_us= p99_us=
//! TARGET is an ssh destination (e.g. localhost); AGENT is the absolute path of this binary
//! as seen on the target. Every child is killed and waited on before exit.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const PREAMBLE: &[u8; 8] = b"\x00tgMUX\x02\n";
const HELLO: u8 = 0;
const OPEN: u8 = 1;
const OPEN_OK: u8 = 2;
const DATA: u8 = 4;
const CLOSE: u8 = 5;
const WINDOW: u8 = 6;
const ECHO_CHANS: u32 = 7;
const FLOOD_CHAN: u32 = 100;
const FLOOD_CHUNK: usize = 16 * 1024;

fn write_frame<W: Write>(w: &mut W, tag: u8, chan: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(9 + payload.len());
    buf.push(tag);
    buf.extend_from_slice(&chan.to_be_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

fn read_frame<R: Read>(r: &mut R) -> std::io::Result<(u8, u32, Vec<u8>)> {
    let mut hdr = [0u8; 9];
    r.read_exact(&mut hdr)?;
    let chan = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
    let len = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((hdr[0], chan, payload))
}

// ---------------------------------------------------------------- agent

struct Chan {
    flood: bool,
    credit: usize,
}

struct State {
    chans: HashMap<u32, Chan>,
    closed: bool,
}

fn agent() {
    let mut stdin = BufReader::new(std::io::stdin());
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let mut pre = [0u8; 8];
    stdin.read_exact(&mut pre).expect("preamble");
    assert_eq!(&pre, PREAMBLE, "bad preamble");
    write_frame(&mut *out.lock().unwrap(), HELLO, 0, b"v2").expect("hello");

    let st = Arc::new((
        Mutex::new(State {
            chans: HashMap::new(),
            closed: false,
        }),
        Condvar::new(),
    ));
    let (st2, out2) = (Arc::clone(&st), Arc::clone(&out));
    let flooder = std::thread::spawn(move || {
        let (m, cv) = &*st2;
        loop {
            let mut g = m.lock().unwrap();
            let pick = loop {
                if g.closed {
                    return;
                }
                let found = g
                    .chans
                    .iter_mut()
                    .find(|(_, c)| c.flood && c.credit > 0)
                    .map(|(id, c)| {
                        let n = c.credit.min(FLOOD_CHUNK);
                        c.credit -= n;
                        (*id, n)
                    });
                if let Some(p) = found {
                    break p;
                }
                g = cv.wait(g).unwrap();
            };
            drop(g);
            let buf = vec![b'y'; pick.1];
            if write_frame(&mut *out2.lock().unwrap(), DATA, pick.0, &buf).is_err() {
                return;
            }
        }
    });

    let (m, cv) = &*st;
    while let Ok((tag, chan, pl)) = read_frame(&mut stdin) {
        match tag {
            OPEN => {
                let kind = String::from_utf8_lossy(&pl).to_string();
                let c = if let Some(w) = kind.strip_prefix("flood:") {
                    Chan {
                        flood: true,
                        credit: w.parse().unwrap_or(0),
                    }
                } else {
                    Chan {
                        flood: false,
                        credit: 0,
                    }
                };
                m.lock().unwrap().chans.insert(chan, c);
                cv.notify_all();
                let _ = write_frame(&mut *out.lock().unwrap(), OPEN_OK, chan, b"");
            }
            DATA => {
                let echo = m.lock().unwrap().chans.get(&chan).is_some_and(|c| !c.flood);
                if echo {
                    let _ = write_frame(&mut *out.lock().unwrap(), DATA, chan, &pl);
                }
            }
            WINDOW if pl.len() == 4 => {
                let n = u32::from_be_bytes([pl[0], pl[1], pl[2], pl[3]]) as usize;
                if let Some(c) = m.lock().unwrap().chans.get_mut(&chan) {
                    c.credit += n;
                }
                cv.notify_all();
            }
            CLOSE => {
                m.lock().unwrap().chans.remove(&chan);
            }
            _ => {}
        }
    }
    m.lock().unwrap().closed = true;
    cv.notify_all();
    let _ = flooder.join();
}

// ---------------------------------------------------------------- client helpers

fn pct(v: &mut [u128], p: f64) -> u128 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let n = v.len();
    v[(((p / 100.0) * n as f64).ceil() as usize).clamp(1, n) - 1]
}

fn ssh_agent(target: &str, agent_path: &str) -> Child {
    Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-T", target, agent_path, "agent"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ssh")
}

type Samples = Arc<Mutex<Vec<(u8, u128)>>>;
type Sent = Arc<Mutex<HashMap<u64, (Instant, u8)>>>;

fn report(mode: &str, window: &str, samples: &Samples, flood_bytes: u64, flood_s: f64) {
    let s = samples.lock().unwrap();
    let mut idle: Vec<u128> = s.iter().filter(|(p, _)| *p == 0).map(|(_, v)| *v).collect();
    let mut fl: Vec<u128> = s.iter().filter(|(p, _)| *p == 1).map(|(_, v)| *v).collect();
    println!(
        "RESULT mode={mode} window={window} idle_p50_us={} idle_p99_us={} flood_p50_us={} flood_p99_us={} flood_mbps={:.1}",
        pct(&mut idle, 50.0),
        pct(&mut idle, 99.0),
        pct(&mut fl, 50.0),
        pct(&mut fl, 99.0),
        flood_bytes as f64 / flood_s / 1e6
    );
}

fn payload_for(seq: u64) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[..8].copy_from_slice(&seq.to_be_bytes());
    p
}

// ---------------------------------------------------------------- mux mode

fn run_mux(target: &str, agent_path: &str, window: usize, idle_s: u64, flood_s: u64) {
    let mut child = ssh_agent(target, agent_path);
    let w: Arc<Mutex<ChildStdin>> = Arc::new(Mutex::new(child.stdin.take().unwrap()));
    let mut r = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
    w.lock().unwrap().write_all(PREAMBLE).unwrap();
    let (tag, _, _) = read_frame(&mut r).expect("hello");
    assert_eq!(tag, HELLO);

    let sent: Sent = Arc::new(Mutex::new(HashMap::new()));
    let samples: Samples = Arc::new(Mutex::new(Vec::new()));
    let flood_bytes = Arc::new(AtomicU64::new(0));
    let phase = Arc::new(AtomicU8::new(0));
    let (sent2, samples2, fb2, w2) = (
        Arc::clone(&sent),
        Arc::clone(&samples),
        Arc::clone(&flood_bytes),
        Arc::clone(&w),
    );
    let reader = std::thread::spawn(move || {
        while let Ok((tag, chan, pl)) = read_frame(&mut r) {
            if tag != DATA {
                continue;
            }
            if chan == FLOOD_CHAN {
                fb2.fetch_add(pl.len() as u64, Ordering::Relaxed);
                let credit = (pl.len() as u32).to_be_bytes();
                if write_frame(&mut *w2.lock().unwrap(), WINDOW, chan, &credit).is_err() {
                    break;
                }
            } else if pl.len() >= 8 {
                let seq = u64::from_be_bytes(pl[..8].try_into().unwrap());
                if let Some((t, ph)) = sent2.lock().unwrap().remove(&seq) {
                    samples2.lock().unwrap().push((ph, t.elapsed().as_micros()));
                }
            }
        }
    });

    for c in 1..=ECHO_CHANS {
        write_frame(&mut *w.lock().unwrap(), OPEN, c, b"echo").unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));

    let mut seq = 0u64;
    let mut ping_for = |secs: u64| {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            for c in 1..=ECHO_CHANS {
                seq += 1;
                sent.lock()
                    .unwrap()
                    .insert(seq, (Instant::now(), phase.load(Ordering::Relaxed)));
                write_frame(&mut *w.lock().unwrap(), DATA, c, &payload_for(seq)).unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    ping_for(idle_s);
    phase.store(1, Ordering::Relaxed);
    let open = format!("flood:{window}");
    let fb_start = flood_bytes.load(Ordering::Relaxed);
    write_frame(&mut *w.lock().unwrap(), OPEN, FLOOD_CHAN, open.as_bytes()).unwrap();
    ping_for(flood_s);
    let fb = flood_bytes.load(Ordering::Relaxed) - fb_start;

    child.kill().ok(); // best-effort: the connection is being torn down anyway
    child.wait().expect("wait ssh");
    reader.join().ok();
    report("mux", &window.to_string(), &samples, fb, flood_s as f64);
}

// ---------------------------------------------------------------- ControlMaster mode

fn run_cm(target: &str, idle_s: u64, flood_s: u64) {
    let sock = std::env::temp_dir().join(format!("tgmuxbench-{}.ctl", std::process::id()));
    let sock_s = sock.to_string_lossy().to_string();
    let mut master = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-M", "-S", &sock_s, "-N", target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn master");
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(sock.exists(), "ControlMaster socket never appeared");

    let sent: Sent = Arc::new(Mutex::new(HashMap::new()));
    let samples: Samples = Arc::new(Mutex::new(Vec::new()));
    let phase = Arc::new(AtomicU8::new(0));
    let mut kids: Vec<Child> = Vec::new();
    let mut writers: Vec<ChildStdin> = Vec::new();
    let mut readers = Vec::new();
    for _ in 0..ECHO_CHANS {
        let mut k = Command::new("ssh")
            .args(["-S", &sock_s, "-T", target, "cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn echo");
        writers.push(k.stdin.take().unwrap());
        let out = BufReader::new(k.stdout.take().unwrap());
        let (sent2, samples2) = (Arc::clone(&sent), Arc::clone(&samples));
        readers.push(std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                if let Ok(seq) = line.trim().parse::<u64>()
                    && let Some((t, ph)) = sent2.lock().unwrap().remove(&seq)
                {
                    samples2.lock().unwrap().push((ph, t.elapsed().as_micros()));
                }
            }
        }));
        kids.push(k);
    }
    std::thread::sleep(Duration::from_millis(500));

    let mut seq = 0u64;
    let mut ping_for = |secs: u64, writers: &mut Vec<ChildStdin>| {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            for w in writers.iter_mut() {
                seq += 1;
                sent.lock()
                    .unwrap()
                    .insert(seq, (Instant::now(), phase.load(Ordering::Relaxed)));
                writeln!(w, "{seq}").unwrap();
                w.flush().unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    ping_for(idle_s, &mut writers);
    phase.store(1, Ordering::Relaxed);
    let mut flood = Command::new("ssh")
        .args(["-S", &sock_s, "-T", target, "yes"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn flood");
    let mut fout = flood.stdout.take().unwrap();
    let flood_bytes = Arc::new(AtomicU64::new(0));
    let fb2 = Arc::clone(&flood_bytes);
    let freader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = fout.read(&mut buf) {
            if n == 0 {
                break;
            }
            fb2.fetch_add(n as u64, Ordering::Relaxed);
        }
    });
    ping_for(flood_s, &mut writers);
    let fb = flood_bytes.load(Ordering::Relaxed);

    flood.kill().ok(); // best-effort teardown
    flood.wait().expect("wait flood");
    freader.join().ok();
    drop(writers);
    for k in kids.iter_mut() {
        k.kill().ok();
        k.wait().expect("wait echo");
    }
    for r in readers {
        r.join().ok();
    }
    Command::new("ssh")
        .args(["-S", &sock_s, "-O", "exit", target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();
    master.wait().expect("wait master");
    report("cm", "n/a", &samples, fb, flood_s as f64);
}

// ---------------------------------------------------------------- unary / optimistic OPEN

fn wait_for(r: &mut impl Read, want_tag: u8, want_chan: u32) {
    loop {
        let (tag, chan, _) = read_frame(r).expect("frame");
        if tag == want_tag && chan == want_chan {
            return;
        }
    }
}

fn unary(target: &str, agent_path: &str, n: usize) {
    let mut child = ssh_agent(target, agent_path);
    let mut w = child.stdin.take().unwrap();
    let mut r = BufReader::new(child.stdout.take().unwrap());
    w.write_all(PREAMBLE).unwrap();
    wait_for(&mut r, HELLO, 0);
    let (mut plain, mut optimistic) = (Vec::new(), Vec::new());
    for i in 0..(n + 20) {
        for opt in [false, true] {
            let chan = 1000 + (i as u32) * 2 + opt as u32;
            let t = Instant::now();
            write_frame(&mut w, OPEN, chan, b"echo").unwrap();
            if opt {
                write_frame(&mut w, DATA, chan, &payload_for(1)).unwrap();
            } else {
                wait_for(&mut r, OPEN_OK, chan);
                write_frame(&mut w, DATA, chan, &payload_for(1)).unwrap();
            }
            wait_for(&mut r, DATA, chan);
            let us = t.elapsed().as_micros();
            if i >= 20 {
                let bucket = if opt { &mut optimistic } else { &mut plain };
                bucket.push(us);
            }
            write_frame(&mut w, CLOSE, chan, b"").unwrap();
        }
    }
    child.kill().ok(); // best-effort: tearing the connection down
    child.wait().expect("wait ssh");
    for (opt, v) in [(0, &mut plain), (1, &mut optimistic)] {
        println!(
            "UNARY optimistic={opt} p50_us={} p99_us={}",
            pct(v, 50.0),
            pct(v, 99.0)
        );
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [_, "agent"] => agent(),
        [_, "run", "mux", t, ag, win, idle, fl] => run_mux(
            t,
            ag,
            win.parse().expect("WINDOW"),
            idle.parse().expect("IDLE_S"),
            fl.parse().expect("FLOOD_S"),
        ),
        [_, "run", "cm", t, _ag, _win, idle, fl] => run_cm(
            t,
            idle.parse().expect("IDLE_S"),
            fl.parse().expect("FLOOD_S"),
        ),
        [_, "unary", t, ag, n] => unary(t, ag, n.parse().expect("N")),
        _ => {
            eprintln!("usage: see the module comment in src/main.rs");
            std::process::exit(2);
        }
    }
}
