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
