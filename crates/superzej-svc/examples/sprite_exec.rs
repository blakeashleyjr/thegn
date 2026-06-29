//! Fast dev/diagnostic harness: run a shell command in a Sprites sandbox over the
//! native WSS exec (the same `run_exec` the env provisioner uses) and print its
//! output + exit code. For developing/validating provisioning steps against a
//! real sprite without recompiling the test suite each iteration.
//!
//!   SPRITES_TOKEN=… cargo run -p superzej-svc --example sprite_exec -- <sprite> '<sh command>'
#![allow(clippy::disallowed_macros)]

use superzej_svc::provider::{ProviderFiles, SpritesProvider};

fn main() {
    let token = std::env::var("SPRITES_TOKEN").expect("SPRITES_TOKEN");
    let args: Vec<String> = std::env::args().collect();
    let sprite = args
        .get(1)
        .expect("usage: sprite_exec <sprite> <command|--upload local remote>")
        .clone();
    let p = SpritesProvider::new("", &token, &sprite);
    let rt = tokio::runtime::Runtime::new().unwrap();

    // `sprite_exec <sprite> --upload <local> <remote>`: push a file into the sprite.
    if args.get(2).map(String::as_str) == Some("--upload") {
        let local = args.get(3).expect("--upload <local> <remote>");
        let remote = args.get(4).expect("--upload <local> <remote>");
        let data = std::fs::read(local).expect("read local");
        rt.block_on(async {
            match p.write(&sprite, remote, &data).await {
                Ok(()) => eprintln!("uploaded {} bytes → {sprite}:{remote}", data.len()),
                Err(e) => {
                    eprintln!("upload error: {e:#}");
                    std::process::exit(2);
                }
            }
        });
        return;
    }

    let cmd = args
        .get(2)
        .expect("usage: sprite_exec <sprite> <command>")
        .clone();
    // Forward a GitHub token (GH_TOKEN, else GITHUB_TOKEN) into the exec env as
    // GH_TOKEN, mirroring the real provisioner's passthrough so private clones work.
    let mut env: Vec<(String, String)> = Vec::new();
    if let Ok(tok) = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN"))
        && !tok.is_empty()
    {
        env.push(("GH_TOKEN".to_string(), tok));
    }
    rt.block_on(async {
        let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), cmd];
        match p.run_exec(&sprite, &argv, None, &env).await {
            Ok((code, out)) => {
                print!("{out}");
                eprintln!("\n[exit {code}]");
                std::process::exit(if code == 0 { 0 } else { 1 });
            }
            Err(e) => {
                eprintln!("run_exec error: {e:#}");
                std::process::exit(2);
            }
        }
    });
}
