//! S3-compatible snapshot store (AWS S3, Cloudflare R2, Backblaze B2, MinIO…).
//! rusty-s3 does the SigV4 signing (sans-io, presigned URLs); the transport is
//! the workspace's existing async reqwest driven from a store-owned
//! current-thread runtime — the trait is synchronous and only ever called off
//! the compositor loop (same shape as `agent::block_on_provider`).
//! S3 uploads sign and send SSE-S3 (AES256) by default; compatible stores that
//! reject it can opt out in `[lifecycle.snapshot]`.
//!
//! Object layout mirrors the fs backend under the configured key prefix:
//! `<prefix>/<repo>/<worktree>/<env>/<id>/{manifest.json,bundle,patch,tar}`.
//! Manifests are bound to their object key and reads are capped.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use rusty_s3::actions::ListObjectsV2;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use thegn_core::config_env_tables::SnapshotStoreConfig;
use thegn_core::secretref::{BareAs, SecretRef};
use thegn_core::snapshot_meta::{SnapshotKey, SnapshotManifest};

use super::{SnapshotAlreadyExists, SnapshotStore};

/// Presigned-URL validity. Requests fire immediately after signing; this only
/// needs to outlive one slow artifact upload.
const SIGN_TTL: Duration = Duration::from_secs(600);
const SSE_HEADER: &str = "x-amz-server-side-encryption";
const SSE_AES256: &str = "AES256";
const IF_NONE_MATCH_HEADER: &str = "if-none-match";
const IF_NONE_MATCH_ANY: &str = "*";
/// Maximum size of one snapshot object, matching the filesystem backend's per-file limit.
const MAX_SNAPSHOT_OBJECT_BYTES: u64 = 1024 * 1024 * 1024;
/// One artifact transfer's HTTP budget.
const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

fn require_success(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("{what}: unexpected HTTP status {status}");
    }
    Ok(response)
}

fn manifest_id_from_key<'a>(root: &str, object: &'a str) -> Option<&'a str> {
    let id = object.strip_prefix(root)?.strip_suffix("/manifest.json")?;
    (!id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\\'))
        .then_some(id)
}

fn decode_manifest(id: &str, data: &[u8]) -> Result<SnapshotManifest> {
    let manifest: SnapshotManifest = serde_json::from_slice(data)
        .with_context(|| format!("snapshot manifest {id} is not valid JSON"))?;
    anyhow::ensure!(
        manifest.id == id,
        "snapshot manifest id {:?} does not match its key id {id:?}",
        manifest.id
    );
    Ok(manifest)
}

/// Parse and admit a custom endpoint. Plain HTTP is reserved for local S3
/// emulators; all other endpoints must protect snapshot contents in transit.
fn admit_endpoint(raw: &str) -> Result<(reqwest::Url, bool)> {
    let endpoint = raw.parse::<reqwest::Url>().map_err(|_| {
        anyhow::anyhow!(
            "[lifecycle.snapshot] endpoint must use https (http is allowed only for a loopback host)"
        )
    })?;
    let host = endpoint.host_str();
    let loopback = host.is_some_and(|host| {
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        let address = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        address
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    });
    let admitted_scheme = endpoint.scheme() == "https" || (endpoint.scheme() == "http" && loopback);
    if !admitted_scheme
        || host.is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        anyhow::bail!(
            "[lifecycle.snapshot] endpoint must use https (http is allowed only for a loopback host)"
        );
    }
    Ok((endpoint, loopback))
}

pub struct S3SnapshotStore {
    bucket: Bucket,
    creds: Credentials,
    prefix: String,
    sse: bool,
    conditional_create: bool,
    rt: tokio::runtime::Runtime,
    http: reqwest::Client,
}

impl S3SnapshotStore {
    pub fn new(
        cfg: &SnapshotStoreConfig,
        resolve_secret: &dyn Fn(&SecretRef, &str) -> Option<String>,
    ) -> Result<Self> {
        let name = cfg.bucket.trim();
        if name.is_empty() {
            anyhow::bail!("[lifecycle.snapshot] backend = \"s3\" requires `bucket`");
        }
        let region = cfg.region.trim();
        // AWS wants virtual-host style; S3-compatibles behind a custom
        // endpoint (R2/B2/MinIO) conventionally use path style.
        let (endpoint, loopback, style) = if cfg.endpoint.trim().is_empty() {
            (
                format!("https://s3.{region}.amazonaws.com")
                    .parse::<reqwest::Url>()
                    .context("[lifecycle.snapshot] AWS endpoint")?,
                false,
                UrlStyle::VirtualHost,
            )
        } else {
            let (endpoint, loopback) = admit_endpoint(cfg.endpoint.trim())?;
            (endpoint, loopback, UrlStyle::Path)
        };
        let bucket = Bucket::new(endpoint, style, name.to_string(), region.to_string())
            .map_err(|e| anyhow::anyhow!("[lifecycle.snapshot] bucket config: {e}"))?;
        let access_ref = SecretRef::parse(&cfg.access_key, BareAs::Literal);
        let secret_ref = SecretRef::parse(&cfg.secret_key, BareAs::Literal);
        let access = resolve_secret(&access_ref, "snapshot:s3:access-key").with_context(|| {
            format!(
                "snapshot store access_key ({}) unresolved",
                access_ref.audit_name()
            )
        })?;
        let secret = resolve_secret(&secret_ref, "snapshot:s3:secret-key").with_context(|| {
            format!(
                "snapshot store secret_key ({}) unresolved",
                secret_ref.audit_name()
            )
        })?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("snapshot store runtime")?;
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .https_only(!loopback)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("snapshot store http client")?;
        Ok(S3SnapshotStore {
            bucket,
            creds: Credentials::new(access, secret),
            prefix: cfg.prefix.trim_matches('/').to_string(),
            sse: cfg.server_side_encryption,
            conditional_create: cfg.conditional_create,
            rt,
            http,
        })
    }

    /// Full object key for one artifact (no leading slash; empty prefix ok).
    fn object_key(&self, key: &SnapshotKey, id: &str, name: &str) -> String {
        let tail = format!("{}/{id}/{name}", key.prefix());
        if self.prefix.is_empty() {
            tail
        } else {
            format!("{}/{tail}", self.prefix)
        }
    }

    fn key_root(&self, key: &SnapshotKey) -> String {
        let tail = format!("{}/", key.prefix());
        if self.prefix.is_empty() {
            tail
        } else {
            format!("{}/{tail}", self.prefix)
        }
    }

    fn put_request(&self, object: &str, data: &[u8]) -> Result<reqwest::Request> {
        let mut action = self.bucket.put_object(Some(&self.creds), object);
        if self.sse {
            action.headers_mut().insert(SSE_HEADER, SSE_AES256);
        }
        // The current layout has only generation-scoped artifact and manifest
        // keys, all immutable. A future mutable pointer such as `latest` must
        // use a separate request path and must not inherit this precondition.
        if self.conditional_create {
            action
                .headers_mut()
                .insert(IF_NONE_MATCH_HEADER, IF_NONE_MATCH_ANY);
        }
        let url = action.sign(SIGN_TTL);
        let mut request = self.http.put(url).body(data.to_vec());
        if self.sse {
            request = request.header(SSE_HEADER, SSE_AES256);
        }
        if self.conditional_create {
            request = request.header(IF_NONE_MATCH_HEADER, IF_NONE_MATCH_ANY);
        }
        request.build().with_context(|| format!("PUT {object}"))
    }

    fn put_bytes(&self, object: &str, data: &[u8]) -> Result<()> {
        let request = self.put_request(object, data)?;
        self.rt.block_on(async {
            let response = self
                .http
                .execute(request)
                .await
                .map_err(reqwest::Error::without_url)
                .with_context(|| format!("PUT {object}"))?;
            if response.status() == reqwest::StatusCode::PRECONDITION_FAILED {
                return Err(anyhow::Error::new(SnapshotAlreadyExists {
                    object: object.to_string(),
                }));
            }
            require_success(response, &format!("PUT {object}"))?;
            Ok(())
        })
    }

    fn get_bytes(&self, object: &str) -> Result<Vec<u8>> {
        self.get_bytes_capped(object, MAX_SNAPSHOT_OBJECT_BYTES)
    }

    fn get_bytes_capped(&self, object: &str, cap: u64) -> Result<Vec<u8>> {
        let url = self
            .bucket
            .get_object(Some(&self.creds), object)
            .sign(SIGN_TTL);
        self.rt.block_on(async {
            let response = self
                .http
                .get(url)
                .send()
                .await
                .map_err(reqwest::Error::without_url)
                .with_context(|| format!("GET {object}"))?;
            let mut response = require_success(response, &format!("GET {object}"))?;
            if response.content_length().is_some_and(|len| len > cap) {
                anyhow::bail!("GET {object}: object exceeds the {cap}-byte snapshot limit");
            }
            let mut body = Vec::new();
            let mut total = 0_u64;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(reqwest::Error::without_url)
                .with_context(|| format!("GET {object}"))?
            {
                let chunk_len = u64::try_from(chunk.len())?;
                if chunk_len > cap - total {
                    anyhow::bail!("GET {object}: object exceeds the {cap}-byte snapshot limit");
                }
                total += chunk_len;
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }

    fn delete_object(&self, object: &str) -> Result<()> {
        let url = self
            .bucket
            .delete_object(Some(&self.creds), object)
            .sign(SIGN_TTL);
        self.rt.block_on(async {
            let response = self
                .http
                .delete(url)
                .send()
                .await
                .map_err(reqwest::Error::without_url)
                .with_context(|| format!("DELETE {object}"))?;
            require_success(response, &format!("DELETE {object}"))?;
            Ok(())
        })
    }

    /// Every object key under `prefix`, following continuation tokens.
    fn list_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.creds));
            action.with_prefix(prefix);
            if let Some(t) = &token {
                action.with_continuation_token(t.clone());
            }
            let url = action.sign(SIGN_TTL);
            let text = self.rt.block_on(async {
                let resp = self
                    .http
                    .get(url)
                    .send()
                    .await
                    .map_err(reqwest::Error::without_url)
                    .with_context(|| format!("LIST {prefix}"))?;
                let resp = require_success(resp, &format!("LIST {prefix}"))?;
                resp.text()
                    .await
                    .map_err(reqwest::Error::without_url)
                    .with_context(|| format!("LIST {prefix}"))
            })?;
            let parsed = ListObjectsV2::parse_response(&text)
                .with_context(|| format!("LIST {prefix}: bad response"))?;
            out.extend(parsed.contents.into_iter().map(|c| c.key));
            match parsed.next_continuation_token {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }
}

impl SnapshotStore for S3SnapshotStore {
    fn put(&self, key: &SnapshotKey, id: &str, name: &str, data: &[u8]) -> Result<()> {
        self.put_bytes(&self.object_key(key, id, name), data)
    }

    fn get(&self, key: &SnapshotKey, id: &str, name: &str) -> Result<Vec<u8>> {
        self.get_bytes(&self.object_key(key, id, name))
    }

    fn put_manifest(&self, key: &SnapshotKey, manifest: &SnapshotManifest) -> Result<()> {
        let data = serde_json::to_vec_pretty(manifest)?;
        self.put_bytes(&self.object_key(key, &manifest.id, "manifest.json"), &data)
    }

    fn get_manifest(&self, key: &SnapshotKey, id: &str) -> Result<SnapshotManifest> {
        let root = self.key_root(key);
        let object = self.object_key(key, id, "manifest.json");
        if manifest_id_from_key(&root, &object).is_none() {
            anyhow::bail!("invalid snapshot id {id:?}");
        }
        decode_manifest(id, &self.get_bytes(&object)?)
    }

    fn list(&self, key: &SnapshotKey) -> Result<Vec<SnapshotManifest>> {
        // Only manifest objects make a snapshot real (same torn-write contract
        // as the fs backend).
        let mut out = Vec::new();
        let root = self.key_root(key);
        for k in self.list_keys(&root)? {
            if !k.ends_with("/manifest.json") {
                continue;
            }
            let id = manifest_id_from_key(&root, &k)
                .ok_or_else(|| anyhow::anyhow!("unexpected snapshot manifest object {k}"))?;
            let data = self.get_bytes(&k)?;
            out.push(decode_manifest(id, &data)?);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn delete(&self, key: &SnapshotKey, id: &str) -> Result<()> {
        let prefix = format!("{}{id}/", self.key_root(key));
        for k in self.list_keys(&prefix)? {
            self.delete_object(&k)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::thread::JoinHandle;

    fn cfg(endpoint: &str) -> SnapshotStoreConfig {
        SnapshotStoreConfig {
            bucket: "tg-snaps".into(),
            endpoint: endpoint.into(),
            region: "auto".into(),
            prefix: "thegn".into(),
            access_key: "env:TG_TEST_S3_ACCESS".into(),
            secret_key: "env:TG_TEST_S3_SECRET".into(),
            ..Default::default()
        }
    }

    fn resolver(s: &SecretRef, _consumer: &str) -> Option<String> {
        match s.audit_name().as_str() {
            "env:TG_TEST_S3_ACCESS" => Some("AKIATEST".into()),
            "env:TG_TEST_S3_SECRET" => Some("sekrit".into()),
            _ => None,
        }
    }

    fn key() -> SnapshotKey {
        SnapshotKey {
            repo_slug: "repo".into(),
            worktree_slug: "wt".into(),
            env: "hetzner".into(),
        }
    }

    fn response_server(response: String) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            stream.write_all(response.as_bytes()).unwrap();
        });
        (address, handle)
    }

    fn ok_response(headers: &str, body: &str) -> String {
        format!("HTTP/1.1 200 OK\r\n{headers}Connection: close\r\n\r\n{body}")
    }

    fn signed_headers(url: &reqwest::Url) -> Option<String> {
        url.query_pairs()
            .find_map(|(key, value)| (key == "X-Amz-SignedHeaders").then(|| value.into_owned()))
    }

    #[test]
    fn put_signs_and_sends_sse_aes256_by_default() {
        let s = S3SnapshotStore::new(&cfg("https://minio.local:9000"), &resolver).unwrap();
        let request = s.put_request("thegn/x/bundle", b"data").unwrap();

        assert!(
            signed_headers(request.url())
                .is_some_and(|headers| headers.split(';').any(|header| header == SSE_HEADER))
        );
        assert_eq!(
            request.headers().get(SSE_HEADER).unwrap().to_str().unwrap(),
            SSE_AES256
        );
    }

    #[test]
    fn put_signs_and_sends_conditional_create_by_default() {
        let s = S3SnapshotStore::new(&cfg("https://minio.local:9000"), &resolver).unwrap();
        let request = s.put_request("thegn/x/bundle", b"data").unwrap();

        assert!(signed_headers(request.url()).is_some_and(|headers| {
            headers
                .split(';')
                .any(|header| header == IF_NONE_MATCH_HEADER)
        }));
        assert_eq!(
            request
                .headers()
                .get(IF_NONE_MATCH_HEADER)
                .unwrap()
                .to_str()
                .unwrap(),
            IF_NONE_MATCH_ANY
        );
    }

    #[test]
    fn put_without_sse_when_opted_out() {
        let cfg = SnapshotStoreConfig {
            server_side_encryption: false,
            ..cfg("https://minio.local:9000")
        };
        let s = S3SnapshotStore::new(&cfg, &resolver).unwrap();
        let request = s.put_request("thegn/x/bundle", b"data").unwrap();

        assert!(
            !signed_headers(request.url())
                .is_some_and(|headers| { headers.split(';').any(|header| header == SSE_HEADER) })
        );
        assert!(!request.headers().contains_key(SSE_HEADER));
    }

    #[test]
    fn put_without_conditional_create_when_opted_out() {
        let cfg = SnapshotStoreConfig {
            conditional_create: false,
            ..cfg("https://minio.local:9000")
        };
        let s = S3SnapshotStore::new(&cfg, &resolver).unwrap();
        let request = s.put_request("thegn/x/bundle", b"data").unwrap();

        assert!(!signed_headers(request.url()).is_some_and(|headers| {
            headers
                .split(';')
                .any(|header| header == IF_NONE_MATCH_HEADER)
        }));
        assert!(!request.headers().contains_key(IF_NONE_MATCH_HEADER));
    }

    #[test]
    fn precondition_failed_maps_to_snapshot_already_exists() {
        let (address, server) =
            response_server("HTTP/1.1 412 Precondition Failed\r\nConnection: close\r\n\r\n".into());
        let s = S3SnapshotStore::new(&cfg(&format!("http://{address}")), &resolver).unwrap();
        let error = s
            .put_bytes("thegn/x/bundle", b"data")
            .expect_err("412 must be a collision");
        server.join().unwrap();

        assert_eq!(
            error.downcast_ref::<SnapshotAlreadyExists>(),
            Some(&SnapshotAlreadyExists {
                object: "thegn/x/bundle".into(),
            })
        );
    }

    #[test]
    fn get_requests_never_carry_the_sse_header() {
        let s = S3SnapshotStore::new(&cfg("https://minio.local:9000"), &resolver).unwrap();
        let url = s
            .bucket
            .get_object(Some(&s.creds), "thegn/x/bundle")
            .sign(SIGN_TTL);

        assert_eq!(signed_headers(&url).as_deref(), Some("host"));
    }

    #[test]
    fn custom_endpoint_uses_path_style_and_prefixed_keys() {
        // rusty-s3 signing is sans-io: assert request shape with no network.
        let s = S3SnapshotStore::new(&cfg("https://minio.local:9000"), &resolver).unwrap();
        assert_eq!(
            s.object_key(&key(), "00000000000000000001-abc", "bundle"),
            "thegn/repo/wt/hetzner/00000000000000000001-abc/bundle"
        );
        let url = s
            .bucket
            .get_object(Some(&s.creds), &s.object_key(&key(), "id1", "tar"))
            .sign(SIGN_TTL);
        assert_eq!(url.host_str(), Some("minio.local"));
        // Path style: bucket in the path, followed by the object key.
        assert!(
            url.path()
                .starts_with("/tg-snaps/thegn/repo/wt/hetzner/id1/tar")
        );
        assert!(
            url.query().unwrap_or("").contains("X-Amz-Signature"),
            "presigned"
        );
    }

    #[test]
    fn aws_endpoint_uses_virtual_host_style() {
        let s = S3SnapshotStore::new(&cfg(""), &resolver).unwrap();
        let url = s
            .bucket
            .get_object(Some(&s.creds), "thegn/x")
            .sign(SIGN_TTL);
        assert_eq!(url.host_str(), Some("tg-snaps.s3.auto.amazonaws.com"));
        assert!(url.path().starts_with("/thegn/x"));
    }

    #[test]
    fn remote_http_endpoints_are_refused_before_resolving_secrets() {
        for endpoint in ["http://203.0.113.5:9000", "http://minio.lan"] {
            let result = S3SnapshotStore::new(&cfg(endpoint), &|_, _| {
                panic!("endpoint validation must precede secret resolution")
            });
            let error = result.err().expect("remote HTTP endpoint must be refused");
            assert!(error.to_string().contains("must use https"));
            assert!(
                error
                    .to_string()
                    .starts_with("[lifecycle.snapshot] endpoint")
            );
        }
    }

    #[test]
    fn loopback_http_endpoints_are_accepted() {
        for endpoint in [
            "http://127.0.0.1:9000",
            "http://127.1.2.3:9000",
            "http://localhost:9000",
            "http://LOCALHOST:9000",
            "http://[::1]:9000",
        ] {
            assert!(
                S3SnapshotStore::new(&cfg(endpoint), &resolver).is_ok(),
                "loopback endpoint should be accepted: {endpoint}"
            );
        }
    }

    #[test]
    fn unsafe_or_non_http_custom_endpoints_are_refused_without_echoing_credentials() {
        for endpoint in [
            "https://user:pw@minio.local",
            "https://minio.local/?x=1",
            "https://minio.local/#f",
            "ftp://minio.local",
        ] {
            let result = S3SnapshotStore::new(&cfg(endpoint), &resolver);
            let error = result.err().expect("unsafe endpoint must be refused");
            assert!(error.to_string().contains("must use https"));
            assert!(
                error
                    .to_string()
                    .starts_with("[lifecycle.snapshot] endpoint")
            );
            if endpoint.contains("user:pw") {
                assert!(!error.to_string().contains("pw"));
            }
        }
    }

    #[test]
    fn missing_bucket_or_secret_is_a_config_error() {
        let mut c = cfg("https://minio.local");
        c.bucket = String::new();
        assert!(S3SnapshotStore::new(&c, &resolver).is_err());
        let c = cfg("https://minio.local");
        assert!(S3SnapshotStore::new(&c, &|_, _| None).is_err());
    }

    #[test]
    fn empty_prefix_produces_no_leading_slash() {
        let mut c = cfg("https://minio.local");
        c.prefix = String::new();
        let s = S3SnapshotStore::new(&c, &resolver).unwrap();
        assert_eq!(
            s.object_key(&key(), "id", "patch"),
            "repo/wt/hetzner/id/patch"
        );
        assert_eq!(s.key_root(&key()), "repo/wt/hetzner/");
    }

    #[test]
    fn manifest_key_binding_accepts_only_a_direct_child_id() {
        let root = "thegn/repo/wt/hetzner/";
        assert_eq!(
            manifest_id_from_key(root, "thegn/repo/wt/hetzner/id1/manifest.json"),
            Some("id1")
        );
        for object in [
            "thegn/repo/wt/hetzner/a/b/manifest.json",
            "thegn/repo/wt/hetzner//manifest.json",
            "thegn/repo/wt/hetzner/../manifest.json",
            "other/id1/manifest.json",
            "thegn/repo/wt/hetzner/id1/bundle",
        ] {
            assert_eq!(manifest_id_from_key(root, object), None, "{object}");
        }
    }

    #[test]
    fn decode_manifest_refuses_a_mismatched_id() {
        let manifest = SnapshotManifest::new(Some("abcd1234"), "main", 10, Vec::new());
        let data = serde_json::to_vec(&manifest).unwrap();
        let error = decode_manifest("different-id", &data).unwrap_err();
        assert!(error.to_string().contains("does not match"));
        assert_eq!(decode_manifest(&manifest.id, &data).unwrap(), manifest);
    }

    #[test]
    fn decode_manifest_refuses_malformed_json() {
        let error = decode_manifest("id1", b"not json").unwrap_err();
        assert!(error.to_string().contains("not valid JSON"));
    }

    #[test]
    fn get_refuses_a_declared_length_over_the_cap() {
        let (address, server) =
            response_server(ok_response("Content-Length: 16\r\n", "0123456789abcdef"));
        let s = S3SnapshotStore::new(&cfg(&format!("http://{address}")), &resolver).unwrap();
        let error = s.get_bytes_capped("thegn/x/bundle", 8).unwrap_err();
        server.join().unwrap();
        assert!(
            error
                .to_string()
                .contains("exceeds the 8-byte snapshot limit")
        );
    }

    #[test]
    fn get_refuses_a_streamed_body_over_the_cap() {
        let response = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10\r\n0123456789abcdef\r\n0\r\n\r\n";
        let (address, server) = response_server(response.into());
        let s = S3SnapshotStore::new(&cfg(&format!("http://{address}")), &resolver).unwrap();
        let error = s.get_bytes_capped("thegn/x/bundle", 8).unwrap_err();
        server.join().unwrap();
        assert!(
            error
                .to_string()
                .contains("exceeds the 8-byte snapshot limit")
        );
    }

    #[test]
    fn get_does_not_follow_a_redirect() {
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let other_address = other.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{other_address}/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let (address, server) = response_server(response);
        let s = S3SnapshotStore::new(&cfg(&format!("http://{address}")), &resolver).unwrap();
        let error = s.get_bytes("thegn/x/bundle").unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("unexpected HTTP status 307"));
        other.set_nonblocking(true).unwrap();
        let error = other.accept().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn transport_errors_do_not_echo_the_presigned_url() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let s = S3SnapshotStore::new(&cfg(&format!("http://{address}")), &resolver).unwrap();
        let error = s.get_bytes("thegn/x/bundle").unwrap_err();
        let message = format!("{error:#}");
        assert!(!message.contains("X-Amz-Signature"));
        assert!(!message.contains("X-Amz-Credential"));
    }

    #[test]
    fn get_within_the_cap_returns_the_body() {
        let (address, server) = response_server(ok_response("Content-Length: 4\r\n", "data"));
        let s = S3SnapshotStore::new(&cfg(&format!("http://{address}")), &resolver).unwrap();
        assert_eq!(s.get_bytes_capped("thegn/x/bundle", 8).unwrap(), b"data");
        server.join().unwrap();
    }
}
