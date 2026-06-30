//! Embedded host Nix binary cache.
//!
//! Serves the HOST `/nix/store` as a live HTTP binary cache (the standard
//! narinfo + NAR protocol) from a background task in the szhost process. A sprite
//! reaches it over the per-worktree reverse tunnel (a sandbox loopback port →
//! this server) and adds it as a nix substituter, so an in-sprite `nix develop`
//! *substitutes* prebuilt store paths from the host instead of building from
//! source. This generalizes the one-shot, per-devShell `file://` push
//! (`agent::push_devshell_closure`) into a substituter covering the whole host
//! store.
//!
//! Trust: paths are served UNSIGNED and the sprite nix.conf sets
//! `require-sigs = false` (see `envplan::nix_install_script`). That's safe here —
//! the cache is reachable ONLY over the per-sprite loopback tunnel (never network
//! exposed), the sandbox store is single-user and owned by the sandbox user, and
//! the narinfo `NarHash` still integrity-checks every NAR against its content. A
//! signed-cache variant (host key + `trusted-public-keys`) is a future option.
//!
//! Everything is async (`tokio::process` + streamed bodies) so a multi-GB NAR
//! never blocks the event loop; concurrent dumps are bounded by a semaphore.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Path as AxumPath;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::Stream;
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;
use tokio_util::io::ReaderStream;

const STORE_DIR: &str = "/nix/store";
/// The loopback port the reverse tunnel binds INSIDE the sandbox for the host
/// cache; the sprite nix.conf substituter points at `http://127.0.0.1:<this>`.
/// Distinct from the llm-proxy tunnel port. The host side binds an ephemeral port
/// and the tunnel maps sandbox `SANDBOX_PORT` → that host port.
pub const SANDBOX_PORT: u16 = 8484;
/// Bound concurrent `nix-store --dump` subprocesses so a burst of substitutions
/// can't fork-bomb the host. Each held permit rides its NAR stream's lifetime.
static NAR_SEM: Semaphore = Semaphore::const_new(8);

/// A running cache server. Holding it keeps the background task alive; the bound
/// `port` is wired into the per-sprite reverse tunnel as the host target.
pub struct NixCacheHandle {
    pub port: u16,
    _task: tokio::task::JoinHandle<()>,
}

/// Bind `addr` (use port 0 for an ephemeral loopback port) and serve the cache in
/// a detached background task. Returns the actually-bound port.
pub async fn serve(addr: SocketAddr) -> anyhow::Result<NixCacheHandle> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router()).await {
            tracing::warn!(target: "szhost::nixcache", error = %e, "nix cache server exited");
        }
    });
    tracing::info!(target: "szhost::nixcache", port, "embedded host nix cache serving {STORE_DIR}");
    Ok(NixCacheHandle { port, _task: task })
}

fn router() -> Router {
    // One root handler dispatches `nix-cache-info` + `<hash>.narinfo` (avoids any
    // static-vs-param route conflict); `/nar/{name}` is a deeper path, no overlap.
    Router::new()
        .route("/{file}", get(root_file))
        .route("/nar/{name}", get(nar))
}

/// Advertise the store + a priority BELOW cache.nixos.org (40) so nix prefers the
/// host only for paths it actually has, falling through to public caches otherwise.
fn nix_cache_info() -> Response {
    (
        [(header::CONTENT_TYPE, "text/x-nix-cache-info")],
        format!("StoreDir: {STORE_DIR}\nWantMassQuery: 1\nPriority: 30\n"),
    )
        .into_response()
}

/// Validate a 32-char Nix base32 store-path hash (alphabet omits e,o,t,u) before
/// touching the filesystem — defends the `<hash>-*` lookup against traversal/junk.
fn valid_hash(h: &str) -> bool {
    h.len() == 32
        && h.bytes().all(
            |b| matches!(b, b'0'..=b'9' | b'a'..=b'd' | b'f'..=b'n' | b'p'..=b's' | b'v'..=b'z'),
        )
}

/// Resolve a store-path hash to its full path by finding the single
/// `/nix/store/<hash>-*` entry. `None` if the host store lacks it (→ 404, so nix
/// falls through to other substituters).
fn resolve_hash(hash: &str) -> Option<PathBuf> {
    if !valid_hash(hash) {
        return None;
    }
    let prefix = format!("{hash}-");
    std::fs::read_dir(STORE_DIR).ok()?.find_map(|e| {
        let e = e.ok()?;
        let name = e.file_name();
        let name = name.to_str()?;
        // Match the store dir itself, not files under it (.narinfo etc. live in the
        // cache, not the store; store entries are `<hash>-<name>`).
        (name.starts_with(&prefix) && !name.ends_with(".drv")).then(|| e.path())
    })
}

/// Root dispatch: `GET /nix-cache-info` or `GET /{hash}.narinfo`.
async fn root_file(AxumPath(file): AxumPath<String>) -> Response {
    if file == "nix-cache-info" {
        return nix_cache_info();
    }
    let Some(hash) = file.strip_suffix(".narinfo") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(path) = resolve_hash(hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match narinfo_text(hash, &path).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/x-nix-narinfo")], text).into_response(),
        Err(e) => {
            tracing::debug!(target: "szhost::nixcache", %hash, error = %e, "narinfo failed");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// Build the narinfo text from `nix path-info --json`. References are emitted as
/// basenames (the narinfo format), the NAR is served uncompressed, and the
/// content-addressed `NarHash` lets the client integrity-check the download.
async fn narinfo_text(hash: &str, path: &std::path::Path) -> anyhow::Result<String> {
    let out = Command::new("nix")
        .args(["--extra-experimental-features", "nix-command"])
        .arg("path-info")
        .arg("--json")
        .arg(path)
        .output()
        .await?;
    anyhow::ensure!(out.status.success(), "nix path-info exited nonzero");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    // Newer nix returns an object keyed by path; older returns a single-element
    // array. Take the one record either way.
    let rec = match &v {
        serde_json::Value::Object(m) => m.values().next(),
        serde_json::Value::Array(a) => a.first(),
        _ => None,
    }
    .ok_or_else(|| anyhow::anyhow!("empty path-info"))?;

    let nar_hash = rec
        .get("narHash")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow::anyhow!("no narHash"))?;
    let nar_size = rec
        .get("narSize")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("no narSize"))?;
    let basename = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let refs: Vec<String> = rec
        .get("references")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|r| r.as_str()).map(basename).collect())
        .unwrap_or_default();

    let mut s = String::new();
    s.push_str(&format!("StorePath: {}\n", path.display()));
    s.push_str(&format!("URL: nar/{hash}.nar\n"));
    s.push_str("Compression: none\n");
    s.push_str(&format!("NarHash: {nar_hash}\n"));
    s.push_str(&format!("NarSize: {nar_size}\n"));
    s.push_str(&format!("References: {}\n", refs.join(" ")));
    if let Some(d) = rec
        .get("deriver")
        .and_then(|x| x.as_str())
        .filter(|d| !d.is_empty())
    {
        s.push_str(&format!("Deriver: {}\n", basename(d)));
    }
    Ok(s)
}

/// `GET /nar/{hash}.nar` — stream the NAR for a host store path via
/// `nix-store --dump`, with constant memory (the tunnel applies backpressure).
async fn nar(AxumPath(name): AxumPath<String>) -> Response {
    let hash = name.strip_suffix(".nar").unwrap_or(&name);
    let Some(path) = resolve_hash(hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Bound concurrent dumps; the permit lives as long as the stream.
    let Ok(permit) = NAR_SEM.acquire().await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut child = match Command::new("nix-store")
        .arg("--dump")
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true) // client disconnect drops the stream → kills the dump
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(target: "szhost::nixcache", error = %e, "nix-store --dump spawn failed");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let Some(stdout) = child.stdout.take() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let stream = NarStream {
        inner: ReaderStream::new(stdout),
        _child: child,
        _permit: permit,
    };
    (
        [(header::CONTENT_TYPE, "application/x-nix-nar")],
        Body::from_stream(stream),
    )
        .into_response()
}

/// A NAR byte stream that owns the `nix-store --dump` child (so it's killed on
/// client disconnect via `kill_on_drop`) and the concurrency permit (released
/// when streaming ends). Both `ReaderStream<ChildStdout>` and `Child` are `Unpin`,
/// so the delegate is a plain forward.
struct NarStream {
    inner: ReaderStream<tokio::process::ChildStdout>,
    _child: Child,
    _permit: tokio::sync::SemaphorePermit<'static>,
}

impl Stream for NarStream {
    type Item = std::io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_hash_accepts_real_and_rejects_junk() {
        // 32-char nix base32 (real-looking).
        assert!(valid_hash("0i6vphsl4f9i9zjy9p0gp2bgrza8b8gz"));
        assert!(!valid_hash("short"));
        assert!(!valid_hash("0123456789abcdefghijklmnopqrstuv")); // contains e,o
        assert!(!valid_hash("../../etc/passwd"));
        assert!(!valid_hash(&"a".repeat(33)));
    }

    #[test]
    fn cache_info_responds_ok() {
        // Priority 30 < cache.nixos.org's 40 → host preferred only for what it has.
        assert_eq!(nix_cache_info().status(), StatusCode::OK);
    }

    async fn curl(url: &str) -> Vec<u8> {
        Command::new("curl")
            .args(["-fsS", url])
            .output()
            .await
            .unwrap()
            .stdout
    }

    /// End-to-end against the REAL host /nix/store (needs nix + curl). Manual:
    /// `cargo test -p superzej-host serves_real_store_path -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs the host nix store + curl"]
    async fn serves_real_store_path() {
        let h = serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let base = format!("http://127.0.0.1:{}", h.port);

        let info = String::from_utf8(curl(&format!("{base}/nix-cache-info")).await).unwrap();
        assert!(info.contains("StoreDir: /nix/store"), "cache-info: {info}");

        // Pick a real `<hash>-<name>` store entry.
        let entry = std::fs::read_dir(STORE_DIR)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .find(|n| {
                n.as_bytes().get(32) == Some(&b'-') && valid_hash(&n[..32]) && !n.ends_with(".drv")
            })
            .expect("a store entry");
        let hash = &entry[..32];

        let ni = String::from_utf8(curl(&format!("{base}/{hash}.narinfo")).await).unwrap();
        assert!(ni.contains("NarHash:"), "narinfo missing NarHash: {ni}");
        assert!(
            ni.contains(&format!("StorePath: /nix/store/{entry}")),
            "narinfo path: {ni}"
        );

        let nar = curl(&format!("{base}/nar/{hash}.nar")).await;
        assert!(!nar.is_empty(), "nar body empty for {entry}");
    }
}
