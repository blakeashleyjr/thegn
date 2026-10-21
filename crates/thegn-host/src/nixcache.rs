//! Embedded host Nix binary cache.
//!
//! Serves the HOST `/nix/store` as a live HTTP binary cache (the standard
//! narinfo + NAR protocol) from a background task in the thegn process. A sprite
//! reaches it over the per-worktree reverse tunnel (a sandbox loopback port →
//! this server) and adds it as a nix substituter, so an in-sprite `nix develop`
//! *substitutes* prebuilt store paths from the host instead of building from
//! source. It generalizes the one-shot, per-devShell `file://` push (the removed
//! host-side devShell transfer) into a substituter for deliberately signed host
//! store closures.
//!
//! Trust: only store paths carrying this installation's exact signing-key name
//! are served. Narinfos include the matching `Sig:` line copied from the host
//! store, and sandboxes trust the installation public key while keeping Nix
//! signature verification enabled. Unsigned host objects are not reachable by
//! hash. The key is loaded lazily on the first cache request.
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
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::Stream;
use tokio::process::{Child, Command};
use tokio::sync::{OnceCell, Semaphore};
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

#[derive(Default)]
struct CacheTrust {
    install_key_name: OnceCell<Option<String>>,
}

impl CacheTrust {
    async fn install_key_name(&self) -> Option<String> {
        self.install_key_name
            .get_or_init(|| async {
                match tokio::task::spawn_blocking(crate::nix_signing::ensure_install_key).await {
                    Ok(Ok(key)) => crate::nix_signing::install_key_name(&key).or_else(|| {
                        tracing::warn!(target: "thegn::nixcache", "installation signing key has no valid key name; host cache is closed");
                        None
                    }),
                    Ok(Err(error)) => {
                        tracing::warn!(target: "thegn::nixcache", %error, "installation signing key unavailable; host cache is closed");
                        None
                    }
                    Err(error) => {
                        tracing::warn!(target: "thegn::nixcache", %error, "installation signing key worker failed; host cache is closed");
                        None
                    }
                }
            })
            .await
            .clone()
    }
}

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
    // host_cache only reaches a sprite over the resident musl bridge's reverse
    // tunnel. With no bridge binary resolvable, `agent` neither pushes the bridge
    // nor injects the :8484 substituter — this server binds but is unreachable
    // in-sprite, so the devShell silently builds FROM SOURCE. Warn loudly at the
    // one place that proves host_cache is on. (Fix: `just bridge`, or install
    // `.#default`, which now bundles `thegn-musl` beside the binary.)
    if crate::bridge_sup::bridge_binary_path().is_none() {
        tracing::warn!(
            target: "thegn::startup",
            "host_cache enabled but no `thegn-musl` bridge found (THEGN_BRIDGE_BINARY \
             unset and none beside the thegn binary) — the in-sprite :8484 cache \
             tunnel won't bind, so sprites build the devShell from source"
        );
    }
    let task = tokio::spawn(async move {
        if let Err(e) =
            axum::serve(listener, router(std::sync::Arc::new(CacheTrust::default()))).await
        {
            tracing::warn!(target: "thegn::nixcache", error = %e, "nix cache server exited");
        }
    });
    tracing::info!(target: "thegn::nixcache", port, "embedded host nix cache serving signed paths from {STORE_DIR}");
    Ok(NixCacheHandle { port, _task: task })
}

fn router(trust: std::sync::Arc<CacheTrust>) -> Router {
    // One root handler dispatches `nix-cache-info` + `<hash>.narinfo` (avoids any
    // static-vs-param route conflict); `/nar/{name}` is a deeper path, no overlap.
    Router::new()
        .route("/{file}", get(root_file))
        .route("/nar/{name}", get(nar))
        .with_state(trust)
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
async fn root_file(
    State(trust): State<std::sync::Arc<CacheTrust>>,
    AxumPath(file): AxumPath<String>,
) -> Response {
    if file == "nix-cache-info" {
        return nix_cache_info();
    }
    let Some(hash) = file.strip_suffix(".narinfo") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(path) = resolve_hash(hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(key_name) = trust.install_key_name().await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match narinfo_text(hash, &path, &key_name).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/x-nix-narinfo")], text).into_response(),
        Err(e) => {
            tracing::debug!(target: "thegn::nixcache", %hash, error = %e, "narinfo failed");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// Build a signed narinfo from `nix path-info --json`. Only signatures whose
/// key name exactly matches the installation key are emitted.
async fn narinfo_text(
    hash: &str,
    path: &std::path::Path,
    key_name: &str,
) -> anyhow::Result<String> {
    let out = Command::new("nix")
        .args(["--extra-experimental-features", "nix-command"])
        .arg("path-info")
        .arg("--json")
        .arg(path)
        .output()
        .await?;
    anyhow::ensure!(out.status.success(), "nix path-info exited nonzero");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    narinfo_for(path, hash, &v, key_name).map_err(anyhow::Error::msg)
}

#[derive(Debug, PartialEq, Eq)]
enum NarinfoRefusal {
    NotSigned,
    Malformed(&'static str),
}

impl std::fmt::Display for NarinfoRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSigned => {
                formatter.write_str("path has no signature from the installation key")
            }
            Self::Malformed(field) => write!(formatter, "path-info record has no valid {field}"),
        }
    }
}

fn narinfo_for(
    path: &std::path::Path,
    hash: &str,
    info: &serde_json::Value,
    key_name: &str,
) -> Result<String, NarinfoRefusal> {
    // Newer nix returns an object keyed by path; older returns a single-element
    // array. Take the one record either way.
    let rec = match info {
        serde_json::Value::Object(m) => m.values().next(),
        serde_json::Value::Array(a) => a.first(),
        _ => None,
    }
    .ok_or(NarinfoRefusal::Malformed("record"))?;

    let nar_hash = rec
        .get("narHash")
        .and_then(|x| x.as_str())
        .ok_or(NarinfoRefusal::Malformed("narHash"))?;
    let nar_size = rec
        .get("narSize")
        .and_then(serde_json::Value::as_u64)
        .ok_or(NarinfoRefusal::Malformed("narSize"))?;
    let signatures: Vec<&str> = rec
        .get("signatures")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|signature| {
            signature
                .split_once(':')
                .is_some_and(|(name, _)| name == key_name)
        })
        .collect();
    if signatures.is_empty() {
        return Err(NarinfoRefusal::NotSigned);
    }
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
    for signature in signatures {
        s.push_str(&format!("Sig: {signature}\n"));
    }
    Ok(s)
}

/// `GET /nar/{hash}.nar` — stream an admitted NAR via `nix-store --dump`, with
/// constant memory (the tunnel applies backpressure).
async fn nar(
    State(trust): State<std::sync::Arc<CacheTrust>>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let hash = name.strip_suffix(".nar").unwrap_or(&name);
    let Some(path) = resolve_hash(hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(key_name) = trust.install_key_name().await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Err(error) = narinfo_text(hash, &path, &key_name).await {
        tracing::debug!(target: "thegn::nixcache", %hash, %error, "NAR refused by cache admission");
        return StatusCode::NOT_FOUND.into_response();
    }
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
            tracing::debug!(target: "thegn::nixcache", error = %e, "nix-store --dump spawn failed");
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

    const INSTALL_KEY_NAME: &str = "thegn-0123456789abcdef-1";

    fn signed_record() -> serde_json::Value {
        serde_json::json!({
            "narHash": "sha256-raw-hash-from-fixture",
            "narSize": 42,
            "references": ["/nix/store/11111111111111111111111111111111-reference"],
            "signatures": [
                "cache.nixos.org-1:AAAA",
                "thegn-0123456789abcdef-1:BBBB"
            ]
        })
    }

    fn narinfo(info: serde_json::Value) -> Result<String, NarinfoRefusal> {
        narinfo_for(
            std::path::Path::new("/nix/store/0123456789abcdef0123456789abcdef-package"),
            "0123456789abcdef0123456789abcdef",
            &info,
            INSTALL_KEY_NAME,
        )
    }

    #[test]
    fn narinfo_carries_only_the_install_key_signature() {
        let info = serde_json::json!({
            "/nix/store/0123456789abcdef0123456789abcdef-package": signed_record()
        });
        let text = narinfo(info).unwrap();
        for line in [
            "StorePath: /nix/store/0123456789abcdef0123456789abcdef-package\n",
            "NarHash: sha256-raw-hash-from-fixture\n",
            "NarSize: 42\n",
            "References: 11111111111111111111111111111111-reference\n",
            "Sig: thegn-0123456789abcdef-1:BBBB\n",
        ] {
            assert!(text.contains(line), "missing {line:?} in {text}");
        }
        assert!(
            !text.contains("cache.nixos.org-1"),
            "foreign signature leaked: {text}"
        );
    }

    #[test]
    fn narinfo_refuses_a_path_without_the_install_key_signature() {
        for signatures in [
            None,
            Some(vec![]),
            Some(vec!["cache.nixos.org-1:AAAA"]),
            Some(vec!["thegn-0123456789abcdef-1x:CCCC"]),
        ] {
            let mut record = serde_json::json!({
                "narHash": "sha256-raw-hash-from-fixture",
                "narSize": 42
            });
            if let Some(signatures) = signatures {
                record["signatures"] = serde_json::json!(signatures);
            }
            let info = serde_json::json!({
                "/nix/store/0123456789abcdef0123456789abcdef-package": record
            });
            assert_eq!(narinfo(info), Err(NarinfoRefusal::NotSigned));
        }
    }

    #[test]
    fn narinfo_accepts_both_path_info_shapes() {
        let object = serde_json::json!({
            "/nix/store/0123456789abcdef0123456789abcdef-package": signed_record()
        });
        let array = serde_json::json!([signed_record()]);
        assert_eq!(narinfo(object).unwrap(), narinfo(array).unwrap());
    }

    #[test]
    fn narinfo_refuses_malformed_records() {
        let missing_hash =
            serde_json::json!({"narSize": 42, "signatures": ["thegn-0123456789abcdef-1:BBBB"]});
        let missing_size = serde_json::json!({"narHash": "sha256-raw-hash-from-fixture", "signatures": ["thegn-0123456789abcdef-1:BBBB"]});
        let missing_hash = serde_json::json!({
            "/nix/store/0123456789abcdef0123456789abcdef-package": missing_hash
        });
        let missing_size = serde_json::json!({
            "/nix/store/0123456789abcdef0123456789abcdef-package": missing_size
        });
        assert_eq!(
            narinfo(missing_hash),
            Err(NarinfoRefusal::Malformed("narHash"))
        );
        assert_eq!(
            narinfo(missing_size),
            Err(NarinfoRefusal::Malformed("narSize"))
        );
    }

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

    async fn curl_status(url: &str) -> String {
        let output = Command::new("curl")
            .args(["-sS", "-o", "/dev/null", "-w", "%{http_code}", url])
            .output()
            .await
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    }

    /// End-to-end against the REAL host /nix/store (needs nix + curl). Manual:
    /// `cargo test -p thegn-host serves_real_store_path -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs the host nix store + curl"]
    async fn serves_real_store_path() {
        let _state = thegn_core::testenv::IsolatedState::new();
        let h = serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let base = format!("http://127.0.0.1:{}", h.port);

        assert_eq!(curl_status(&format!("{base}/nix-cache-info")).await, "200");

        let key_name = crate::nix_signing::install_key_name(
            &crate::nix_signing::ensure_install_key().unwrap(),
        )
        .unwrap();
        // Pick a real store entry that is not signed by this installation key.
        let entries = std::fs::read_dir(STORE_DIR)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| {
                n.as_bytes().get(32) == Some(&b'-') && valid_hash(&n[..32]) && !n.ends_with(".drv")
            })
            .collect::<Vec<_>>();
        let mut unsigned_entry = None;
        for candidate in entries {
            let hash = &candidate[..32];
            let path = PathBuf::from(format!("{STORE_DIR}/{candidate}"));
            let output = Command::new("nix")
                .args([
                    "--extra-experimental-features",
                    "nix-command",
                    "path-info",
                    "--json",
                ])
                .arg(&path)
                .output()
                .await
                .unwrap();
            if !output.status.success() {
                continue;
            }
            let Ok(info) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
                continue;
            };
            if matches!(
                narinfo_for(&path, hash, &info, &key_name),
                Err(NarinfoRefusal::NotSigned)
            ) {
                unsigned_entry = Some(candidate);
                break;
            }
        }
        let entry = unsigned_entry.expect("an unsigned host store entry");
        let hash = &entry[..32];
        assert_eq!(curl_status(&format!("{base}/{hash}.narinfo")).await, "404");
        assert_eq!(curl_status(&format!("{base}/nar/{hash}.nar")).await, "404");
    }
}
