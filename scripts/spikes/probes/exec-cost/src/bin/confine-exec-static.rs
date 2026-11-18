//! WS03-P0-02 subject 2: the "small static binary" candidate.
//! `confine-exec-static __sandbox-exec -- <argv...>` execve()s <argv...> and
//! does nothing else (no policy decoding), like the future helper's dispatch.

use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("__sandbox-exec") {
        eprintln!("usage: confine-exec-static __sandbox-exec -- <argv...>");
        std::process::exit(2);
    }
    let Some(pos) = args.iter().position(|a| a == "--") else {
        eprintln!("missing --");
        std::process::exit(2);
    };
    let payload = &args[pos + 1..];
    let Some((prog, rest)) = payload.split_first() else {
        eprintln!("empty payload");
        std::process::exit(2);
    };
    let err = Command::new(prog).args(rest).exec();
    eprintln!("exec failed: {err}");
    std::process::exit(126);
}
