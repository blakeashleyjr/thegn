//! The **cloud lowering adapter**: a [`HostRunner`] for `Reach::Cloud` hosts
//! (Sprites, Daytona). There is no shell to probe and no runtime to install —
//! the "connection" is an authenticated API client, and "delivery" means
//! registering the base image as the provider's bootable template/snapshot
//! primitive. Every method is BLOCKING (the runner contract): each HTTP call
//! spins a small current-thread tokio runtime for the async `reqwest` client,
//! which is safe on the `spawn_blocking`/CLI threads runners live on.
//!
//! NOTE: the host crate currently gates cloud reaches off before `runner_for`
//! ever dispatches here (its binding resolution returns no cloud placements
//! yet) — this adapter is correct and mock-tested, just not user-reachable
//! until that gate lifts.

use std::time::Duration;

use superzej_core::host::{Arch, CloudReach, HostCaps, RuntimeInfo, RuntimeKind, VolumeSpec};
use superzej_core::image::{DeliveryStrategy, Digest, ImageRef, ResolvedImage};

use super::HostRunner;
use crate::provider::{Provider, SpritesProvider};

/// Connect/list calls follow the host-flow policy table (connect 30s).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Template registration is the cloud "deliver" step (policy: deliver 1800s) —
/// providers may pull the image server-side before the call returns.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(1800);

/// The cloud providers this adapter can lower to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Sprites,
    Daytona,
}

/// The [`HostRunner`] for provider-managed hosts. Holds the reach and the
/// token resolved by `connect()`; HTTP clients are per-call (see module doc).
pub struct CloudRunner {
    reach: CloudReach,
    kind: ProviderKind,
    /// Resolved from `reach.api_key_env` on `connect()` (never persisted).
    token: Option<String>,
}

/// Build the cloud runner for a reach; errors on an unknown provider name.
pub fn cloud_runner_for(reach: &CloudReach) -> Result<Box<dyn HostRunner>, String> {
    Ok(Box::new(CloudRunner::new(reach.clone())?))
}

impl CloudRunner {
    fn new(reach: CloudReach) -> Result<CloudRunner, String> {
        let kind = match reach.provider.trim() {
            "sprites" => ProviderKind::Sprites,
            "daytona" => ProviderKind::Daytona,
            other => {
                return Err(format!(
                    "cloud host: unknown provider {other:?} (supported: sprites, daytona)"
                ));
            }
        };
        Ok(CloudRunner {
            reach,
            kind,
            token: None,
        })
    }

    /// Test seam: construct with an explicit token so tests never touch the
    /// process environment (`connect()` skips env resolution when set).
    #[cfg(test)]
    fn with_token(reach: CloudReach, token: &str) -> CloudRunner {
        let mut r = CloudRunner::new(reach).expect("test reach has a known provider");
        r.token = Some(token.to_string());
        r
    }

    fn token(&self) -> Result<&str, String> {
        self.token
            .as_deref()
            .ok_or_else(|| "cloud: no API token resolved — connect() first (driver bug)".into())
    }

    /// The effective API base: the reach's, or the provider's documented
    /// default when empty (mirrors `provider.rs` conventions).
    fn api_base(&self) -> String {
        let b = self.reach.api_base.trim().trim_end_matches('/');
        if !b.is_empty() {
            return b.to_string();
        }
        match self.kind {
            ProviderKind::Sprites => "https://api.sprites.dev/v1".into(),
            ProviderKind::Daytona => "https://app.daytona.io/api".into(),
        }
    }

    /// The cheap authenticated list endpoint `connect()` pings — exactly the
    /// lifecycle-list endpoints `provider.rs` uses.
    fn ping_url(&self) -> String {
        match self.kind {
            ProviderKind::Sprites => format!("{}/sprites", self.api_base()),
            ProviderKind::Daytona => format!("{}/sandbox", self.api_base()),
        }
    }

    /// Where registered templates/snapshots are listed for `image_present`.
    /// Sprites templates are just named sprites (the checkpoint source);
    /// Daytona registers snapshots (singular path, matching its `/sandbox`
    /// convention in `provider.rs`).
    fn template_list_url(&self) -> String {
        match self.kind {
            ProviderKind::Sprites => format!("{}/sprites", self.api_base()),
            ProviderKind::Daytona => format!("{}/snapshot", self.api_base()),
        }
    }

    /// The envelope key a wrapped list response may use.
    fn list_envelope(&self) -> &'static str {
        match self.kind {
            ProviderKind::Sprites => "sprites",
            ProviderKind::Daytona => "snapshots",
        }
    }
}

/// Extract `name` fields from a list response (a bare array, or wrapped in
/// `{ "<envelope>": [...] }`) — pure, unit-tested.
fn parse_names(v: &serde_json::Value, envelope: &str) -> Vec<String> {
    let arr = v
        .as_array()
        .cloned()
        .or_else(|| v.get(envelope).and_then(|e| e.as_array()).cloned())
        .unwrap_or_default();
    arr.iter()
        .filter_map(|e| e.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .collect()
}

/// Mirrors `provider.rs`'s private client builder: bound connection
/// establishment so an unreachable control plane fails fast, never hangs.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Drive one async call to completion on a fresh current-thread runtime. The
/// runtime (and any client created inside `fut`) dies with the call, so no
/// reactor/pool state ever leaks across the blocking runner methods.
fn block_on<T>(fut: impl std::future::Future<Output = T>) -> Result<T, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cloud: tokio runtime: {e}"))?;
    Ok(rt.block_on(fut))
}

/// Authenticated GET returning `(status, json-or-null)`; `Err` only on
/// transport failure (callers decide what a non-2xx means).
fn get_json(url: &str, token: &str, timeout: Duration) -> Result<(u16, serde_json::Value), String> {
    block_on(async {
        let resp = http_client()
            .get(url)
            .bearer_auth(token)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok((status, body))
    })?
}

/// Authenticated JSON POST returning `(status, json-or-null)`.
fn post_json(
    url: &str,
    token: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<(u16, serde_json::Value), String> {
    block_on(async {
        let resp = http_client()
            .post(url)
            .bearer_auth(token)
            .json(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok((status, body))
    })?
}

impl HostRunner for CloudRunner {
    fn connect(&mut self) -> Result<(), String> {
        if self.token.is_none() {
            let var = &self.reach.api_key_env;
            let tok = std::env::var(var)
                .ok()
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "cloud host ({}): API token env var {var} is not set",
                        self.reach.provider
                    )
                })?;
            self.token = Some(tok);
        }
        let url = self.ping_url();
        let (status, _body) = get_json(&url, self.token()?, CONNECT_TIMEOUT)
            .map_err(|e| format!("cloud connect: {e}"))?;
        match status {
            // Bad credentials won't fix themselves — sound non-retryable.
            401 | 403 => Err(format!(
                "cloud connect: token rejected by {} ({status}) — check ${}",
                self.reach.provider, self.reach.api_key_env
            )),
            s if (200..300).contains(&s) => Ok(()),
            s => Err(format!(
                "cloud connect: {} answered HTTP {s} on {url}",
                self.reach.provider
            )),
        }
    }

    fn probe(&mut self) -> Result<HostCaps, String> {
        // No shell to probe: the provider manages the machine. Cloud fleets
        // are amd64 today (Sprites/Daytona both boot x86_64 microVMs); revisit
        // when a provider exposes arm64 templates.
        Ok(HostCaps::cloud_managed(Arch::Amd64))
    }

    fn install_runtime(
        &mut self,
        _kind: RuntimeKind,
        _note: &mut dyn FnMut(String),
    ) -> Result<RuntimeInfo, String> {
        // Unreachable in practice: probe() always reports a CloudManaged
        // runtime, so the machine never walks Installing.
        Err("cloud-managed hosts have no runtime to install".into())
    }

    fn resolve_image(&mut self, reference: &ImageRef) -> Result<ResolvedImage, String> {
        // Cloud registration is by REFERENCE, not per-arch bytes: the provider
        // pulls/boots the image itself, so there is no manifest list to fan
        // out. A digest-pinned reference keeps its pin; an unpinned one gets a
        // STABLE pseudo-digest (sha256 of the `name_tag()` string). That
        // pseudo-digest keys the provider template in inventory — it is NOT a
        // content digest and never verifies bytes.
        let list_digest = match &reference.manifest_list_digest {
            Some(pin) => pin.clone(),
            None => super::sha256_local(&reference.name_tag())?,
        };
        let mut per_arch = std::collections::BTreeMap::new();
        per_arch.insert(Arch::Amd64, list_digest.clone());
        Ok(ResolvedImage {
            reference: reference.clone(),
            list_digest,
            per_arch,
        })
    }

    fn image_present(&mut self, _image: &ImageRef, _digest: &Digest) -> Result<bool, String> {
        // "Present" for a cloud host = the provider already has the named
        // template/snapshot (`reach.template`); the digest only keys inventory.
        let (status, body) = get_json(&self.template_list_url(), self.token()?, LIST_TIMEOUT)
            .map_err(|e| format!("cloud image check: {e}"))?;
        if !(200..300).contains(&status) {
            // Reachable but unhelpful (odd status/shape): report absent and
            // let deliver() register idempotently.
            return Ok(false);
        }
        Ok(parse_names(&body, self.list_envelope())
            .iter()
            .any(|n| n == &self.reach.template))
    }

    fn deliver(
        &mut self,
        strategy: DeliveryStrategy,
        image: &ImageRef,
        digest: &Digest,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<Digest, String> {
        if strategy != DeliveryStrategy::ProviderTemplate {
            return Err(format!(
                "cloud delivery only supports provider-template (got {})",
                strategy.as_str()
            ));
        }
        match self.kind {
            ProviderKind::Daytona => {
                // Register a snapshot the provider builds by pulling the image
                // itself — this path REQUIRES the image to live in a registry
                // Daytona can reach (a digest pin is honored when present).
                let source = image.pinned().unwrap_or_else(|| image.name_tag());
                let body = serde_json::json!({
                    "name": self.reach.template,
                    "imageName": source,
                });
                let (status, resp) =
                    post_json(&self.snapshot_url(), self.token()?, &body, REGISTER_TIMEOUT)
                        .map_err(|e| format!("daytona snapshot registration: {e}"))?;
                // 409 = already registered — idempotent success.
                if !(200..300).contains(&status) && status != 409 {
                    return Err(format!(
                        "daytona snapshot registration failed ({status}): {resp}"
                    ));
                }
            }
            ProviderKind::Sprites => {
                // Sprites boot their own base image; "registering" the
                // template means the named sprite exists to serve as the
                // checkpoint source. The full envplan-base-prefix + checkpoint
                // flow is the host crate's follow-up — this adapter only
                // guarantees the named resource exists.
                let token = self.token()?.to_string();
                let api_base = self.reach.api_base.clone();
                let template = self.reach.template.clone();
                block_on(async {
                    let p = Provider::Sprites(SpritesProvider::new(&api_base, &token, &template));
                    p.ensure_exists(&template)
                        .await
                        .map(|_created| ())
                        .map_err(|e| format!("sprites: register template sprite {template:?}: {e}"))
                })??;
            }
        }
        // One indivisible registration step (no byte-level progress to report).
        progress(1, Some(1));
        Ok(digest.clone())
    }

    fn seed_volume(
        &mut self,
        _spec: &VolumeSpec,
        _image: &ImageRef,
        _digest: &Digest,
    ) -> Result<(), String> {
        // Cloud providers fold warm state into checkpoints/snapshots — there
        // are no named volumes to pre-seed, so seeding is a no-op success.
        Ok(())
    }

    fn oci_url(&self) -> Option<String> {
        // No daemon socket to pin; the provider's API is the spawn plane.
        None
    }
}

impl CloudRunner {
    /// Daytona's snapshot registration endpoint (singular, matching the
    /// `/sandbox` convention in `provider.rs`).
    fn snapshot_url(&self) -> String {
        format!("{}/snapshot", self.api_base())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn reach(provider: &str, base: &str) -> CloudReach {
        CloudReach {
            provider: provider.into(),
            api_base: base.into(),
            // Only the unset-var test depends on the environment, and it uses
            // its own uniquely-named var; everything else injects a token.
            api_key_env: "SUPERZEJ_TEST_CLOUD_TOKEN".into(),
            template: "tmpl".into(),
        }
    }

    fn img() -> ImageRef {
        ImageRef::parse("ghcr.io/x/base:v1").unwrap()
    }

    // --- a minimal in-test HTTP server (no mock-server dev-dep in this crate;
    //     provider.rs keeps its tests pure, so the HTTP seam is exercised here
    //     against a real socket) --------------------------------------------

    struct Mock {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let head_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break buf.len(),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let content_len = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                if k.eq_ignore_ascii_case("content-length") {
                    v.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);
        while buf.len() < head_end + content_len {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Serve `responses` (one connection each, in order), capturing requests.
    fn mock_http(responses: &[(u16, &str)]) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let reqs = Arc::clone(&requests);
        let responses: Vec<(u16, String)> =
            responses.iter().map(|(s, b)| (*s, b.to_string())).collect();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let req = read_request(&mut stream);
                reqs.lock().unwrap().push(req);
                let resp = format!(
                    "HTTP/1.1 {status} MOCK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        Mock { base, requests }
    }

    // --- connect ------------------------------------------------------------

    #[test]
    fn connect_pings_the_provider_list_with_bearer_auth() {
        let mock = mock_http(&[(200, r#"{"sprites":[]}"#)]);
        let mut r = CloudRunner::with_token(reach("sprites", &mock.base), "tok");
        r.connect().unwrap();
        let req = mock.requests.lock().unwrap()[0].to_ascii_lowercase();
        assert!(req.starts_with("get /sprites "), "{req}");
        assert!(req.contains("authorization: bearer tok"), "{req}");
    }

    #[test]
    fn connect_daytona_pings_sandbox_list() {
        let mock = mock_http(&[(200, "[]")]);
        let mut r = CloudRunner::with_token(reach("daytona", &mock.base), "tok");
        r.connect().unwrap();
        let req = mock.requests.lock().unwrap()[0].to_ascii_lowercase();
        assert!(req.starts_with("get /sandbox "), "{req}");
    }

    #[test]
    fn connect_maps_auth_failures_to_token_rejected() {
        for status in [401u16, 403] {
            let mock = mock_http(&[(status, r#"{"error":"nope"}"#)]);
            let mut r = CloudRunner::with_token(reach("sprites", &mock.base), "bad");
            let err = r.connect().unwrap_err();
            assert!(err.contains("token rejected"), "{status}: {err}");
            assert!(err.contains("SUPERZEJ_TEST_CLOUD_TOKEN"), "{err}");
        }
    }

    #[test]
    fn connect_connection_error_is_a_plain_message() {
        // A port nothing listens on: transport error, not an auth complaint.
        let mut r = CloudRunner::with_token(reach("sprites", "http://127.0.0.1:9"), "tok");
        let err = r.connect().unwrap_err();
        assert!(err.contains("cloud connect:"), "{err}");
        assert!(!err.contains("token rejected"), "{err}");
    }

    #[test]
    fn connect_without_env_var_names_it() {
        let mut re = reach("sprites", "http://127.0.0.1:9");
        // Uniquely named and never set by anything — no env mutation needed.
        re.api_key_env = "SUPERZEJ_TEST_CLOUD_TOKEN_DEFINITELY_UNSET_7Q".into();
        let mut r = CloudRunner::new(re).unwrap();
        let err = r.connect().unwrap_err();
        assert!(
            err.contains("SUPERZEJ_TEST_CLOUD_TOKEN_DEFINITELY_UNSET_7Q"),
            "{err}"
        );
    }

    #[test]
    fn unknown_provider_is_rejected_at_build() {
        let err = cloud_runner_for(&reach("nimbus", ""))
            .map(|_| ())
            .unwrap_err();
        assert!(err.contains("nimbus"), "{err}");
    }

    // --- probe / install / seed / oci_url ------------------------------------

    #[test]
    fn probe_synthesizes_cloud_managed_caps_and_stubs_hold() {
        let mut r = CloudRunner::with_token(reach("sprites", "http://127.0.0.1:9"), "tok");
        let caps = r.probe().unwrap();
        assert_eq!(
            caps.runtime.as_ref().unwrap().kind,
            RuntimeKind::CloudManaged
        );
        assert_eq!(caps.arch, Arch::Amd64);
        let err = r
            .install_runtime(RuntimeKind::Podman, &mut |_| {})
            .unwrap_err();
        assert!(err.contains("no runtime to install"), "{err}");
        let spec = VolumeSpec::by_name("cargo").unwrap();
        r.seed_volume(&spec, &img(), &Digest::parse(D1).unwrap())
            .unwrap();
        assert_eq!(r.oci_url(), None);
    }

    // --- resolve_image --------------------------------------------------------

    #[test]
    fn resolve_image_pinned_keeps_the_pin_for_amd64() {
        let mut r = CloudRunner::with_token(reach("sprites", ""), "tok");
        let pinned = ImageRef::parse(&format!("ghcr.io/x/base:v1@{D1}")).unwrap();
        let resolved = r.resolve_image(&pinned).unwrap();
        assert_eq!(resolved.list_digest.as_str(), D1);
        assert_eq!(resolved.digest_for(Arch::Amd64).unwrap().as_str(), D1);
        assert_eq!(resolved.per_arch.len(), 1);
    }

    #[test]
    fn resolve_image_unpinned_pseudo_digest_is_stable() {
        let mut r = CloudRunner::with_token(reach("sprites", ""), "tok");
        let a = r.resolve_image(&img()).unwrap();
        let b = r.resolve_image(&img()).unwrap();
        assert_eq!(a.list_digest, b.list_digest, "same input, same digest");
        // It is exactly sha256(name_tag) — the inventory key, not content.
        let expected = super::super::sha256_local(&img().name_tag()).unwrap();
        assert_eq!(a.list_digest, expected);
        // A different reference keys differently.
        let other = r
            .resolve_image(&ImageRef::parse("ghcr.io/x/base:v2").unwrap())
            .unwrap();
        assert_ne!(a.list_digest, other.list_digest);
    }

    // --- image_present / deliver ----------------------------------------------

    #[test]
    fn image_present_daytona_matches_template_name_in_snapshot_list() {
        let d = Digest::parse(D1).unwrap();
        let mock = mock_http(&[(200, r#"[{"name":"tmpl"},{"name":"other"}]"#)]);
        let mut r = CloudRunner::with_token(reach("daytona", &mock.base), "tok");
        assert!(r.image_present(&img(), &d).unwrap());
        let req = mock.requests.lock().unwrap()[0].to_ascii_lowercase();
        assert!(req.starts_with("get /snapshot "), "{req}");

        // Enveloped list without the template ⇒ absent.
        let mock2 = mock_http(&[(200, r#"{"snapshots":[{"name":"zzz"}]}"#)]);
        let mut r2 = CloudRunner::with_token(reach("daytona", &mock2.base), "tok");
        assert!(!r2.image_present(&img(), &d).unwrap());

        // Reachable-but-unhelpful (odd status) ⇒ Ok(false), not an error.
        let mock3 = mock_http(&[(404, "{}")]);
        let mut r3 = CloudRunner::with_token(reach("daytona", &mock3.base), "tok");
        assert!(!r3.image_present(&img(), &d).unwrap());
    }

    #[test]
    fn image_present_sprites_checks_the_sprite_list() {
        let d = Digest::parse(D1).unwrap();
        let mock = mock_http(&[(200, r#"{"sprites":[{"name":"tmpl"}]}"#)]);
        let mut r = CloudRunner::with_token(reach("sprites", &mock.base), "tok");
        assert!(r.image_present(&img(), &d).unwrap());
        let req = mock.requests.lock().unwrap()[0].to_ascii_lowercase();
        assert!(req.starts_with("get /sprites "), "{req}");
    }

    #[test]
    fn deliver_rejects_every_non_template_strategy() {
        let d = Digest::parse(D1).unwrap();
        let mut r = CloudRunner::with_token(reach("sprites", ""), "tok");
        for s in [
            DeliveryStrategy::SshStream { rsync: false },
            DeliveryStrategy::SshStream { rsync: true },
            DeliveryStrategy::RegistryPull,
            DeliveryStrategy::SkopeoRemoteCopy,
            DeliveryStrategy::RemoteBuild,
        ] {
            let err = r.deliver(s, &img(), &d, &mut |_, _| {}).unwrap_err();
            assert!(err.contains("provider-template"), "{}: {err}", s.as_str());
        }
    }

    #[test]
    fn deliver_daytona_registers_a_snapshot_from_the_registry_ref() {
        let d = Digest::parse(D1).unwrap();
        let mock = mock_http(&[(200, "{}")]);
        let mut r = CloudRunner::with_token(reach("daytona", &mock.base), "tok");
        let mut ticks = Vec::new();
        let got = r
            .deliver(
                DeliveryStrategy::ProviderTemplate,
                &img(),
                &d,
                &mut |n, t| ticks.push((n, t)),
            )
            .unwrap();
        assert_eq!(got, d, "returns the digest passed in");
        assert_eq!(ticks, vec![(1, Some(1))]);
        let req = &mock.requests.lock().unwrap()[0];
        assert!(
            req.to_ascii_lowercase().starts_with("post /snapshot "),
            "{req}"
        );
        assert!(req.contains(r#""name":"tmpl""#), "{req}");
        // Unpinned ⇒ registry name:tag; a pin would ride `pinned()` instead.
        assert!(req.contains("ghcr.io/x/base:v1"), "{req}");
    }

    #[test]
    fn deliver_daytona_conflict_is_idempotent_success() {
        let d = Digest::parse(D1).unwrap();
        let mock = mock_http(&[(409, r#"{"error":"exists"}"#)]);
        let mut r = CloudRunner::with_token(reach("daytona", &mock.base), "tok");
        r.deliver(
            DeliveryStrategy::ProviderTemplate,
            &img(),
            &d,
            &mut |_, _| {},
        )
        .unwrap();
    }

    #[test]
    fn deliver_sprites_ensures_the_template_sprite_exists() {
        let d = Digest::parse(D1).unwrap();
        // ensure_exists lists first; the template already exists ⇒ no create.
        let mock = mock_http(&[(200, r#"{"sprites":[{"name":"tmpl"}]}"#)]);
        let mut r = CloudRunner::with_token(reach("sprites", &mock.base), "tok");
        let mut ticks = Vec::new();
        let got = r
            .deliver(
                DeliveryStrategy::ProviderTemplate,
                &img(),
                &d,
                &mut |n, t| ticks.push((n, t)),
            )
            .unwrap();
        assert_eq!(got, d);
        assert_eq!(ticks, vec![(1, Some(1))]);
        let req = mock.requests.lock().unwrap()[0].to_ascii_lowercase();
        assert!(req.starts_with("get /sprites "), "{req}");
    }

    // --- pure helpers -----------------------------------------------------------

    #[test]
    fn parse_names_handles_array_envelope_and_junk() {
        let arr = serde_json::json!([{"name":"a"},{"name":"b"},{"id":"no-name"}]);
        assert_eq!(parse_names(&arr, "sprites"), vec!["a", "b"]);
        let env = serde_json::json!({"snapshots":[{"name":"c"}]});
        assert_eq!(parse_names(&env, "snapshots"), vec!["c"]);
        assert!(parse_names(&serde_json::Value::Null, "sprites").is_empty());
        assert!(parse_names(&serde_json::json!({}), "sprites").is_empty());
    }

    #[test]
    fn api_base_defaults_per_provider_and_trims() {
        let s = CloudRunner::with_token(reach("sprites", ""), "t");
        assert_eq!(s.api_base(), "https://api.sprites.dev/v1");
        let dt = CloudRunner::with_token(reach("daytona", ""), "t");
        assert_eq!(dt.api_base(), "https://app.daytona.io/api");
        let custom = CloudRunner::with_token(reach("sprites", "https://x.test/v9/"), "t");
        assert_eq!(custom.api_base(), "https://x.test/v9");
        assert_eq!(custom.ping_url(), "https://x.test/v9/sprites");
        assert_eq!(
            dt.template_list_url(),
            "https://app.daytona.io/api/snapshot"
        );
        assert_eq!(dt.snapshot_url(), "https://app.daytona.io/api/snapshot");
    }
}
