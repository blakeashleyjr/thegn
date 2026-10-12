//! Cover-art rendering for the Now-Playing overlay (optional `[media]` feature).
//!
//! Art is fetched + decoded **off the loop** (`file://` reads and HTTP GETs are
//! blocking / slow) and folded into a half-block **mosaic**: each cell is a `▀`
//! glyph whose foreground is the upper pixel and background the lower pixel, so
//! one text cell shows two vertical pixels. This renders in any truecolor
//! terminal — no kitty/sixel required — which is the safe default the spec calls
//! for (graphics protocols stay gated / best-effort elsewhere). When the
//! terminal can't do truecolor the seg color layer quantizes it down like every
//! other frame.
//!
//! The art URL is attacker-influenced (a player publishes it), so every fetch is
//! bounded: http(s) only, resolved addresses vetted and the connection pinned to
//! them, redirects re-validated per hop, body streamed under a byte cap and a
//! total deadline, local files must be bounded regular files, and only the
//! latest request may own the (tracked) task. Local art is limited to files
//! under the system temporary directory, `$XDG_RUNTIME_DIR`, `$XDG_CACHE_HOME`
//! (or `$HOME/.cache` when unset), and `$HOME/Music`; paths are canonicalized
//! against those roots before bounded reads.

use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use termwiz::terminal::TerminalWaker;
use tokio::sync::Semaphore;

use crate::seg::{Line, Tok, seg};

/// One decoded cover as mosaic rows, ready for `seg::draw_line`. `cols`/`rows`
/// are the cell dimensions (rows = pixel-rows / 2).
#[derive(Debug, Clone)]
pub(crate) struct ArtMosaic {
    /// The source URL this was decoded from — so a stale delivery for a
    /// now-different track can be dropped.
    pub url: String,
    pub lines: Vec<Line>,
}

/// Build a `cols × rows`-cell mosaic from a decoded RGBA raster by nearest-
/// neighbour sampling two vertical pixels per cell into a `▀` (upper-half block).
pub(crate) fn mosaic(raster: &crate::rasterize::Raster, cols: usize, rows: usize) -> Vec<Line> {
    let (iw, ih) = (raster.w.max(1), raster.h.max(1));
    let px = |cx: usize, py: usize| -> (u8, u8, u8) {
        // Map cell/sub-pixel coords into the source image (nearest neighbour).
        let sx = ((cx as u32) * iw / cols.max(1) as u32).min(iw - 1);
        let sy = ((py as u32) * ih / (rows.max(1) as u32 * 2)).min(ih - 1);
        let idx = ((sy * iw + sx) * 4) as usize;
        match raster.rgba.get(idx..idx + 3) {
            Some(p) => (p[0], p[1], p[2]),
            None => (0, 0, 0),
        }
    };
    (0..rows)
        .map(|row| {
            let mut segs = Vec::with_capacity(cols);
            for cx in 0..cols {
                let top = px(cx, row * 2);
                let bot = px(cx, row * 2 + 1);
                segs.push(
                    seg(Tok::Rgb(top.0, top.1, top.2), "\u{2580}") // ▀
                        .bg(Tok::Rgb(bot.0, bot.1, bot.2)),
                );
            }
            Line::segs(segs)
        })
        .collect()
}

/// Longest art URL admitted; longer is refused outright.
pub(crate) const MAX_URL_LEN: usize = 2048;
/// Encoded-body cap for both HTTP bodies and local files.
const MAX_ART_BYTES: usize = 8 * 1024 * 1024;
/// Wall-clock budget for a whole fetch (DNS, every redirect hop, body).
const FETCH_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
/// Redirect hops followed (each re-validated and re-resolved).
const MAX_REDIRECTS: usize = 3;
/// A blocking image decoder may continue after its async owner is aborted.
/// Keep its admission permit in the blocking closure so churn cannot queue an
/// unbounded number of decoder workers.
static DECODE_SLOT: Semaphore = Semaphore::const_new(1);

/// Why a locator was refused. Carries no URL or path, so a diagnostic never
/// reflects attacker-controlled text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reject {
    TooLong,
    Scheme,
    Credentials,
    NoHost,
    ForbiddenAddress,
    Resolve,
    Status,
    ContentType,
    TooLarge,
    Redirects,
    Network,
    Decode,
    LocalPath,
}

/// The host of an admitted URL.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    Ip,
    Name(String),
}

/// Whether `ip` is a destination cover art must never be fetched from:
/// loopback, private, link-local, unspecified, multicast, CGNAT, broadcast,
/// and IPv6 forms that embed an IPv4 address (mapped, compat, NAT64, 6to4).
fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xC0) == 64) // 100.64.0.0/10 CGNAT
                || o[0] >= 240 // reserved
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            let seg = v6.segments();
            if (seg[0] & 0xFE00) == 0xFC00 || (seg[0] & 0xFFC0) == 0xFE80 {
                return true; // unique-local / link-local
            }
            if let Some(v4) = v6.to_ipv4() {
                // ::ffff:a.b.c.d and ::a.b.c.d
                return forbidden_ip(IpAddr::V4(v4));
            }
            // 64:ff9b::/96 NAT64 embeds an IPv4 in the low 32 bits.
            if seg[0] == 0x64 && seg[1] == 0xFF9B && seg[2..6].iter().all(|s| *s == 0) {
                let o = v6.octets();
                let v4 = std::net::Ipv4Addr::new(o[12], o[13], o[14], o[15]);
                return forbidden_ip(IpAddr::V4(v4));
            }
            seg[0] == 0x2002 // 6to4 can tunnel to arbitrary v4
        }
    }
}

/// Admission for a URL BEFORE resolution: http(s) only, no credentials, a host,
/// and — for an IP-literal host — an allowed address. Pure.
fn admit_url(url: &reqwest::Url) -> Result<Host, Reject> {
    if url.as_str().len() > MAX_URL_LEN {
        return Err(Reject::TooLong);
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Reject::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Reject::Credentials);
    }
    let host = url.host_str().ok_or(Reject::NoHost)?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.is_empty() {
        return Err(Reject::NoHost);
    }
    match bare.parse::<IpAddr>() {
        Ok(ip) if forbidden_ip(ip) => Err(Reject::ForbiddenAddress),
        Ok(_) => Ok(Host::Ip),
        Err(_) => Ok(Host::Name(bare.to_string())),
    }
}

/// Admission for the RESOLVED address set of a hostname: non-empty and every
/// address allowed (one bad record rejects the lot, so a mixed answer cannot
/// be used to reach an internal address). The returned set is what the
/// connection is pinned to. Pure.
fn admit_resolved(addrs: &[SocketAddr]) -> Result<Vec<SocketAddr>, Reject> {
    if addrs.is_empty() {
        return Err(Reject::Resolve);
    }
    if addrs.iter().any(|a| forbidden_ip(a.ip())) {
        return Err(Reject::ForbiddenAddress);
    }
    Ok(addrs.to_vec())
}

/// Latest-wins sequencing: only the most recently begun request may deliver.
#[derive(Debug, Default)]
struct Latest(AtomicU64);

impl Latest {
    /// Start a request; every earlier one becomes stale.
    fn begin(&self) -> u64 {
        self.0.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn is_current(&self, generation: u64) -> bool {
        self.0.load(Ordering::SeqCst) == generation
    }
}

static LATEST: Latest = Latest(AtomicU64::new(0));
/// The one tracked in-flight art task; replaced (and the old one aborted) on
/// every new request, so at most one network job is ever live.
static IN_FLIGHT: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

/// Fetch one http(s) body: redirects followed manually (each hop re-admitted,
/// re-resolved and the connection pinned to the vetted addresses), streamed
/// under [`MAX_ART_BYTES`]. Proxies are disabled so pinning cannot be bypassed.
async fn fetch_http(start: &str) -> Result<Vec<u8>, Reject> {
    let mut url = reqwest::Url::parse(start).map_err(|_| Reject::Scheme)?;
    for _ in 0..=MAX_REDIRECTS {
        let host = admit_url(&url)?;
        let port = url.port_or_known_default().ok_or(Reject::Scheme)?;
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(FETCH_TOTAL_TIMEOUT);
        if let Host::Name(name) = &host {
            let lookup = tokio::net::lookup_host((name.as_str(), port));
            let resolved: Vec<SocketAddr> = tokio::time::timeout(DNS_TIMEOUT, lookup)
                .await
                .map_err(|_| Reject::Resolve)?
                .map_err(|_| Reject::Resolve)?
                .collect();
            let pinned = admit_resolved(&resolved)?;
            builder = builder.resolve_to_addrs(name, &pinned);
        }
        let client = builder.build().map_err(|_| Reject::Network)?;
        let mut resp = client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| Reject::Network)?;
        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(Reject::Redirects)?;
            url = url.join(loc).map_err(|_| Reject::Redirects)?;
            continue;
        }
        if !resp.status().is_success() {
            return Err(Reject::Status);
        }
        admit_content_type(resp.headers().get(reqwest::header::CONTENT_TYPE))?;
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_ART_BYTES as u64)
        {
            return Err(Reject::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|_| Reject::Network)? {
            if body.len() + chunk.len() > MAX_ART_BYTES {
                return Err(Reject::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        return Ok(body);
    }
    Err(Reject::Redirects)
}

/// Accept image media types and generic binary bodies. Missing Content-Type is
/// allowed because the bounded image decoder sniffs the format itself.
fn admit_content_type(value: Option<&reqwest::header::HeaderValue>) -> Result<(), Reject> {
    let Some(value) = value else {
        return Ok(());
    };
    let Ok(value) = value.to_str() else {
        return Err(Reject::ContentType);
    };
    let media_type = value.split(';').next().unwrap_or_default().trim();
    let mut parts = media_type.split('/');
    let top_level = parts.next().unwrap_or_default();
    let subtype = parts.next().unwrap_or_default();
    let image_type = top_level.eq_ignore_ascii_case("image")
        && !subtype.is_empty()
        && parts.next().is_none()
        && subtype
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    if image_type || media_type.eq_ignore_ascii_case("application/octet-stream") {
        Ok(())
    } else {
        Err(Reject::ContentType)
    }
}

/// Run a blocking decode only while holding a slot. Moving the permit into the
/// closure keeps it held until the decoder exits, even if this future is aborted.
async fn admitted_blocking<T, F>(slot: &'static Semaphore, f: F) -> Result<T, Reject>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = slot.acquire().await.map_err(|_| Reject::Decode)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .map_err(|_| Reject::Decode)
}

/// Fetch + decode one art locator to a raster, enforcing every bound.
async fn load_raster(art_url: &str) -> Result<crate::rasterize::Raster, Reject> {
    if art_url.len() > MAX_URL_LEN {
        return Err(Reject::TooLong);
    }
    if let Some(path) = local_path(art_url) {
        return admitted_blocking(&DECODE_SLOT, move || {
            let (root, rel) = admit_local_art(&path, &local_art_roots())?;
            let file = crate::platform::open_beneath::open_regular_beneath(&root, &rel)
                .map_err(|_| Reject::LocalPath)?;
            let bytes = crate::rasterize::read_open_regular_capped(file, MAX_ART_BYTES as u64)
                .map_err(|_| Reject::Decode)?;
            crate::rasterize::image_bytes_with_limits(&bytes, crate::rasterize::MEDIA_ART_LIMITS)
                .map_err(|_| Reject::Decode)
        })
        .await?;
    }
    if !(art_url.starts_with("http://") || art_url.starts_with("https://")) {
        return Err(Reject::Scheme);
    }
    let bytes = tokio::time::timeout(FETCH_TOTAL_TIMEOUT, fetch_http(art_url))
        .await
        .map_err(|_| Reject::Network)??;
    admitted_blocking(&DECODE_SLOT, move || {
        crate::rasterize::image_bytes_with_limits(&bytes, crate::rasterize::MEDIA_ART_LIMITS)
    })
    .await?
    .map_err(|_| Reject::Decode)
}

/// Fetch + decode `art_url` into an [`ArtMosaic`] off-thread, then deliver it and
/// pulse the waker. Supports `file://`/bare paths and `http(s)://`. Silent on
/// any failure (the overlay just shows its placeholder; the typed reason goes to
/// `tracing` without the URL). Latest-wins: a newer call aborts the previous
/// in-flight task and a stale completion never delivers.
pub(crate) fn spawn_fetch(
    art_url: String,
    cols: usize,
    rows: usize,
    tx: tokio::sync::watch::Sender<Option<ArtMosaic>>,
    waker: TerminalWaker,
) {
    let generation = LATEST.begin();
    let handle = tokio::spawn(async move {
        let raster = match load_raster(&art_url).await {
            Ok(r) => r,
            Err(reason) => {
                tracing::debug!(target: "thegn::media_art", ?reason, "cover art refused or failed");
                return;
            }
        };
        if !LATEST.is_current(generation) {
            return;
        }
        let lines = mosaic(&raster, cols, rows);
        if !LATEST.is_current(generation) {
            return;
        }
        if tx.is_closed() {
            tracing::debug!(target: "thegn::media_art", "cover art receiver closed");
        }
        drop(tx.send_replace(Some(ArtMosaic {
            url: art_url,
            lines,
        })));
        if let Err(error) = waker.wake() {
            tracing::debug!(target: "thegn::media_art", %error, "cover art wake failed");
        }
    });
    replace_in_flight(&IN_FLIGHT, handle);
}

pub(crate) fn cancel_in_flight() {
    cancel_slot(&LATEST, &IN_FLIGHT);
}

fn cancel_slot(latest: &Latest, slot: &Mutex<Option<tokio::task::JoinHandle<()>>>) {
    latest.begin();
    if let Some(previous) = slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        previous.abort();
    }
}

fn replace_in_flight(
    slot: &Mutex<Option<tokio::task::JoinHandle<()>>>,
    handle: tokio::task::JoinHandle<()>,
) {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(previous) = slot.replace(handle) {
        previous.abort();
    }
}

/// A local filesystem path for a `file:` URL (structurally parsed: local host
/// only, percent-decoded) or a bare absolute path; `None` for remote schemes
/// and for `file:` URLs naming another host.
fn local_path(url: &str) -> Option<std::path::PathBuf> {
    if url
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("file:"))
    {
        let parsed = reqwest::Url::parse(url).ok()?;
        match parsed.host_str() {
            None | Some("") | Some("localhost") => {}
            Some(_) => return None,
        }
        parsed.to_file_path().ok()
    } else if url.starts_with('/') {
        Some(std::path::PathBuf::from(url))
    } else {
        None
    }
}

/// Runtime allowlist for locally cached/player-library art. Non-absolute roots
/// are ignored so malformed environment overrides cannot widen the policy.
fn local_art_roots() -> Vec<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut roots = vec![std::env::temp_dir()];
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        roots.push(runtime.into());
    }
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        roots.push(cache.into());
    } else if let Some(home) = &home {
        roots.push(home.join(".cache"));
    }
    if let Some(home) = home {
        roots.push(home.join("Music"));
    }
    roots.retain(|root| root.is_absolute());
    roots
}

/// Admit a local art file only when its canonical location is under one of the
/// canonical allowlist roots. Return the root plus relative path so the later
/// open can walk every component without following a symlink.
fn admit_local_art(
    path: &std::path::Path,
    roots: &[std::path::PathBuf],
) -> Result<(std::path::PathBuf, std::path::PathBuf), Reject> {
    let canonical_path = path.canonicalize().map_err(|_| Reject::LocalPath)?;
    for root in roots {
        let Ok(canonical_root) = root.canonicalize() else {
            continue;
        };
        if let Ok(rel) = canonical_path.strip_prefix(&canonical_root) {
            return Ok((canonical_root, rel.to_path_buf()));
        }
    }
    Err(Reject::LocalPath)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn admit(s: &str) -> Result<Host, Reject> {
        admit_url(&reqwest::Url::parse(s).unwrap())
    }

    #[test]
    fn local_path_parses_file_urls() {
        assert_eq!(
            local_path("file:///home/u/art.png"),
            Some(std::path::PathBuf::from("/home/u/art.png"))
        );
        assert_eq!(
            local_path("/tmp/cover.jpg"),
            Some(std::path::PathBuf::from("/tmp/cover.jpg"))
        );
        assert_eq!(local_path("https://i.scdn.co/x"), None);
        assert_eq!(local_path("data:image/png;base64,AAAA"), None);
    }

    #[test]
    fn file_urls_are_parsed_structurally() {
        assert_eq!(
            local_path("file:///home/u/my%20art.png"),
            Some(std::path::PathBuf::from("/home/u/my art.png"))
        );
        assert_eq!(
            local_path("file://localhost/tmp/a.png"),
            Some(std::path::PathBuf::from("/tmp/a.png"))
        );
        assert_eq!(local_path("file://evil.example/share/a.png"), None);
    }

    #[test]
    fn local_art_requires_a_canonical_path_under_an_allowed_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let allowed_file = sub.join("cover.png");
        let outside_file = outside.path().join("cover.png");
        std::fs::write(&allowed_file, b"bounded test fixture").unwrap();
        std::fs::write(&outside_file, b"bounded test fixture").unwrap();
        let roots = vec![root.path().to_path_buf()];

        let (admitted_root, admitted_rel) = admit_local_art(&allowed_file, &roots).unwrap();
        assert_eq!(admitted_root, root.path().canonicalize().unwrap());
        assert_eq!(admitted_rel, std::path::Path::new("sub/cover.png"));
        let opened =
            crate::platform::open_beneath::open_regular_beneath(&admitted_root, &admitted_rel)
                .unwrap();
        assert_eq!(
            crate::rasterize::read_open_regular_capped(opened, 100).unwrap(),
            b"bounded test fixture"
        );
        assert_eq!(
            admit_local_art(&outside_file, &roots),
            Err(Reject::LocalPath)
        );
        let escaped_path = root
            .path()
            .join("sub")
            .join("../..")
            .join(outside.path().file_name().unwrap())
            .join("cover.png");
        assert_eq!(
            admit_local_art(&escaped_path, &roots),
            Err(Reject::LocalPath)
        );
        assert_eq!(
            admit_local_art(&root.path().join("missing.png"), &roots),
            Err(Reject::LocalPath)
        );
    }

    #[test]
    fn admitted_fifo_is_refused_by_nonblocking_beneath_open() {
        if !crate::platform::test_fifo_supported() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("cover.fifo");
        crate::platform::test_fifo(&fifo).unwrap();
        let (admitted_root, admitted_rel) =
            admit_local_art(&fifo, &[root.path().to_path_buf()]).unwrap();
        assert!(
            crate::platform::open_beneath::open_regular_beneath(&admitted_root, &admitted_rel)
                .is_err()
        );
    }

    #[test]
    fn admitted_directory_swap_to_symlink_is_refused() {
        if !crate::platform::test_symlink_supported() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("cover.png"), b"inside bytes").unwrap();
        std::fs::write(outside.path().join("cover.png"), b"outside bytes").unwrap();
        let (admitted_root, admitted_rel) =
            admit_local_art(&sub.join("cover.png"), &[root.path().to_path_buf()]).unwrap();

        std::fs::rename(&sub, root.path().join("sub-saved")).unwrap();
        crate::platform::test_symlink(outside.path(), &sub).unwrap();

        assert!(
            crate::platform::open_beneath::open_regular_beneath(&admitted_root, &admitted_rel)
                .is_err()
        );
    }

    #[test]
    fn forbidden_ip_covers_internal_ranges() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "100.64.0.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2002::1",
        ] {
            assert!(forbidden_ip(ip(bad)), "{bad} must be forbidden");
        }
        for good in ["8.8.8.8", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(!forbidden_ip(ip(good)), "{good} must be allowed");
        }
    }

    #[test]
    fn admit_url_enforces_scheme_credentials_and_literals() {
        assert_eq!(
            admit("https://i.scdn.co/a.jpg"),
            Ok(Host::Name("i.scdn.co".into()))
        );
        assert_eq!(admit("http://8.8.8.8/x"), Ok(Host::Ip));
        assert_eq!(admit("ftp://example.com/x"), Err(Reject::Scheme));
        assert_eq!(admit("file:///etc/passwd"), Err(Reject::Scheme));
        assert_eq!(
            admit("https://user:pw@example.com/"),
            Err(Reject::Credentials)
        );
        assert_eq!(
            admit("http://127.0.0.1:8080/"),
            Err(Reject::ForbiddenAddress)
        );
        assert_eq!(admit("http://[::1]/"), Err(Reject::ForbiddenAddress));
        // WHATWG normalises exotic IPv4 spellings to the dotted form.
        assert_eq!(admit("http://0x7f.1/"), Err(Reject::ForbiddenAddress));
        assert_eq!(admit("http://2130706433/"), Err(Reject::ForbiddenAddress));
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert_eq!(admit(&long), Err(Reject::TooLong));
    }

    #[test]
    fn admit_resolved_rejects_any_internal_record() {
        let ok = SocketAddr::new(ip("8.8.8.8"), 443);
        let bad = SocketAddr::new(ip("169.254.169.254"), 443);
        assert_eq!(admit_resolved(&[]), Err(Reject::Resolve));
        assert_eq!(admit_resolved(&[ok]), Ok(vec![ok]));
        // A mixed (rebinding-style) answer is rejected whole.
        assert_eq!(admit_resolved(&[ok, bad]), Err(Reject::ForbiddenAddress));
    }

    #[test]
    fn content_type_admission_accepts_images_and_rejects_other_media() {
        let header = |value: &str| reqwest::header::HeaderValue::from_str(value).unwrap();
        assert_eq!(admit_content_type(None), Ok(()));
        assert_eq!(
            admit_content_type(Some(&header("image/jpeg; charset=x"))),
            Ok(())
        );
        assert_eq!(admit_content_type(Some(&header("IMAGE/PNG"))), Ok(()));
        assert_eq!(
            admit_content_type(Some(&header("application/octet-stream"))),
            Ok(())
        );
        for rejected in ["text/html", "application/json", "image/", "garbage"] {
            assert_eq!(
                admit_content_type(Some(&header(rejected))),
                Err(Reject::ContentType),
                "{rejected} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn decode_slot_lives_until_blocking_closure_returns_after_owner_abort() {
        static TEST_SLOT: Semaphore = Semaphore::const_new(1);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = tokio::spawn(admitted_blocking(&TEST_SLOT, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_eq!(TEST_SLOT.available_permits(), 0);

        let second_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_started_in_job = second_started.clone();
        let mut second = tokio::spawn(admitted_blocking(&TEST_SLOT, move || {
            second_started_in_job.store(true, Ordering::SeqCst);
        }));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut second)
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        second.await.unwrap().unwrap();
        assert!(second_started.load(Ordering::SeqCst));
        assert_eq!(TEST_SLOT.available_permits(), 1);
    }

    #[tokio::test]
    async fn replacing_in_flight_aborts_every_previous_task() {
        let slot = Mutex::new(None);
        let mut previous = Vec::new();
        for _ in 0..4 {
            let handle = tokio::spawn(std::future::pending());
            previous.push(handle.abort_handle());
            replace_in_flight(&slot, handle);
        }
        tokio::task::yield_now().await;
        assert!(
            previous[..previous.len() - 1]
                .iter()
                .all(tokio::task::AbortHandle::is_finished)
        );
        assert!(!previous.last().unwrap().is_finished());
        assert!(
            slot.lock()
                .unwrap()
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
        );
    }

    #[test]
    fn latest_wins_sequencing() {
        let l = Latest::default();
        let first = l.begin();
        assert!(l.is_current(first));
        let second = l.begin();
        assert!(!l.is_current(first), "older request is superseded");
        assert!(l.is_current(second));
    }

    #[tokio::test]
    async fn cancel_invalidates_and_aborts_the_in_flight_task() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let latest = Latest::default();
        let slot = Mutex::new(None);
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let handle = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        replace_in_flight(&slot, handle);
        cancel_slot(&latest, &slot);
        tokio::task::yield_now().await;

        assert!(dropped.load(Ordering::Acquire));
        assert!(!latest.is_current(0));
    }

    #[test]
    fn mosaic_dims() {
        let raster = crate::rasterize::Raster {
            rgba: vec![255u8; 8 * 8 * 4],
            w: 8,
            h: 8,
        };
        let lines = mosaic(&raster, 6, 3);
        assert_eq!(lines.len(), 3);
    }
}
