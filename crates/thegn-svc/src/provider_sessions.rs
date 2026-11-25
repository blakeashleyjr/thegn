//! Authoritative native-session absence. Transport/status/decoding failures are
//! never an empty roster. The Sprites list contract is documented at
//! <https://sprites.dev/api/sprites/exec> (GET /v1/sprites/{name}/exec).

use super::{
    CONTROL_TIMEOUT, ExecControl, ExecSpec, ExecStderr, Provider, SpritesProvider, StderrTail,
    StreamingExec,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the Sprites session roster says about one exec session (THE-2052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecSessionState {
    /// Listed and still running.
    Running,
    /// Listed and no longer running; the exit code when the roster carries one.
    Finished { exit_code: Option<i32> },
    /// Not in the roster: finished and reaped, or the sprite lost it.
    Absent,
}

/// How long a detached launch waits for the server to announce the session id.
const DETACH_ANNOUNCE_WAIT: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
#[serde(untagged)]
enum SessionId {
    Text(String),
    Number(u64),
}

#[derive(Deserialize)]
struct SessionIdentity {
    id: SessionId,
    #[serde(default)]
    is_active: Option<bool>,
    #[serde(default)]
    exit_code: Option<i32>,
}

fn session_text(id: SessionId) -> Result<String> {
    match id {
        SessionId::Text(id) if !id.is_empty() => Ok(id),
        SessionId::Number(id) => Ok(id.to_string()),
        SessionId::Text(_) => bail!("sprites: empty identifier in session roster"),
    }
}

/// The state of `session` in a decoded roster body. A listed session with no
/// `is_active` field counts as running: the roster only lists live sessions
/// unless it says otherwise.
fn roster_state(body: &[u8], session: &str) -> Result<ExecSessionState> {
    if session.is_empty() {
        bail!("cannot query an empty session identifier");
    }
    let roster: Vec<SessionIdentity> =
        serde_json::from_slice(body).context("sprites: invalid session roster")?;
    let mut state = ExecSessionState::Absent;
    for item in roster {
        let (active, exit_code) = (item.is_active, item.exit_code);
        if session_text(item.id)? == session {
            state = if active == Some(false) {
                ExecSessionState::Finished { exit_code }
            } else {
                ExecSessionState::Running
            };
        }
    }
    Ok(state)
}

fn roster_absent(body: &[u8], session: &str) -> Result<bool> {
    if session.is_empty() {
        bail!("cannot query an empty session identifier");
    }
    let roster: Vec<SessionIdentity> =
        serde_json::from_slice(body).context("sprites: invalid session roster")?;
    let mut found = false;
    for item in roster {
        let id = match item.id {
            SessionId::Text(id) if !id.is_empty() => id,
            SessionId::Number(id) => id.to_string(),
            _ => bail!("sprites: empty identifier in session roster"),
        };
        found |= id == session;
    }
    Ok(!found)
}

impl SpritesProvider {
    /// POST /v1/sprites/{id}/exec/{session}/kill?signal=SIGHUP&timeout=5s
    /// (OpenAPI "Kill Exec Session").
    ///
    /// A missing session is already in the requested state, so 404 is treated
    /// as success. The NDJSON response body is drained without exposing its
    /// contents and is capped at 64 KiB to prevent an unbounded control-plane
    /// response.
    pub async fn kill_exec_session(&self, id: &str, session: &str) -> Result<()> {
        let id = super::checked_sprite_name(id)?;
        let session = super::checked_exec_session(session)?;
        let mut url = reqwest::Url::parse(&self.api_base).context("sprites: bad API URL")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("sprites: API URL cannot hold a path"))?
            .pop_if_empty()
            .push("sprites")
            .push(id)
            .push("exec")
            .push(session)
            .push("kill");
        url.query_pairs_mut().append_pair("signal", "SIGHUP");

        let mut response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .context("sprites: kill exec session")?;
        const MAX_KILL_BODY_BYTES: usize = 64 * 1024;
        let mut body_len = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .context("sprites: read kill exec response")?
        {
            if chunk.len() > MAX_KILL_BODY_BYTES.saturating_sub(body_len) {
                bail!("sprites: kill exec response exceeds size limit");
            }
            body_len += chunk.len();
        }

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !status.is_success() {
            bail!("sprites: kill exec session request failed ({status})");
        }
        Ok(())
    }

    pub async fn exec_session_absent(&self, id: &str, session: &str) -> Result<bool> {
        let bytes = self.session_roster(id).await?;
        roster_absent(&bytes, session)
    }

    /// The session's state from the authoritative roster (`--status`).
    pub async fn exec_session_state(&self, id: &str, session: &str) -> Result<ExecSessionState> {
        let bytes = self.session_roster(id).await?;
        roster_state(&bytes, session)
    }

    /// Start `spec` as a server-side exec session that outlives this client and
    /// return its id (THE-2052). The command is the session's own process, not a
    /// child of a client-bound shell, so closing the socket here leaves it
    /// running (Sprites exec sessions are detachable); `max_run_after_disconnect`
    /// asks the server to keep it for `max_run` after the last client leaves. The
    /// upgrade is attempted exactly once: a launch must never run twice.
    pub async fn open_exec_detached(
        &self,
        id: &str,
        spec: &ExecSpec,
        max_run: Duration,
    ) -> Result<String> {
        anyhow::ensure!(!spec.tty, "sprites: a detached exec is non-tty");
        let url = detached_ws_url(&super::exec_ws_url(self.base()?, id, spec)?, max_run)?;
        let mut session = Box::pin(self.connect_exec(url, false, false, None, None)).await?;
        let until = tokio::time::Instant::now() + DETACH_ANNOUNCE_WAIT;
        let announced = loop {
            let current = session.session_id.borrow().clone();
            if let Some(handle) = current {
                break Some(handle);
            }
            match tokio::time::timeout_at(until, session.session_id.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break None,
            }
        };
        // Close only our end; the session keeps running server-side.
        if let Err(error) = session.control.send(ExecControl::Close).await {
            tracing::debug!(target: "thegn::provider", %error, "detached exec socket already closed");
        }
        let Some(handle) = announced else {
            bail!(
                "sprites: the server announced no session id; the command may or may not be \
                 running (check `thegn sprite-exec {id} --status`)"
            );
        };
        super::checked_exec_session(&handle)?;
        Ok(handle)
    }

    async fn session_roster(&self, id: &str) -> Result<Vec<u8>> {
        let id = super::checked_sprite_name(id)?;
        let mut url = reqwest::Url::parse(&self.api_base).context("sprites: bad API URL")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("sprites: API URL cannot hold a path"))?
            .pop_if_empty()
            .push("sprites")
            .push(id)
            .push("exec");
        let mut response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .timeout(CONTROL_TIMEOUT)
            .send()
            .await
            .context("sprites: query session roster")?;
        if response.status() != reqwest::StatusCode::OK {
            bail!(
                "sprites: session roster request failed ({})",
                response.status()
            );
        }
        const MAX_ROSTER_BYTES: usize = 1024 * 1024;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("sprites: read session roster")?
        {
            if chunk.len() > MAX_ROSTER_BYTES.saturating_sub(bytes.len()) {
                bail!("sprites: session roster exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// `exec_url` plus the `max_run_after_disconnect` the server applies once the
/// last client has gone.
fn detached_ws_url(exec_url: &str, max_run: Duration) -> Result<String> {
    let mut url = reqwest::Url::parse(exec_url).context("sprites: bad exec url")?;
    url.query_pairs_mut().append_pair(
        "max_run_after_disconnect",
        &format!("{}s", max_run.as_secs().max(1)),
    );
    Ok(url.to_string())
}

impl Provider {
    /// Kill a persisted provider exec session when the provider owns one.
    /// Providers without a persistent server-side exec session to kill return
    /// successfully without making a request.
    pub async fn kill_exec_session(&self, id: &str, session: &str) -> Result<()> {
        match self {
            Provider::Sprites(provider) => provider.kill_exec_session(id, session).await,
            _ => Ok(()),
        }
    }

    /// The session's roster state; `None` for providers without native sessions.
    pub async fn exec_session_state(
        &self,
        id: &str,
        session: &str,
    ) -> Result<Option<ExecSessionState>> {
        match self {
            Provider::Sprites(provider) => provider.exec_session_state(id, session).await.map(Some),
            _ => Ok(None),
        }
    }

    /// Start a command that outlives this client; `None` for providers whose
    /// exec sessions are not detachable.
    pub async fn open_exec_detached(
        &self,
        id: &str,
        spec: &ExecSpec,
        max_run: Duration,
    ) -> Result<Option<String>> {
        match self {
            Provider::Sprites(provider) => provider
                .open_exec_detached(id, spec, max_run)
                .await
                .map(Some),
            _ => Ok(None),
        }
    }

    /// Attach to a session started by [`open_exec_detached`](Self::open_exec_detached)
    /// with a fresh stderr tail, resuming output after `output_offset` bytes
    /// (`--attach`). Sprites only.
    pub async fn attach_exec_fresh(
        &self,
        id: &str,
        session: &str,
        output_offset: u64,
    ) -> Result<StreamingExec> {
        match self {
            Provider::Sprites(provider) => {
                let stderr = ExecStderr(Arc::new(Mutex::new(StderrTail::default())));
                Box::pin(provider.attach_exec_streaming(id, session, output_offset, &stderr)).await
            }
            _ => bail!("provider '{}' has no streaming exec", self.name()),
        }
    }

    /// Only an authoritative, successfully decoded roster may prove absence.
    /// Providers without persistent native session identities stay conservative.
    pub async fn exec_session_absent(&self, id: &str, session: &str) -> Result<bool> {
        match self {
            Provider::Sprites(provider) => provider.exec_session_absent(id, session).await,
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_state_reads_running_finished_and_absent() {
        let body = br#"[{"id":12,"is_active":true},{"id":"13","is_active":false,"exit_code":3},{"id":"14","is_active":false},{"id":"15"}]"#;
        assert_eq!(roster_state(body, "12").unwrap(), ExecSessionState::Running);
        assert_eq!(
            roster_state(body, "13").unwrap(),
            ExecSessionState::Finished { exit_code: Some(3) }
        );
        assert_eq!(
            roster_state(body, "14").unwrap(),
            ExecSessionState::Finished { exit_code: None }
        );
        assert_eq!(roster_state(body, "15").unwrap(), ExecSessionState::Running);
        assert_eq!(roster_state(body, "99").unwrap(), ExecSessionState::Absent);
        assert!(roster_state(body, "").is_err());
        assert!(roster_state(b"nope", "12").is_err());
    }

    #[test]
    fn detached_url_asks_the_server_to_keep_the_session() {
        let url = detached_ws_url(
            "wss://h/v1/sprites/a/exec?tty=false",
            Duration::from_secs(7200),
        )
        .unwrap();
        assert_eq!(
            url,
            "wss://h/v1/sprites/a/exec?tty=false&max_run_after_disconnect=7200s"
        );
    }

    #[test]
    fn roster_requires_complete_valid_identity_list() {
        assert!(roster_absent(b"[]", "12").unwrap());
        assert!(!roster_absent(br#"[{"id":12}]"#, "12").unwrap());
        assert!(!roster_absent(br#"[{"id":"12","is_active":false}]"#, "12").unwrap());
        assert!(roster_absent(br#"[{"id":"13"}]"#, "12").unwrap());
        for malformed in [
            b"null".as_slice(),
            b"{}",
            b"[{\"command\":\"sh\"}]",
            b"[{\"id\":null}]",
            b"[{\"id\":\"\"}]",
            b"[{\"id\":-1}]",
            b"[{\"id\":1.5}]",
            b"[{\"id\":\"12\"},{}]",
            b"[{\"id\":12}]x",
        ] {
            assert!(roster_absent(malformed, "12").is_err());
        }
    }

    #[tokio::test]
    async fn roster_http_errors_never_authorize_fresh_exec() {
        use axum::http::StatusCode;
        use axum::{Router, routing::get};
        for (status, body, expected) in [
            (StatusCode::OK, "[]", Some(true)),
            (StatusCode::OK, r#"[{"id":"live"}]"#, Some(false)),
            (StatusCode::OK, r#"{"error":"bad"}"#, None),
            (StatusCode::UNAUTHORIZED, "[]", None),
            (StatusCode::FORBIDDEN, "[]", None),
            (StatusCode::NOT_FOUND, "[]", None),
            (StatusCode::INTERNAL_SERVER_ERROR, "[]", None),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new().route(
                        "/v1/sprites/test/exec",
                        get(move |headers: axum::http::HeaderMap| async move {
                            assert_eq!(headers["authorization"], "Bearer test-token");
                            (status, body)
                        }),
                    ),
                )
                .await
                .unwrap();
            });
            let provider =
                SpritesProvider::new(&format!("http://{address}/v1"), "test-token", "test");
            let result = provider.exec_session_absent("test", "live").await;
            assert_eq!(result.ok(), expected, "status {status}");
            server.abort();
        }
    }

    #[tokio::test]
    async fn kill_exec_session_handles_statuses_and_request_shape() {
        use axum::Router;
        use axum::extract::Request;
        use axum::routing::any;
        use std::sync::{Arc, Mutex};

        for (status, expected_ok) in [
            (axum::http::StatusCode::OK, true),
            (axum::http::StatusCode::NOT_FOUND, true),
            (axum::http::StatusCode::UNAUTHORIZED, false),
            (axum::http::StatusCode::INTERNAL_SERVER_ERROR, false),
        ] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&requests);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(
                    listener,
                    Router::new().fallback(any(move |request: Request| {
                        let seen = Arc::clone(&seen);
                        async move {
                            seen.lock().unwrap().push((
                                request.method().clone(),
                                request.uri().clone(),
                                request
                                    .headers()
                                    .get("authorization")
                                    .and_then(|value| value.to_str().ok())
                                    .map(str::to_owned),
                            ));
                            (status, "ignored response body")
                        }
                    })),
                )
                .await
                .unwrap();
            });

            let provider =
                SpritesProvider::new(&format!("http://{address}/v1"), "test-token", "test");
            let result = provider.kill_exec_session("test", "live").await;
            assert_eq!(result.is_ok(), expected_ok, "status {status}");

            let requests = requests.lock().unwrap();
            let (method, uri, authorization) = requests.first().expect("kill request recorded");
            assert_eq!(method, axum::http::Method::POST);
            assert_eq!(uri.path(), "/v1/sprites/test/exec/live/kill");
            let query = reqwest::Url::parse(&format!("http://localhost{uri}"))
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect::<Vec<_>>();
            assert!(query.contains(&("signal".into(), "SIGHUP".into())));
            assert_eq!(authorization.as_deref(), Some("Bearer test-token"));
            drop(requests);
            server.abort();
        }
    }

    #[tokio::test]
    async fn kill_exec_session_rejects_multi_segment_session_before_request() {
        use axum::Router;
        use axum::routing::any;
        use std::sync::{Arc, atomic::AtomicUsize};

        let requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&requests);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(any(move || {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async { "unexpected request" }
                })),
            )
            .await
            .unwrap();
        });

        let provider = SpritesProvider::new(&format!("http://{address}/v1"), "test-token", "test");
        assert!(provider.kill_exec_session("test", "a/b").await.is_err());
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
        server.abort();
    }
}
