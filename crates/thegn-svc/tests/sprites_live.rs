//! Live end-to-end test against the **real** Sprites API. `#[ignore]` so it never
//! runs in CI; it needs `SPRITES_TOKEN` and network, and it creates + destroys a
//! throwaway sprite (real cloud spend). Run:
//!
//!   SPRITES_TOKEN=… cargo test -p thegn-svc --test sprites_live -- --ignored --nocapture
//!
//! This is the ground-truth check the replay mock (`sprites_mock`) is modeled on.
// Progress output for `--ignored --nocapture`; std print is fine in this
// human-run integration test (the repo bans it only in the shipping binaries).
#![allow(clippy::disallowed_macros)]

use std::collections::HashSet;
use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::bail;
use thegn_svc::provider::{
    ExecControl, ExecFrame, ExecSession, ExecSpec, InventoryReport, Provider, ProviderCheckpoints,
    ProviderEgress, ProviderFiles, RemoteProvider, SpritesProvider,
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

/// A provider that may create and destroy: scoped like the host builds it
/// (`provider_factory::sprites_policy`), with a throwaway ledger dir.
fn scoped(token: &str, name: &str, owner: &str) -> (SpritesProvider, tempfile::TempDir) {
    let ledger = tempfile::tempdir().expect("temporary sprites ledger");
    let provider = SpritesProvider::new("", token, name)
        .with_scope(thegn_svc::sprites::ledger::OwnerScope {
            owner: owner.into(),
            account: format!("acct1-{owner}"),
        })
        .with_ledger_dir(ledger.path().to_path_buf());
    (provider, ledger)
}

/// On panic, name the sprite so it can be found and removed by hand (its
/// temporary ledger is gone, so `thegn env reap --provider sprites --unrecorded`
/// lists it).
struct LeakNote(String);

impl Drop for LeakNote {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("sprite may have leaked; remove it by hand: {}", self.0);
        }
    }
}

async fn recv_until(
    session: &mut ExecSession,
    needle: &[u8],
    budget: Duration,
) -> anyhow::Result<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut output = Vec::new();
    loop {
        let frame = tokio::time::timeout_at(deadline, session.frames.recv())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for {:?}", needle))?;
        let Some(frame) = frame else {
            bail!("exec stream closed while waiting for {:?}", needle);
        };
        match frame {
            ExecFrame::Stdout(bytes) => {
                output.extend(bytes);
                if output.windows(needle.len()).any(|window| window == needle) {
                    return Ok(output);
                }
                const MAX_CAPTURE: usize = 4 * 1024 * 1024;
                if output.len() > MAX_CAPTURE {
                    let excess = output.len() - MAX_CAPTURE;
                    output.drain(..excess);
                }
            }
            ExecFrame::Exit(code) => {
                bail!("exec exited with {code} while waiting for {:?}", needle);
            }
        }
    }
}

async fn wait_session_id(session: &mut ExecSession, budget: Duration) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(id) = session.session_id.borrow().clone() {
            return Ok(id);
        }
        tokio::time::timeout_at(deadline, session.session_id.changed())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for the exec session id"))??;
    }
}

async fn collect_for(session: &mut ExecSession, budget: Duration, cap: usize) -> (Vec<u8>, bool) {
    let deadline = tokio::time::Instant::now() + budget;
    let mut output = Vec::new();
    loop {
        let frame = match tokio::time::timeout_at(deadline, session.frames.recv()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return (output, true),
            Err(_) => return (output, false),
        };
        if let ExecFrame::Stdout(bytes) = frame {
            output.extend(bytes);
            if output.len() > cap {
                output.drain(..output.len() - cap);
            }
        }
    }
}

async fn collect_until_end(
    session: &mut ExecSession,
    budget: Duration,
    cap: usize,
) -> (Vec<u8>, bool) {
    let deadline = tokio::time::Instant::now() + budget;
    let mut output = Vec::new();
    loop {
        let frame = match tokio::time::timeout_at(deadline, session.frames.recv()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return (output, true),
            Err(_) => return (output, false),
        };
        match frame {
            ExecFrame::Stdout(bytes) => {
                output.extend(bytes);
                if output.len() > cap {
                    output.drain(..output.len() - cap);
                }
            }
            ExecFrame::Exit(_) => return (output, false),
        }
    }
}

struct WireStats {
    bytes: usize,
    elapsed: Duration,
    frames: usize,
    max_frame: usize,
    output: Vec<u8>,
}

async fn collect_wire_stats(
    session: &mut ExecSession,
    started: Instant,
    budget: Duration,
    marker: Option<&[u8]>,
    capture_output: bool,
) -> anyhow::Result<WireStats> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut bytes = 0;
    let mut frames = 0;
    let mut max_frame = 0;
    let mut output = Vec::new();
    let mut tail = Vec::new();

    loop {
        let frame = tokio::time::timeout_at(deadline, session.frames.recv())
            .await
            .map_err(|_| anyhow::anyhow!("timed out collecting exec wire stats"))?;
        let Some(frame) = frame else {
            if marker.is_none() {
                return Ok(WireStats {
                    bytes,
                    elapsed: started.elapsed(),
                    frames,
                    max_frame,
                    output,
                });
            }
            bail!("exec stream closed before wire marker");
        };
        match frame {
            ExecFrame::Stdout(chunk) => {
                frames += 1;
                max_frame = max_frame.max(chunk.len());
                let prior_tail_len = tail.len();
                let mut probe = tail;
                probe.extend_from_slice(&chunk);
                let marker_end = marker.and_then(|needle| {
                    probe
                        .windows(needle.len())
                        .position(|window| window == needle)
                        .map(|start| start + needle.len())
                });
                let consumed = marker_end
                    .map(|end| end.saturating_sub(prior_tail_len).min(chunk.len()))
                    .unwrap_or(chunk.len());
                bytes += consumed;
                if capture_output {
                    output.extend_from_slice(&chunk[..consumed]);
                }
                if marker_end.is_some() {
                    return Ok(WireStats {
                        bytes,
                        elapsed: started.elapsed(),
                        frames,
                        max_frame,
                        output,
                    });
                }
                if probe.len() > 16 {
                    probe.drain(..probe.len() - 16);
                }
                tail = probe;
            }
            ExecFrame::Exit(code) => {
                if marker.is_some() {
                    bail!("exec exited with {code} before wire marker");
                }
                return Ok(WireStats {
                    bytes,
                    elapsed: started.elapsed(),
                    frames,
                    max_frame,
                    output,
                });
            }
        }
    }
}

async fn session_absent_within(
    provider: &Provider,
    name: &str,
    session: &str,
    budget: Duration,
) -> anyhow::Result<bool> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let absent = tokio::time::timeout_at(deadline, provider.exec_session_absent(name, session))
            .await
            .map_err(|_| anyhow::anyhow!("timed out querying the exec session roster"))??;
        if absent {
            return Ok(true);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        tokio::time::sleep_until((now + Duration::from_millis(500)).min(deadline)).await;
    }
}

#[test]
#[ignore = "live: needs SPRITES_TOKEN and network; read-only (lists, creates nothing)"]
fn live_inventory_pages_read_only() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        return;
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let unpaged = SpritesProvider::new("", &token, "tg-inventory-unpaged")
            .sandbox_inventory()
            .await
            .expect("unpaged inventory");
        let paged = SpritesProvider::new("", &token, "tg-inventory-paged")
            .with_list_page_size(2)
            .sandbox_inventory()
            .await
            .expect("paged inventory");

        let InventoryReport::Complete(unpaged) = unpaged else {
            panic!("unpaged inventory must be Complete");
        };
        let InventoryReport::Complete(paged) = paged else {
            panic!("paged inventory must be Complete");
        };
        // There must be at least two continuation pages to prove that paging was used.
        if unpaged.len() < 3 {
            return;
        }
        let unpaged_ids: HashSet<&str> = unpaged.iter().map(|row| row.id.as_str()).collect();
        let paged_ids: HashSet<&str> = paged.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(unpaged_ids, paged_ids);
        assert_eq!(
            unpaged_ids.len(),
            unpaged.len(),
            "unpaged inventory has duplicates"
        );
        assert_eq!(
            paged_ids.len(),
            paged.len(),
            "paged inventory has duplicates"
        );
    });
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
    let name = format!("tglc-{}", std::process::id());
    let (sprites, _ledger) = scoped(&token, &name, "tglc-live");
    let prov = thegn_svc::provider::Provider::Sprites(sprites);
    let _leak_note = LeakNote(name.clone());
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Clean slate (ignore: may not exist yet).
        let _ = prov.destroy(&name).await; // best-effort: clean slate; may not exist yet (see above)

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
    let name = format!("tgac-{}", std::process::id());
    let (p, _ledger) = scoped(&token, &name, "tgac-live");
    let _leak_note = LeakNote(name.clone());
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
        let (claude_files, _claude_dirs) = thegn_core::envplan::agent_config_paths("claude");
        assert!(claude_files.contains(&".claude.json".to_string()));
        let (_pi_files, pi_dirs) = thegn_core::envplan::agent_config_paths("pi");
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
    use thegn_core::envplan::{EnvRequirements, PlanOpts, StepKind, plan};
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

    let name = format!("tgnix-{}", std::process::id());
    let (prov, _ledger) = scoped(&token, &name, "tgnix-live");
    let _leak_note = LeakNote(name.clone());
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

/// Live reverse-tunnel: upload a real musl `thegn` into a throwaway sprite, run
/// `thegn bridge-revtunnel <port>` inside it, pump `run_host` over the provider
/// exec with a HOST echo target, then have a process INSIDE the sprite dial
/// `127.0.0.1:<port>` and assert its bytes round-trip through the tunnel to the
/// host service and back. This exercises the ENTIRE reverse-tunnel stack against
/// real sprite infrastructure (not mocks). Needs `THEGN_MUSL` = path to a static
/// musl `thegn` (`nix build .#thegn-musl`).
#[test]
#[ignore = "live: needs SPRITES_TOKEN + THEGN_MUSL + network; creates a sprite"]
fn live_reverse_tunnel() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        eprintln!("SPRITES_TOKEN unset — skipping");
        return;
    };
    let Ok(musl) = std::env::var("THEGN_MUSL") else {
        eprintln!("THEGN_MUSL unset — skipping");
        return;
    };
    let bin = std::fs::read(&musl).expect("read musl thegn");
    let name = format!("tgtun-{}", std::process::id());
    let owner = "tgtun-live";
    let (p, ledger) = scoped(&token, &name, owner);
    let prov = thegn_svc::provider::Provider::Sprites(
        SpritesProvider::new("", &token, &name)
            .with_scope(thegn_svc::sprites::ledger::OwnerScope {
                owner: owner.into(),
                account: format!("acct1-{owner}"),
            })
            .with_ledger_dir(ledger.path().to_path_buf()),
    );
    let _leak_note = LeakNote(name.clone());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let sh = |s: &str| vec!["/bin/sh".to_string(), "-lc".to_string(), s.to_string()];

    rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        p.create().await.expect("create");
        let warm = sh("true");
        warm_retry("warm", || p.run_exec(&name, &warm, None, &[])).await;

        // Upload the musl thegn + make it executable.
        warm_retry("upload thegn", || p.write(&name, "/tmp/thegn", &bin)).await;
        let chmod = sh("chmod +x /tmp/thegn && /tmp/thegn --help >/dev/null 2>&1; echo ok");
        let (_c, o) = p.run_exec(&name, &chmod, None, &[]).await.expect("chmod");
        assert!(o.contains("ok"), "thegn runnable in sprite: {o}");

        // Host echo target (stands in for tgproxy): echo everything back.
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
        let spec = thegn_svc::provider::ExecSpec {
            argv: vec![
                "/tmp/thegn".into(),
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
        let stream = thegn_svc::revtunnel::exec_stream(session);
        tokio::spawn(thegn_svc::revtunnel::run_host(
            stream,
            thegn_svc::revtunnel::TcpDialer { addr: echo_addr },
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
    let name = format!("tgprov-{}", std::process::id());
    let (p, _ledger) = scoped(&token, &name, "tgprov-live");
    let _leak_note = LeakNote(name.clone());
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
    let name = format!("tglive-{}", std::process::id());
    let owner = "tglive-live";
    let (p, ledger) = scoped(&token, &name, owner);
    let _leak_note = LeakNote(name.clone());
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
            p.write(&name, "/workspace/tg.txt", b"hello-thegn")
        })
        .await;
        let got = p.read(&name, "/workspace/tg.txt").await.expect("read");
        assert_eq!(got, b"hello-thegn");
        let entries = p.list_dir(&name, "/workspace").await.expect("list");
        println!("ls /workspace: {entries:?}");
        assert!(entries.iter().any(|e| e.name == "tg.txt" && !e.is_dir));

        // upload_dir / download_dir round-trip via a temp dir
        let tmp = std::env::temp_dir().join(format!("tglive-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp); // best-effort: test tmp cleanup
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("top.txt"), b"top").unwrap();
        std::fs::write(tmp.join("sub/nested.txt"), b"nested").unwrap();
        p.upload_dir(&name, &tmp, "/workspace/up")
            .await
            .expect("upload_dir");
        let back = std::env::temp_dir().join(format!("tglive-down-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&back); // best-effort: test tmp cleanup
        p.download_dir(&name, "/workspace/up", &back)
            .await
            .expect("download_dir");
        assert_eq!(std::fs::read(back.join("top.txt")).unwrap(), b"top");
        assert_eq!(
            std::fs::read(back.join("sub/nested.txt")).unwrap(),
            b"nested"
        );
        println!("upload/download round-trip ok");
        let _ = std::fs::remove_dir_all(&tmp); // best-effort: test tmp cleanup
        let _ = std::fs::remove_dir_all(&back); // best-effort: test tmp cleanup

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
        let env_argv = sh("printf %s \"$TG_TEST_VAR\"");
        let (c2, o2) = p
            .run_exec(
                &name,
                &env_argv,
                None,
                &[("TG_TEST_VAR".to_string(), "xyz-42".to_string())],
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
        // A small fake stands in for the musl thegn — the handshake is what we
        // verify (push once, no-op on identical bytes, bytes land at the path).
        let prov = thegn_svc::provider::Provider::Sprites(
            SpritesProvider::new("", &token, &name)
                .with_scope(thegn_svc::sprites::ledger::OwnerScope {
                    owner: owner.into(),
                    account: format!("acct1-{owner}"),
                })
                .with_ledger_dir(ledger.path().to_path_buf()),
        );
        let fake = b"#!/bin/sh\necho fake-thegn\n";
        let pushed = warm_retry("ensure_executable", || {
            prov.ensure_executable(&name, "/workspace/.tg/thegn", fake)
        })
        .await;
        assert!(pushed, "first ensure should push");
        let again = prov
            .ensure_executable(&name, "/workspace/.tg/thegn", fake)
            .await
            .expect("second ensure");
        assert!(!again, "second ensure (same bytes) should be a no-op");
        let back_bin = p
            .read(&name, "/workspace/.tg/thegn")
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

#[test]
#[ignore = "live: needs SPRITES_TOKEN, network, creates a real sprite"]
fn live_exec_session_semantics() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        eprintln!("SPRITES_TOKEN unset — skipping");
        return;
    };
    let name = format!("tgexec-{}", std::process::id());
    let (sprites, _ledger) = scoped(&token, &name, "tgexec-live");
    let provider = Provider::Sprites(sprites);
    let _leak_note = LeakNote(name.clone());
    let rt = tokio::runtime::Runtime::new().unwrap();

    let result: anyhow::Result<()> = rt.block_on(async {
        let result = async {
            provider.ensure_exists(&name).await?;
            provider.wait_ready(&name, Duration::from_secs(120)).await?;

            let spec = ExecSpec {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "exec bash --norc --noprofile -i".into(),
                ],
                tty: true,
                cols: 120,
                rows: 40,
                env: vec![("TERM".into(), "xterm-256color".into())],
                cwd: None,
            };
            let connected_at = Instant::now();
            let mut session = provider.open_exec(&name, &spec).await?;
            println!(
                "INFO exec connect: {} ms",
                connected_at.elapsed().as_millis()
            );

            session
                .control
                .send(ExecControl::Stdin(b"stty size\r".to_vec()))
                .await?;
            let initial_size = recv_until(&mut session, b"40 120", Duration::from_secs(10)).await?;
            println!(
                "INFO stty size initial: {}",
                String::from_utf8_lossy(&initial_size).trim()
            );

            session
                .control
                .send(ExecControl::Resize {
                    cols: 100,
                    rows: 30,
                })
                .await?;
            session
                .control
                .send(ExecControl::Stdin(b"stty size\r".to_vec()))
                .await?;
            let resized = recv_until(&mut session, b"30 100", Duration::from_secs(10)).await?;
            let resized_text = String::from_utf8_lossy(&resized);
            println!("INFO stty size resized: {}", resized_text.trim());
            anyhow::ensure!(
                resized_text.contains("30 100"),
                "resized stty answer did not contain 30 100: {resized_text:?}"
            );

            let mut rtts = Vec::with_capacity(20);
            for _ in 0..20 {
                let started = Instant::now();
                session
                    .control
                    .send(ExecControl::Stdin(b"x".to_vec()))
                    .await?;
                recv_until(&mut session, b"x", Duration::from_secs(5)).await?;
                rtts.push(started.elapsed());
                session
                    .control
                    .send(ExecControl::Stdin(b"\x15".to_vec()))
                    .await?;
            }
            rtts.sort_unstable();
            let p50 = rtts[(rtts.len() - 1) * 50 / 100];
            let p95 = rtts[(rtts.len() - 1) * 95 / 100];
            println!("INFO keystroke RTT: p50={p50:?}, p95={p95:?}");

            session
                .control
                .send(ExecControl::Stdin(b"echo REPLAY-MARK\r".to_vec()))
                .await?;
            let replay_marker =
                recv_until(&mut session, b"REPLAY-MARK", Duration::from_secs(10)).await?;
            let session_id = wait_session_id(&mut session, Duration::from_secs(10)).await?;
            session.control.send(ExecControl::Close).await?;
            drop(session);

            let mut attached = provider.attach_exec(&name, &session_id, 120, 40).await?;
            let (replay, _) =
                collect_for(&mut attached, Duration::from_secs(2), 4 * 1024 * 1024).await;
            let replay_has_marker = replay
                .windows(b"REPLAY-MARK".len())
                .any(|window| window == b"REPLAY-MARK");
            let replay_starts_with_clear =
                replay.starts_with(b"\x1bc") || replay.starts_with(b"\x1b[2J");
            println!(
                "INFO reattach replay: bytes={}, REPLAY-MARK={}, starts-with-clear={}",
                replay.len(),
                replay_has_marker,
                replay_starts_with_clear
            );
            let _ = attached.control.send(ExecControl::Close).await; // best-effort: the driver may already have ended
            drop(attached);
            let session_persists =
                match session_absent_within(&provider, &name, &session_id, Duration::from_secs(5))
                    .await
                {
                    Ok(absent) => !absent,
                    Err(error) => {
                        println!("INFO session roster probe error: {error:#}");
                        true
                    }
                };
            println!(
                "INFO session persists after disconnect: {}",
                if session_persists { "yes" } else { "no" }
            );

            let mut kill_url = reqwest::Url::parse("https://api.sprites.dev/v1")?;
            kill_url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("Sprites API URL cannot hold a path"))?
                .push("sprites")
                .push(&name)
                .push("exec")
                .push(&session_id)
                .push("kill");
            kill_url
                .query_pairs_mut()
                .append_pair("signal", "SIGHUP")
                .append_pair("timeout", "5s");
            let kill_response = reqwest::Client::new()
                .post(kill_url)
                .bearer_auth(&token)
                .timeout(Duration::from_secs(15))
                .send()
                .await?;
            let kill_status = kill_response.status();
            println!("INFO kill session status: {kill_status}");
            if kill_status.is_success() {
                anyhow::ensure!(
                    session_absent_within(&provider, &name, &session_id, Duration::from_secs(10),)
                        .await?,
                    "exec session {session_id} remained present after SIGHUP"
                );
            }

            let big_spec = ExecSpec {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "cat > /tmp/big; wc -c < /tmp/big".into(),
                ],
                tty: true,
                cols: 120,
                rows: 40,
                env: vec![("TERM".into(), "xterm-256color".into())],
                cwd: None,
            };
            let mut big_session = provider.open_exec(&name, &big_spec).await?;
            let mut chunk = "x".repeat(63);
            chunk.push('\n');
            let big_input = chunk.repeat(16_384).into_bytes();
            let big_send = big_session
                .control
                .send(ExecControl::Stdin(big_input))
                .await;
            let eof_send = big_session
                .control
                .send(ExecControl::Stdin(b"\x04".to_vec()))
                .await;
            let (big_output, receiver_dropped) =
                collect_until_end(&mut big_session, Duration::from_secs(15), 2 * 1024 * 1024).await;
            let byte_count = String::from_utf8_lossy(&big_output)
                .split_whitespace()
                .find_map(|word| word.parse::<usize>().ok());
            println!(
                "INFO 1 MiB stdin: socket dropped={}, byte count={byte_count:?}",
                big_send.is_err() || eof_send.is_err() || receiver_dropped
            );
            let _ = big_session.control.send(ExecControl::Close).await; // best-effort: the driver may already have ended
            drop(big_session);

            // Keep the first observed marker available in the live trace even
            // when the provider's replay buffer is empty.
            println!("INFO command marker bytes: {}", replay_marker.len());
            anyhow::Ok(())
        }
        .await;

        let destroy = provider.destroy(&name).await;
        destroy.expect("destroy live exec sprite");
        result
    });

    result.expect("live exec session semantics");
}

#[test]
#[ignore = "live: needs SPRITES_TOKEN, network, creates a real sprite"]
fn live_exec_wire_throughput() {
    let Ok(token) = std::env::var("SPRITES_TOKEN") else {
        println!("INFO SPRITES_TOKEN unset — skipping");
        return;
    };
    let name = format!("tgwire-{}", std::process::id());
    let (sprites, _ledger) = scoped(&token, &name, "tgwire-live");
    let provider = Provider::Sprites(sprites);
    let _leak_note = LeakNote(name.clone());
    let rt = tokio::runtime::Runtime::new().unwrap();

    let result: anyhow::Result<()> = rt.block_on(async {
        let result = async {
            provider.ensure_exists(&name).await?;
            provider.wait_ready(&name, Duration::from_secs(120)).await?;

            let print_stats = |label: &str, stats: &WireStats| {
                let seconds = stats.elapsed.as_secs_f64().max(f64::EPSILON);
                let kb_per_second = stats.bytes as f64 / 1024.0 / seconds;
                let average_frame = if stats.frames == 0 {
                    0.0
                } else {
                    stats.bytes as f64 / stats.frames as f64
                };
                println!(
                    "INFO {label}: bytes={}, ms={}, KB/s={kb_per_second:.1}, frames={}, avg={average_frame:.1}, max={}",
                    stats.bytes,
                    stats.elapsed.as_millis(),
                    stats.frames,
                    stats.max_frame,
                );
            };
            let sh = |script: &str| {
                vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    script.to_string(),
                ]
            };

            let mut tty_session = provider
                .open_exec(
                    &name,
                    &ExecSpec {
                        argv: vec![
                            "/bin/bash".into(),
                            "--norc".into(),
                            "--noprofile".into(),
                            "-i".into(),
                        ],
                        tty: true,
                        cols: 120,
                        rows: 40,
                        env: vec![("TERM".into(), "xterm-256color".into())],
                        cwd: None,
                    },
                )
                .await?;
            tty_session
                .control
                .send(ExecControl::Stdin(b"stty size\r".to_vec()))
                .await?;
            let initial_size = collect_for(&mut tty_session, Duration::from_secs(3), 4096)
                .await
                .0;
            println!(
                "INFO initial size: {}",
                String::from_utf8_lossy(&initial_size).trim()
            );

            let tty_started = Instant::now();
            tty_session
                .control
                .send(ExecControl::Stdin(
                    b"yes 0123456789 | head -n 400000; printf '%s%s\\n' __RP END__\r".to_vec(),
                ))
                .await?;
            let tty_stats = collect_wire_stats(
                &mut tty_session,
                tty_started,
                Duration::from_secs(60),
                Some(b"__RPEND__"),
                false,
            )
            .await?;
            print_stats("tty flood", &tty_stats);
            anyhow::ensure!(
                tty_stats.bytes >= 4_400_000,
                "tty flood byte count was {}, expected at least 4400000",
                tty_stats.bytes
            );
            tty_session.control.send(ExecControl::Close).await?;
            drop(tty_session);

            let mut non_tty_session = provider
                .open_exec(
                    &name,
                    &ExecSpec {
                        argv: sh("yes 0123456789 | head -n 400000"),
                        tty: false,
                        cols: 0,
                        rows: 0,
                        env: Vec::new(),
                        cwd: None,
                    },
                )
                .await?;
            let non_tty_started = Instant::now();
            let non_tty_stats = collect_wire_stats(
                &mut non_tty_session,
                non_tty_started,
                Duration::from_secs(60),
                None,
                false,
            )
            .await?;
            print_stats("non-tty flood", &non_tty_stats);
            anyhow::ensure!(
                non_tty_stats.bytes == 4_400_000,
                "non-tty flood byte count was {}, expected 4400000",
                non_tty_stats.bytes
            );
            non_tty_session.control.send(ExecControl::Close).await?;
            drop(non_tty_session);

            let baseline_started = Instant::now();
            let (_, baseline_output) = provider
                .run_exec(
                    &name,
                    &sh("yes 0123456789 | head -n 400000 | wc -c"),
                    None,
                    &[],
                )
                .await?;
            let baseline_bytes = baseline_output.trim().parse::<usize>()?;
            println!(
                "INFO server baseline: bytes={baseline_bytes}, ms={}",
                baseline_started.elapsed().as_millis()
            );
            anyhow::ensure!(
                baseline_bytes == 4_400_000,
                "server baseline byte count was {baseline_bytes}, expected 4400000"
            );

            let bulk_spec = ExecSpec {
                argv: sh("wc -c"),
                tty: false,
                cols: 0,
                rows: 0,
                env: Vec::new(),
                cwd: None,
            };
            let mut bulk_session = provider.open_exec(&name, &bulk_spec).await?;
            let bulk_started = Instant::now();
            for _ in 0..32 {
                bulk_session
                    .control
                    .send(ExecControl::Stdin(vec![b'x'; 32 * 1024]))
                    .await?;
            }
            let (closed_tx, _) = tokio::sync::mpsc::channel(1);
            drop(std::mem::replace(&mut bulk_session.control, closed_tx));
            let bulk_stats = collect_wire_stats(
                &mut bulk_session,
                bulk_started,
                Duration::from_secs(30),
                None,
                true,
            )
            .await?;
            let bulk_output = String::from_utf8_lossy(&bulk_stats.output);
            println!(
                "INFO stdin bulk: bytes=1048576, ms={}, output={bulk_output:?}",
                bulk_stats.elapsed.as_millis()
            );
            anyhow::ensure!(
                bulk_output.contains("1048576"),
                "bulk stdin output did not contain 1048576: {bulk_output:?}"
            );
            drop(bulk_session);

            let mut byte_session = provider.open_exec(&name, &bulk_spec).await?;
            let byte_started = Instant::now();
            for _ in 0..65_536 {
                byte_session
                    .control
                    .send(ExecControl::Stdin(vec![b'x']))
                    .await?;
            }
            let (closed_tx, _) = tokio::sync::mpsc::channel(1);
            drop(std::mem::replace(&mut byte_session.control, closed_tx));
            let byte_stats = collect_wire_stats(
                &mut byte_session,
                byte_started,
                Duration::from_secs(60),
                None,
                true,
            )
            .await?;
            let byte_output = String::from_utf8_lossy(&byte_stats.output);
            let messages_per_second = 65_536.0 / byte_stats.elapsed.as_secs_f64().max(f64::EPSILON);
            println!(
                "INFO stdin per byte: bytes=65536, ms={}, messages/s={messages_per_second:.1}, output={byte_output:?}",
                byte_stats.elapsed.as_millis()
            );
            anyhow::ensure!(
                byte_output.contains("65536"),
                "per-byte stdin output did not contain 65536: {byte_output:?}"
            );
            drop(byte_session);

            anyhow::Ok(())
        }
        .await;

        let destroy = provider.destroy(&name).await;
        destroy.expect("destroy live wire probe sprite");
        result
    });

    result.expect("live exec wire throughput");
}
