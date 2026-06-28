//! Live end-to-end test against the **real** Sprites API. `#[ignore]` so it never
//! runs in CI; it needs `SPRITES_TOKEN` and network, and it creates + destroys a
//! throwaway sprite (real cloud spend). Run:
//!
//!   SPRITES_TOKEN=… cargo test -p superzej-svc --test sprites_live -- --ignored --nocapture
//!
//! This is the ground-truth check the replay mock (`sprites_mock`) is modeled on.
// Progress output for `--ignored --nocapture`; std print is fine in this
// human-run integration test (the repo bans it only in the shipping binaries).
#![allow(clippy::disallowed_macros)]

use std::future::Future;
use std::time::Duration;

use superzej_svc::provider::{
    ProviderCheckpoints, ProviderEgress, ProviderFiles, RemoteProvider, SpritesProvider,
};

/// Retry an op that fails while the sprite is still cold-starting (fs/checkpoint
/// 404/409 until warm), up to ~60s.
async fn warm_retry<T, F, Fut>(label: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut last = None;
    for _ in 0..30 {
        match f().await {
            Ok(v) => return v,
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    panic!("{label} never succeeded: {last:?}");
}

/// Live: the sandbox **lifecycle** the worktree open/delete paths depend on —
/// `ensure_exists` creates a missing sandbox (recreate-if-cleaned-out-of-band),
/// is a no-op when it already exists, and `destroy` is idempotent (a second
/// delete / a delete of an already-gone sandbox both succeed). This is the live
/// proof behind "recreate if one is missing" + "cleanup on delete".
#[test]
#[ignore = "live: needs SPRITES_TOKEN, network, creates a real sprite"]
fn live_sandbox_lifecycle() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        eprintln!("SPRITES_TOKEN unset — skipping");
        return;
    };
    let name = format!("szlc-{}", std::process::id());
    let prov = superzej_svc::provider::Provider::Sprites(SpritesProvider::new("", &token, &name));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Clean slate (ignore: may not exist yet).
        let _ = prov.destroy(&name).await;

        // ensure_exists on a missing sandbox CREATES it (returns true).
        assert!(
            prov.ensure_exists(&name).await.expect("ensure create"),
            "ensure_exists should create a missing sandbox"
        );
        // ensure_exists when it already exists is a no-op (returns false).
        assert!(
            !prov.ensure_exists(&name).await.expect("ensure noop"),
            "ensure_exists should be a no-op when present"
        );

        // destroy works, and a SECOND destroy is idempotent (404 = already gone) —
        // racing a TTL/manual delete must not error the teardown path.
        prov.destroy(&name).await.expect("destroy");
        prov.destroy(&name)
            .await
            .expect("destroy idempotent on 404");

        // After an out-of-band delete, ensure_exists RECREATES (returns true) —
        // this is the "a sandbox cleaned up without us" recovery.
        assert!(
            prov.ensure_exists(&name).await.expect("ensure recreate"),
            "ensure_exists should recreate after out-of-band delete"
        );
        prov.destroy(&name).await.expect("final destroy");
        println!("sandbox lifecycle (create/noop/idempotent-destroy/recreate) ok");
    });
}

/// Live: agent **customizations** land in the sandbox. Uploads the config paths
/// the host's `upload_agent_configs` uses (resolved from the SAME
/// `envplan::agent_config_paths`) — a `.claude.json` file and a nested `.pi`
/// extension/skill tree — into the sandbox `$HOME`, then reads them back and
/// asserts byte-for-byte. Proves claude/pi/hermes customizations are carried in.
#[test]
#[ignore = "live: needs SPRITES_TOKEN, network, creates a real sprite"]
fn live_agent_customizations() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        eprintln!("SPRITES_TOKEN unset — skipping");
        return;
    };
    let name = format!("szac-{}", std::process::id());
    let p = SpritesProvider::new("", &token, &name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sh = |s: &str| vec!["/bin/sh".to_string(), "-lc".to_string(), s.to_string()];
    rt.block_on(async {
        p.create().await.expect("create");
        let warm = sh("true");
        warm_retry("warm", || p.run_exec(&name, &warm, None, &[])).await;

        // Resolve the sandbox user's real $HOME (sprites exec as `sprite`).
        let home_cmd = sh("printf %s \"$HOME\"");
        let (_c, home) = p.run_exec(&name, &home_cmd, None, &[]).await.expect("home");
        let home = home.trim();
        assert!(home.starts_with('/'), "resolved $HOME: {home:?}");

        // The host uploader maps each agent to (files, dirs) via this exact fn.
        let (claude_files, _claude_dirs) = superzej_core::envplan::agent_config_paths("claude");
        assert!(claude_files.contains(&".claude.json".to_string()));
        let (_pi_files, pi_dirs) = superzej_core::envplan::agent_config_paths("pi");
        assert!(pi_dirs.contains(&".pi".to_string()));

        // Upload a claude top-level config FILE + a nested pi customization (an
        // extension under ~/.pi/agent/extensions) exactly where the agents read.
        let claude_path = format!("{home}/.claude.json");
        let pi_path = format!("{home}/.pi/agent/extensions/my-ext.ts");
        let claude_json = br#"{"theme":"dark","mcpServers":{}}"#;
        let pi_ext = b"export default { name: 'my-ext' }\n";
        warm_retry("upload .claude.json", || {
            p.write(&name, &claude_path, claude_json)
        })
        .await;
        warm_retry("upload .pi ext", || p.write(&name, &pi_path, pi_ext)).await;

        // Read both back through the fs API and assert byte-for-byte.
        let got_claude = p
            .read(&name, &claude_path)
            .await
            .expect("read .claude.json");
        assert_eq!(got_claude, claude_json, "claude config round-trip");
        let got_pi = p.read(&name, &pi_path).await.expect("read pi ext");
        assert_eq!(got_pi, pi_ext, "pi customization round-trip");

        println!("agent customizations (claude.json + ~/.pi extension) landed in {home}");

        p.destroy(&name).await.expect("destroy");
    });
}

/// Live HEAVY: run the REAL Nix-install provisioning step (from the public
/// `envplan::plan`) in a throwaway sprite and verify `nix` is usable afterward —
/// the definitive "the declared dev env actually comes up" check. Slow (Nix
/// install is minutes). Catches sh-compat bugs the unit tests can't.
#[test]
#[ignore = "live SLOW: installs Nix in a sprite (minutes); needs SPRITES_TOKEN"]
fn live_provision_nix() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        eprintln!("SPRITES_TOKEN unset — skipping");
        return;
    };
    use superzej_core::envplan::{EnvRequirements, PlanOpts, StepKind, plan};
    let req = EnvRequirements {
        nix_flake_devshell: true,
        direnv: true,
        direnv_uses_flake: true,
        ..Default::default()
    };
    let opts = PlanOpts {
        origin: None,
        checkpoint: false,
        ..Default::default()
    };
    let nix_script = plan(&req, &opts)
        .steps
        .into_iter()
        .find(|s| s.id == "nix")
        .and_then(|s| match s.kind {
            StepKind::Exec(x) => Some(x),
            _ => None,
        })
        .expect("plan has a nix step");

    let name = format!("sznix-{}", std::process::id());
    let prov = SpritesProvider::new("", &token, &name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sh = |s: &str| vec!["/bin/sh".to_string(), "-lc".to_string(), s.to_string()];
    rt.block_on(async {
        prov.create().await.expect("create");
        println!("created {name}; installing Nix (slow)…");
        let warm = sh("true");
        warm_retry("warm", || prov.run_exec(&name, &warm, None, &[])).await;

        let nix_argv = sh(&format!("{nix_script} 2>&1"));
        let (c, o) = prov
            .run_exec(&name, &nix_argv, None, &[])
            .await
            .expect("nix install exec");
        let tail = &o[o.len().saturating_sub(1500)..];
        assert_eq!(c, 0, "nix install failed (tail): {tail}");

        let ver = sh("export PATH=\"$HOME/.nix-profile/bin:$PATH\"; nix --version");
        let (vc, vo) = prov
            .run_exec(&name, &ver, None, &[])
            .await
            .expect("nix --version");
        assert_eq!(vc, 0, "nix not usable after install: {vo}");
        assert!(
            vo.to_lowercase().contains("nix"),
            "nix --version output: {vo}"
        );
        println!("nix usable in sprite: {}", vo.trim());

        prov.destroy(&name).await.expect("destroy");
        println!("destroyed {name}");
    });
}

/// Live reverse-tunnel: upload a real musl `szhost` into a throwaway sprite, run
/// `szhost bridge-revtunnel <port>` inside it, pump `run_host` over the provider
/// exec with a HOST echo target, then have a process INSIDE the sprite dial
/// `127.0.0.1:<port>` and assert its bytes round-trip through the tunnel to the
/// host service and back. This exercises the ENTIRE reverse-tunnel stack against
/// real sprite infrastructure (not mocks). Needs `SZHOST_MUSL` = path to a static
/// musl `szhost` (`nix build .#szhost-musl`).
#[test]
#[ignore = "live: needs SPRITES_TOKEN + SZHOST_MUSL + network; creates a sprite"]
fn live_reverse_tunnel() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        eprintln!("SPRITES_TOKEN unset — skipping");
        return;
    };
    let Ok(musl) = std::env::var("SZHOST_MUSL") else {
        eprintln!("SZHOST_MUSL unset — skipping");
        return;
    };
    let bin = std::fs::read(&musl).expect("read musl szhost");
    let name = format!("sztun-{}", std::process::id());
    let p = SpritesProvider::new("", &token, &name);
    let prov = superzej_svc::provider::Provider::Sprites(SpritesProvider::new("", &token, &name));
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sh = |s: &str| vec!["/bin/sh".to_string(), "-lc".to_string(), s.to_string()];

    rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        p.create().await.expect("create");
        let warm = sh("true");
        warm_retry("warm", || p.run_exec(&name, &warm, None, &[])).await;

        // Upload the musl szhost + make it executable.
        warm_retry("upload szhost", || p.write(&name, "/tmp/szhost", &bin)).await;
        let chmod = sh("chmod +x /tmp/szhost && /tmp/szhost --help >/dev/null 2>&1; echo ok");
        let (_c, o) = p.run_exec(&name, &chmod, None, &[]).await.expect("chmod");
        assert!(o.contains("ok"), "szhost runnable in sprite: {o}");

        // Host echo target (stands in for szproxy): echo everything back.
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = echo.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if s.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        // Start the in-sprite tunnel agent + the host pump.
        let spec = superzej_svc::provider::ExecSpec {
            argv: vec![
                "/tmp/szhost".into(),
                "bridge-revtunnel".into(),
                "18080".into(),
            ],
            tty: false,
            cols: 0,
            rows: 0,
            env: Vec::new(),
            cwd: None,
        };
        let session = prov.open_exec(&name, &spec).await.expect("open bridge-revtunnel");
        let stream = superzej_svc::revtunnel::exec_stream(session);
        tokio::spawn(superzej_svc::revtunnel::run_host(
            stream,
            superzej_svc::revtunnel::TcpDialer { addr: echo_addr },
        ));

        // From INSIDE the sprite, dial 127.0.0.1:18080 (the tunnel) and round-trip.
        let client = sh(
            "for i in 1 2 3 4 5 6; do \
               out=$(bash -c 'exec 3<>/dev/tcp/127.0.0.1/18080 2>/dev/null && printf %s tunneltest >&3 && head -c 10 <&3'); \
               [ \"$out\" = tunneltest ] && { printf %s \"$out\"; exit 0; }; sleep 2; \
             done; echo TIMEOUT",
        );
        let (_cc, co) = p.run_exec(&name, &client, None, &[]).await.expect("tunnel client");
        assert!(
            co.contains("tunneltest"),
            "bytes round-tripped sprite→tunnel→host-echo→back: {co:?}"
        );
        println!("reverse tunnel round-trip through real sprite ok");

        p.destroy(&name).await.expect("destroy");
    });
}

/// Live provisioning smoke: create a throwaway sprite and run the real
/// workspace + clone steps (a public repo, so no token needed) through
/// `run_exec`, asserting the repo lands in `/workspace`. Validates the env
/// provisioner's core exec path end-to-end against the actual Sprites API
/// (skips the minutes-long Nix install — that's the heavier manual check).
#[test]
#[ignore = "live: needs SPRITES_TOKEN, network, creates a real sprite"]
fn live_provision_clone() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        eprintln!("SPRITES_TOKEN unset — skipping");
        return;
    };
    let name = format!("szprov-{}", std::process::id());
    let p = SpritesProvider::new("", &token, &name);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sh = |s: &str| vec!["/bin/sh".to_string(), "-lc".to_string(), s.to_string()];

    rt.block_on(async {
        p.create().await.expect("create");
        println!("created {name}");
        // Warm: retry a trivial exec until the VM accepts connections.
        let warm = sh("true");
        warm_retry("warm", || p.run_exec(&name, &warm, None, &[])).await;

        // The real provisioning shape: mkdir workspace, then clone into it.
        let clone = sh("mkdir -p /workspace && cd /workspace && \
             git clone --depth 1 https://github.com/octocat/Hello-World . 2>&1 && ls -a");
        let (code, out) = p
            .run_exec(&name, &clone, None, &[])
            .await
            .expect("clone exec");
        assert_eq!(code, 0, "clone failed: {out}");
        assert!(out.contains("README"), "repo files present: {out}");

        // The git_auth credential-helper script is a safe no-op without a token.
        let auth = sh("git config --global --add safe.directory '*'; \
             cd /workspace && git rev-parse --is-inside-work-tree");
        let (ac, ao) = p
            .run_exec(&name, &auth, None, &[])
            .await
            .expect("auth exec");
        assert_eq!(ac, 0, "git auth/repo check: {ao}");
        assert!(ao.contains("true"), "workspace is a git repo: {ao}");
        println!("provision clone+auth ok");

        p.destroy(&name).await.expect("destroy");
        println!("destroyed {name}");
    });
}

#[test]
#[ignore = "live: needs SPRITES_TOKEN, network, and creates a real sprite"]
fn live_end_to_end() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        eprintln!("SPRITES_TOKEN unset — skipping");
        return;
    };
    let name = format!("szlive-{}", std::process::id());
    let p = SpritesProvider::new("", &token, &name);
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(async {
        // lifecycle
        let h = p.create().await.expect("create");
        assert_eq!(h.id, name);
        println!("created {name}");

        // egress translate (cold-safe)
        p.set_network_policy(&name, &["github.com".into()], &["evil.example".into()])
            .await
            .expect("set policy");
        let rules = p.get_network_policy(&name).await.expect("get policy");
        println!("policy: {rules:?}");
        assert!(
            rules
                .iter()
                .any(|r| r.domain == "github.com" && r.action == "allow")
        );
        assert!(
            rules
                .iter()
                .any(|r| r.domain == "evil.example" && r.action == "deny")
        );
        assert!(rules.iter().any(|r| r.domain == "*" && r.action == "deny"));

        // file-sync primitives (retry until warm)
        warm_retry("fs write", || {
            p.write(&name, "/workspace/sz.txt", b"hello-superzej")
        })
        .await;
        let got = p.read(&name, "/workspace/sz.txt").await.expect("read");
        assert_eq!(got, b"hello-superzej");
        let entries = p.list_dir(&name, "/workspace").await.expect("list");
        println!("ls /workspace: {entries:?}");
        assert!(entries.iter().any(|e| e.name == "sz.txt" && !e.is_dir));

        // upload_dir / download_dir round-trip via a temp dir
        let tmp = std::env::temp_dir().join(format!("szlive-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("top.txt"), b"top").unwrap();
        std::fs::write(tmp.join("sub/nested.txt"), b"nested").unwrap();
        p.upload_dir(&name, &tmp, "/workspace/up")
            .await
            .expect("upload_dir");
        let back = std::env::temp_dir().join(format!("szlive-down-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&back);
        p.download_dir(&name, "/workspace/up", &back)
            .await
            .expect("download_dir");
        assert_eq!(std::fs::read(back.join("top.txt")).unwrap(), b"top");
        assert_eq!(
            std::fs::read(back.join("sub/nested.txt")).unwrap(),
            b"nested"
        );
        println!("upload/download round-trip ok");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&back);

        // exec primitives (run_exec): the CLI/bridge-free one-shot channel the env
        // provisioner uses. Validates (exit, output) capture + env passing (P0a).
        let sh = |s: &str| vec!["/bin/sh".to_string(), "-lc".to_string(), s.to_string()];
        let echo_argv = sh("echo hi; uname -s");
        let (code, out) = warm_retry("run_exec", || p.run_exec(&name, &echo_argv, None, &[])).await;
        assert_eq!(code, 0, "run_exec echo exit 0");
        assert!(
            out.contains("hi") && out.contains("Linux"),
            "run_exec captured stdout: {out:?}"
        );
        // Env passing (P0a): the var must reach the in-sprite process.
        let env_argv = sh("printf %s \"$SZ_TEST_VAR\"");
        let (c2, o2) = p
            .run_exec(
                &name,
                &env_argv,
                None,
                &[("SZ_TEST_VAR".to_string(), "xyz-42".to_string())],
            )
            .await
            .expect("run_exec env");
        assert_eq!(c2, 0);
        assert_eq!(o2.trim(), "xyz-42", "injected env var visible in sprite");
        // Non-zero exit is surfaced (not swallowed).
        let exit_argv = sh("exit 7");
        let (c3, _) = p
            .run_exec(&name, &exit_argv, None, &[])
            .await
            .expect("run_exec exit code");
        assert_eq!(c3, 7, "non-zero exit propagates");
        println!("run_exec (exit/output/env) ok");

        // resident-bridge binary push: idempotent content handshake (8-B.3).
        // A small fake stands in for the musl szhost — the handshake is what we
        // verify (push once, no-op on identical bytes, bytes land at the path).
        let prov =
            superzej_svc::provider::Provider::Sprites(SpritesProvider::new("", &token, &name));
        let fake = b"#!/bin/sh\necho fake-szhost\n";
        let pushed = warm_retry("ensure_executable", || {
            prov.ensure_executable(&name, "/workspace/.sz/szhost", fake)
        })
        .await;
        assert!(pushed, "first ensure should push");
        let again = prov
            .ensure_executable(&name, "/workspace/.sz/szhost", fake)
            .await
            .expect("second ensure");
        assert!(!again, "second ensure (same bytes) should be a no-op");
        let back_bin = p
            .read(&name, "/workspace/.sz/szhost")
            .await
            .expect("read pushed");
        assert_eq!(back_bin, fake, "pushed bytes round-trip");
        println!("bridge binary push idempotent ok");

        // checkpoint + list + restore (retry until warm)
        let cp = warm_retry("checkpoint", || p.checkpoint(&name, Some("livetest"))).await;
        println!("checkpoint id: {cp}");
        let cps = p.list_checkpoints(&name).await.expect("list checkpoints");
        assert!(cps.iter().any(|c| c.id == cp));
        p.restore(&name, &cp).await.expect("restore");
        println!("restored {cp}");

        // teardown
        p.destroy(&name).await.expect("destroy");
        println!("destroyed {name}");
    });
}
