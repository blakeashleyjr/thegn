//! One sprite's status over the org API: `GET /v1/sprites/{name}`. A read
//! that never wakes the sprite (no exec, no resume), used by `thegn env adopt`
//! and the opt-in lifetime reaper.
//!
//! The name is pushed as one percent-encoded path segment (a `/` in it can
//! never address another route), the body read is bounded and the request
//! carries `CONTROL_TIMEOUT`. Errors name the sprite, never the token or the
//! request URL.

use anyhow::{Context, Result, anyhow, bail};

use crate::provider::{CONTROL_TIMEOUT, redacted_transport_error, response_json};

/// The longest API string kept from an inspect response (longer is refused).
const MAX_FIELD_LEN: usize = 256;

/// What `GET /v1/sprites/{name}` reports about one sprite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpriteInfo {
    pub name: String,
    pub status: String,
    pub created_at: Option<String>,
}

/// A string field: absent → `None`; present but not a string, or longer than
/// [`MAX_FIELD_LEN`] → `Err`.
fn checked_str(obj: &serde_json::Value, key: &str) -> Result<Option<String>> {
    match obj.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => {
            if s.len() > MAX_FIELD_LEN {
                bail!("sprites: inspect field {key} is too long");
            }
            Ok(Some(s.clone()))
        }
        Some(_) => bail!("sprites: inspect field {key} is not a string"),
    }
}

/// Parse one sprite object from the Sprites API (tolerating a
/// `{ "sprite": { … } }` envelope). `name` and `status` are required non-empty
/// strings; `created_at` is an optional string.
pub fn parse_info(v: &serde_json::Value) -> Result<SpriteInfo> {
    let obj = v
        .get("sprite")
        .filter(|inner| inner.is_object())
        .unwrap_or(v);
    if !obj.is_object() {
        bail!("sprites: inspect response is not an object");
    }
    let name = checked_str(obj, "name")?
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("sprites: inspect response has no sprite name"))?;
    let status = checked_str(obj, "status")?
        .filter(|status| !status.is_empty())
        .ok_or_else(|| anyhow!("sprites: inspect response for {name} has no status"))?;
    let created_at = checked_str(obj, "created_at")?.filter(|at| !at.is_empty());
    Ok(SpriteInfo {
        name,
        status,
        created_at,
    })
}

impl crate::provider::SpritesProvider {
    /// GET /v1/sprites/{name}; 404 → Ok(None). Never wakes the sprite (org API).
    pub async fn inspect(&self, name: &str) -> Result<Option<SpriteInfo>> {
        let http = self.sprites_http()?;
        anyhow::ensure!(!name.is_empty(), "sprites: empty sprite name");
        // A dot segment would be resolved by the URL, not encoded.
        anyhow::ensure!(
            name != "." && name != "..",
            "sprites: invalid sprite name {name}"
        );
        let mut url = reqwest::Url::parse(http.base)
            .map_err(|_| anyhow!("sprites: invalid API base for inspecting {name}"))?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("sprites: invalid inspect URL for {name}"))?
            .pop_if_empty()
            .push("sprites")
            .push(name);
        let resp = http
            .client
            .get(url)
            .bearer_auth(http.token)
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .map_err(|error| redacted_transport_error("sprites", "inspect", &error))
            .with_context(|| format!("sprites: inspect {name}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("sprites: inspect {name} failed ({status})");
        }
        let body = response_json(resp, "sprites", "inspect")
            .await
            .with_context(|| format!("sprites: inspect {name}"))?;
        let info = parse_info(&body).with_context(|| format!("sprites: inspect {name}"))?;
        if info.name != name {
            bail!("sprites: inspect {name} returned a different sprite; refusing it");
        }
        Ok(Some(info))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn parses_the_api_fixture() {
        let v = serde_json::json!({
            "id": "sprite-0123",
            "name": "sx-adopt",
            "organization": "org",
            "status": "warm",
            "url": "https://sx-adopt.sprites.app",
            "created_at": "2026-11-18T10:00:00Z",
            "updated_at": "2026-11-18T11:00:00Z"
        });
        assert_eq!(
            parse_info(&v).unwrap(),
            SpriteInfo {
                name: "sx-adopt".into(),
                status: "warm".into(),
                created_at: Some("2026-11-18T10:00:00Z".into()),
            }
        );
        let wrapped = serde_json::json!({"sprite": {"name": "sx-b", "status": "cold"}});
        let info = parse_info(&wrapped).unwrap();
        assert_eq!(info.name, "sx-b");
        assert_eq!(info.created_at, None);
    }

    #[test]
    fn parse_refuses_unchecked_fields() {
        for bad in [
            serde_json::json!({"status": "warm"}),
            serde_json::json!({"name": "", "status": "warm"}),
            serde_json::json!({"name": "sx", "status": 3}),
            serde_json::json!({"name": "sx"}),
            serde_json::json!({"name": "sx", "status": "warm", "created_at": 7}),
            serde_json::json!({"name": "sx", "status": "x".repeat(MAX_FIELD_LEN + 1)}),
            serde_json::json!([1, 2]),
        ] {
            assert!(parse_info(&bad).is_err(), "{bad}");
        }
    }

    /// A one-shot HTTP fake: records each request line and answers every
    /// request with `status` and `body`.
    fn fake(status: &'static str, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                rec.lock().unwrap().push(line.trim_end().to_string());
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim_end().is_empty() {
                        break;
                    }
                }
                if write!(
                    writer,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .is_err()
                {
                    continue;
                }
            }
        });
        (format!("http://127.0.0.1:{port}/v1"), seen)
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn inspect_reads_one_sprite() {
        let (base, seen) = fake(
            "200 OK",
            r#"{"name":"sx-a","status":"cold","created_at":"2026-11-01T00:00:00Z"}"#,
        );
        let p = crate::provider::SpritesProvider::new(&base, "tok-secret", "");
        let info = block_on(p.inspect("sx-a")).unwrap().unwrap();
        assert_eq!(info.status, "cold");
        assert_eq!(seen.lock().unwrap()[0], "GET /v1/sprites/sx-a HTTP/1.1");
    }

    #[test]
    fn inspect_404_is_none() {
        let (base, _) = fake("404 Not Found", r#"{"error":"not found"}"#);
        let p = crate::provider::SpritesProvider::new(&base, "tok-secret", "");
        assert_eq!(block_on(p.inspect("sx-gone")).unwrap(), None);
    }

    #[test]
    fn inspect_percent_encodes_the_name() {
        let (base, seen) = fake("404 Not Found", "");
        let p = crate::provider::SpritesProvider::new(&base, "tok-secret", "");
        assert_eq!(block_on(p.inspect("a/../b")).unwrap(), None);
        let line = seen.lock().unwrap()[0].clone();
        assert!(line.starts_with("GET /v1/sprites/a%2F..%2Fb "), "{line}");
    }

    #[test]
    fn inspect_errors_name_the_sprite_never_the_token() {
        let (base, _) = fake("500 Internal Server Error", "boom");
        let p = crate::provider::SpritesProvider::new(&base, "tok-secret", "");
        let text = format!("{:#}", block_on(p.inspect("sx-a")).unwrap_err());
        assert!(text.contains("sx-a"), "{text}");
        assert!(!text.contains("tok-secret"), "{text}");
        let (base, _) = fake("200 OK", r#"{"name":"sx-other","status":"warm"}"#);
        let p = crate::provider::SpritesProvider::new(&base, "tok-secret", "");
        assert!(block_on(p.inspect("sx-a")).is_err(), "a mismatched name");
        let p = crate::provider::SpritesProvider::new(&base, "tok-secret", "");
        assert!(block_on(p.inspect("")).is_err());
    }
}
