//! S3-compatible snapshot store (AWS S3, Cloudflare R2, Backblaze B2, MinIO…).
//! rusty-s3 does the SigV4 signing (sans-io, presigned URLs); the transport is
//! the workspace's existing async reqwest driven from a store-owned
//! current-thread runtime — the trait is synchronous and only ever called off
//! the compositor loop (same shape as `agent::block_on_provider`).
//!
//! Object layout mirrors the fs backend under the configured key prefix:
//! `<prefix>/<repo>/<worktree>/<env>/<id>/{manifest.json,bundle,patch,tar}`.

use std::time::Duration;

use anyhow::{Context, Result};
use rusty_s3::actions::ListObjectsV2;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use thegn_core::config_env_tables::SnapshotStoreConfig;
use thegn_core::snapshot_meta::{SnapshotKey, SnapshotManifest};

use super::SnapshotStore;

/// Presigned-URL validity. Requests fire immediately after signing; this only
/// needs to outlive one slow artifact upload.
const SIGN_TTL: Duration = Duration::from_secs(600);
/// One artifact transfer's HTTP budget.
const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

pub struct S3SnapshotStore {
    bucket: Bucket,
    creds: Credentials,
    prefix: String,
    rt: tokio::runtime::Runtime,
    http: reqwest::Client,
}

impl S3SnapshotStore {
    pub fn new(
        cfg: &SnapshotStoreConfig,
        resolve_secret: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let name = cfg.bucket.trim();
        if name.is_empty() {
            anyhow::bail!("[lifecycle.snapshot] backend = \"s3\" requires `bucket`");
        }
        let region = cfg.region.trim();
        // AWS wants virtual-host style; S3-compatibles behind a custom
        // endpoint (R2/B2/MinIO) conventionally use path style.
        let (endpoint, style) = if cfg.endpoint.trim().is_empty() {
            (
                format!("https://s3.{region}.amazonaws.com"),
                UrlStyle::VirtualHost,
            )
        } else {
            (cfg.endpoint.trim().to_string(), UrlStyle::Path)
        };
        let endpoint: reqwest::Url = endpoint
            .parse()
            .with_context(|| format!("[lifecycle.snapshot] endpoint {endpoint:?}"))?;
        let bucket = Bucket::new(endpoint, style, name.to_string(), region.to_string())
            .map_err(|e| anyhow::anyhow!("[lifecycle.snapshot] bucket config: {e}"))?;
        let access = resolve_secret(&cfg.access_key).with_context(|| {
            format!("snapshot store access_key ({}) unresolved", cfg.access_key)
        })?;
        let secret = resolve_secret(&cfg.secret_key).with_context(|| {
            format!("snapshot store secret_key ({}) unresolved", cfg.secret_key)
        })?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("snapshot store runtime")?;
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("snapshot store http client")?;
        Ok(S3SnapshotStore {
            bucket,
            creds: Credentials::new(access, secret),
            prefix: cfg.prefix.trim_matches('/').to_string(),
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

    fn put_bytes(&self, object: &str, data: &[u8]) -> Result<()> {
        let url = self
            .bucket
            .put_object(Some(&self.creds), object)
            .sign(SIGN_TTL);
        let body = data.to_vec();
        self.rt.block_on(async {
            self.http
                .put(url)
                .body(body)
                .send()
                .await
                .with_context(|| format!("PUT {object}"))?
                .error_for_status()
                .with_context(|| format!("PUT {object}"))?;
            Ok(())
        })
    }

    fn get_bytes(&self, object: &str) -> Result<Vec<u8>> {
        let url = self
            .bucket
            .get_object(Some(&self.creds), object)
            .sign(SIGN_TTL);
        self.rt.block_on(async {
            let resp = self
                .http
                .get(url)
                .send()
                .await
                .with_context(|| format!("GET {object}"))?
                .error_for_status()
                .with_context(|| format!("GET {object}"))?;
            Ok(resp.bytes().await?.to_vec())
        })
    }

    fn delete_object(&self, object: &str) -> Result<()> {
        let url = self
            .bucket
            .delete_object(Some(&self.creds), object)
            .sign(SIGN_TTL);
        self.rt.block_on(async {
            self.http
                .delete(url)
                .send()
                .await
                .with_context(|| format!("DELETE {object}"))?
                .error_for_status()
                .with_context(|| format!("DELETE {object}"))?;
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
                    .with_context(|| format!("LIST {prefix}"))?
                    .error_for_status()
                    .with_context(|| format!("LIST {prefix}"))?;
                anyhow::Ok(resp.text().await?)
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
        let data = self.get_bytes(&self.object_key(key, id, "manifest.json"))?;
        Ok(serde_json::from_slice(&data)?)
    }

    fn list(&self, key: &SnapshotKey) -> Result<Vec<SnapshotManifest>> {
        // Only manifest objects make a snapshot real (same torn-write contract
        // as the fs backend).
        let mut out = Vec::new();
        for k in self.list_keys(&self.key_root(key))? {
            if !k.ends_with("/manifest.json") {
                continue;
            }
            let data = self.get_bytes(&k)?;
            if let Ok(m) = serde_json::from_slice::<SnapshotManifest>(&data) {
                out.push(m);
            }
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

    fn cfg(endpoint: &str) -> SnapshotStoreConfig {
        SnapshotStoreConfig {
            bucket: "sz-snaps".into(),
            endpoint: endpoint.into(),
            region: "auto".into(),
            prefix: "thegn".into(),
            access_key: "env:TG_TEST_S3_ACCESS".into(),
            secret_key: "env:TG_TEST_S3_SECRET".into(),
            ..Default::default()
        }
    }

    fn resolver(s: &str) -> Option<String> {
        match s {
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
                .starts_with("/sz-snaps/thegn/repo/wt/hetzner/id1/tar")
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
        assert_eq!(url.host_str(), Some("sz-snaps.s3.auto.amazonaws.com"));
        assert!(url.path().starts_with("/thegn/x"));
    }

    #[test]
    fn missing_bucket_or_secret_is_a_config_error() {
        let mut c = cfg("https://minio.local");
        c.bucket = String::new();
        assert!(S3SnapshotStore::new(&c, &resolver).is_err());
        let c = cfg("https://minio.local");
        assert!(S3SnapshotStore::new(&c, &|_| None).is_err());
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
}
