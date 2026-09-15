//! Shared bounded HTTP policy for authenticated issue providers.
//!
//! A logical provider operation owns one semaphore permit and one absolute
//! deadline.  Provider helpers pass the same operation through every request
//! in a mutation or expansion, so a second request cannot reset either limit.

use super::IssueError;
use futures_util::StreamExt;
use reqwest::{Client, Method, RequestBuilder, Response, Url};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, timeout_at};

pub(crate) const MAX_IN_FLIGHT: usize = 8;
pub(crate) const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MAX_BODY_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_REQUEST_BYTES: usize = 512 * 1024;
pub(crate) const MAX_DYNAMIC_INPUT_BYTES: usize = 64 * 1024;

pub(crate) fn ensure_dynamic_input(value: &str) -> Result<(), IssueError> {
    if value.len() > MAX_DYNAMIC_INPUT_BYTES {
        Err(IssueError::BodyLimit("tracker input exceeds limit"))
    } else {
        Ok(())
    }
}

/// Process-wide ownership of tracker HTTP concurrency.  Routers are rebuilt by
/// CLI, daemon, and background hydration paths, so a router-local semaphore
/// would fail to cap the process as a whole.
#[derive(Clone)]
pub(crate) struct TrackerHttpBudget {
    semaphore: Arc<Semaphore>,
}

impl TrackerHttpBudget {
    pub(crate) fn process() -> Arc<Self> {
        static PROCESS: OnceLock<Arc<TrackerHttpBudget>> = OnceLock::new();
        PROCESS
            .get_or_init(|| Arc::new(Self::with_permits(MAX_IN_FLIGHT)))
            .clone()
    }

    pub(crate) fn with_permits(permits: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(permits)),
        }
    }
}

/// Account-local client policy.  The budget is shared, while origin and
/// authorization remain private to this account.
pub(crate) struct TrackerHttpClient {
    client: Client,
    origin: Url,
    authorization: String,
    provider: &'static str,
    budget: Arc<TrackerHttpBudget>,
}

impl TrackerHttpClient {
    pub(crate) fn new(
        provider: &'static str,
        origin: &str,
        authorization: String,
        budget: Arc<TrackerHttpBudget>,
    ) -> Result<Self, IssueError> {
        let origin = parse_origin(origin)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_IDLE_TIMEOUT)
            .timeout(OPERATION_TIMEOUT)
            .build()
            .map_err(|_| IssueError::Policy("tracker HTTP client configuration failed"))?;
        Ok(Self {
            client,
            origin,
            authorization,
            provider,
            budget,
        })
    }

    pub(crate) fn operation(&self) -> TrackerHttpOperation<'_> {
        self.operation_with_timeout(OPERATION_TIMEOUT)
    }

    #[cfg(test)]
    pub(crate) fn operation_with_timeout(&self, timeout: Duration) -> TrackerHttpOperation<'_> {
        TrackerHttpOperation {
            client: self,
            deadline: Instant::now() + timeout,
            permit: None,
        }
    }

    fn url(&self, path: &str) -> Result<Url, IssueError> {
        if path.starts_with("//") || path.contains("\\") || path.starts_with("http") {
            return Err(IssueError::Policy("tracker request path is not relative"));
        }
        if path.len() > MAX_REQUEST_BYTES {
            return Err(IssueError::BodyLimit("tracker request path exceeds limit"));
        }
        let path = path.trim_start_matches('/');
        let url = self
            .origin
            .join(path)
            .map_err(|_| IssueError::Policy("tracker request path is invalid"))?;
        if url.scheme() != self.origin.scheme()
            || url.host_str() != self.origin.host_str()
            || url.port_or_known_default() != self.origin.port_or_known_default()
            || !url.path().starts_with(self.origin.path())
        {
            return Err(IssueError::Policy("tracker request escaped account origin"));
        }
        Ok(url)
    }
}

pub(crate) struct TrackerHttpOperation<'a> {
    client: &'a TrackerHttpClient,
    deadline: Instant,
    permit: Option<OwnedSemaphorePermit>,
}

impl TrackerHttpOperation<'_> {
    fn remaining(&self) -> Result<Duration, IssueError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .ok_or(IssueError::Timeout("operation deadline exceeded"))
    }

    async fn acquire(&mut self) -> Result<(), IssueError> {
        if self.permit.is_some() {
            return Ok(());
        }
        let _ = self.remaining()?;
        let acquire = self.client.budget.semaphore.clone().acquire_owned();
        self.permit = Some(
            timeout_at(self.deadline, acquire)
                .await
                .map_err(|_| IssueError::Timeout("waiting for tracker HTTP capacity"))?
                .map_err(|_| IssueError::Policy("tracker HTTP capacity is closed"))?,
        );
        Ok(())
    }

    /// Acquire the operation's sole permit and check its absolute deadline.
    /// Callers use this before constructing dynamic query strings or bodies;
    /// request helpers call it again harmlessly through `send`.
    pub(crate) async fn prepare(&mut self) -> Result<(), IssueError> {
        self.acquire().await?;
        self.remaining()?;
        Ok(())
    }

    async fn send(&mut self, request: RequestBuilder) -> Result<Response, IssueError> {
        self.acquire().await?;
        let _ = self.remaining()?;
        timeout_at(self.deadline, request.send())
            .await
            .map_err(|_| IssueError::Timeout("tracker HTTP operation deadline exceeded"))?
            .map_err(IssueError::Network)
    }

    pub(crate) async fn json<B, R>(
        &mut self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<R, IssueError>
    where
        B: Serialize,
        R: DeserializeOwned,
    {
        self.prepare().await?;
        let bytes = serialize_bounded(body)?;
        self.remaining()?;
        let url = self.client.url(path)?;
        let request = self
            .client
            .client
            .request(method, url)
            .header("Authorization", &self.client.authorization)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .body(bytes);
        let response = self.send(request).await?;
        self.decode_json(response).await
    }

    pub(crate) async fn get<R>(&mut self, path: &str) -> Result<R, IssueError>
    where
        R: DeserializeOwned,
    {
        self.prepare().await?;
        let url = self.client.url(path)?;
        let request = self
            .client
            .client
            .get(url)
            .header("Authorization", &self.client.authorization)
            .header("Accept", "application/json");
        let response = self.send(request).await?;
        self.decode_json(response).await
    }

    pub(crate) async fn json_empty<B: Serialize>(
        &mut self,
        method: Method,
        path: &str,
        body: &B,
    ) -> Result<(), IssueError> {
        self.prepare().await?;
        let bytes = serialize_bounded(body)?;
        self.remaining()?;
        let url = self.client.url(path)?;
        let request = self
            .client
            .client
            .request(method, url)
            .header("Authorization", &self.client.authorization)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .body(bytes);
        let response = self.send(request).await?;
        self.check_response(&response, false)?;
        let _ = read_bounded(self.deadline, response).await?;
        Ok(())
    }

    pub(crate) async fn empty(&mut self, method: Method, path: &str) -> Result<(), IssueError> {
        self.prepare().await?;
        let url = self.client.url(path)?;
        let request = self
            .client
            .client
            .request(method, url)
            .header("Authorization", &self.client.authorization)
            .header("Accept", "application/json");
        let response = self.send(request).await?;
        self.check_response(&response, false)?;
        let _ = read_bounded(self.deadline, response).await?;
        Ok(())
    }

    async fn decode_json<R: DeserializeOwned>(
        &mut self,
        response: Response,
    ) -> Result<R, IssueError> {
        self.check_response(&response, true)?;
        let bytes = read_bounded(self.deadline, response).await?;
        let _ = self.remaining()?;
        let value = serde_json::from_slice(&bytes)
            .map_err(|_| IssueError::Parse("tracker JSON decode failed".into()))?;
        let _ = self.remaining()?;
        Ok(value)
    }

    fn check_response(&self, response: &Response, json: bool) -> Result<(), IssueError> {
        let status = response.status();
        if status.is_redirection() {
            return Err(IssueError::Policy("tracker redirect refused"));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(IssueError::Auth(format!(
                "{} HTTP {}",
                self.client.provider, status
            )));
        }
        if !status.is_success() {
            return Err(IssueError::Api(format!(
                "{} HTTP {}",
                self.client.provider, status
            )));
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(IssueError::Policy("tracker response encoding refused"));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BODY_BYTES as u64)
        {
            return Err(IssueError::BodyLimit("tracker response exceeds limit"));
        }
        if json {
            let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE) else {
                return Err(IssueError::Policy("tracker JSON content type missing"));
            };
            let content_type = content_type
                .to_str()
                .map_err(|_| IssueError::Policy("tracker JSON content type invalid"))?;
            let media = content_type.split(';').next().unwrap_or("").trim();
            if media != "application/json" && !media.ends_with("+json") {
                return Err(IssueError::Policy("tracker JSON content type refused"));
            }
        }
        Ok(())
    }
}

async fn read_bounded(deadline: Instant, response: Response) -> Result<Vec<u8>, IssueError> {
    let length = response.content_length().unwrap_or(0) as usize;
    let mut body = Vec::with_capacity(length.min(MAX_BODY_BYTES));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = timeout_at(deadline, stream.next())
        .await
        .map_err(|_| IssueError::Timeout("reading tracker response"))?
    {
        let chunk = chunk.map_err(IssueError::Network)?;
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(IssueError::BodyLimit("tracker response exceeds limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

struct BoundedWriter {
    bytes: Vec<u8>,
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(input.len()) > MAX_REQUEST_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "tracker request exceeds limit",
            ));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_bounded<T: Serialize>(value: &T) -> Result<Vec<u8>, IssueError> {
    let mut writer = BoundedWriter { bytes: Vec::new() };
    let mut serializer = serde_json::Serializer::new(&mut writer);
    value
        .serialize(&mut serializer)
        .map_err(|_| IssueError::BodyLimit("tracker request exceeds limit"))?;
    Ok(writer.bytes)
}

fn parse_origin(raw: &str) -> Result<Url, IssueError> {
    let mut origin = Url::parse(raw).map_err(|_| IssueError::Policy("tracker origin invalid"))?;
    if !matches!(origin.scheme(), "http" | "https")
        || origin.host_str().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(IssueError::Policy("tracker origin refused"));
    }
    let path = origin.path().trim_end_matches('/').to_string();
    origin.set_path(if path.is_empty() {
        "/"
    } else {
        &format!("{path}/")
    });
    Ok(origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::any;
    use futures_util::StreamExt;

    #[test]
    fn origin_policy_preserves_explicit_lan_http_and_rejects_authority_state() {
        assert_eq!(
            parse_origin("http://192.0.2.10:8080").unwrap().scheme(),
            "http"
        );
        assert!(parse_origin("http://user:pass@192.0.2.10:8080").is_err());
        assert_eq!(
            parse_origin("https://example.test/path").unwrap().path(),
            "/path/"
        );
        assert!(parse_origin("ftp://example.test").is_err());
    }

    #[test]
    fn constructors_admit_linear_and_preserve_self_hosted_base_paths() {
        let budget = Arc::new(TrackerHttpBudget::with_permits(1));
        let linear = TrackerHttpClient::new(
            "linear",
            "https://api.linear.app",
            "linear-secret".into(),
            Arc::clone(&budget),
        )
        .expect("official Linear endpoint is admitted");
        assert_eq!(linear.url("graphql").unwrap().path(), "/graphql");

        let jira = TrackerHttpClient::new(
            "jira",
            "http://jira.lan:8080/company/jira/",
            "jira-secret".into(),
            budget,
        )
        .expect("self-hosted base path is admitted");
        assert_eq!(
            jira.url("rest/api/3/issue/ABC-1").unwrap().path(),
            "/company/jira/rest/api/3/issue/ABC-1"
        );
        assert!(jira.url("../outside").is_err());
    }

    #[test]
    fn issue_error_debug_redacts_reqwest_network_details() {
        let error = IssueError::Network(
            reqwest::Client::new()
                .get("https://[invalid]/?token=NETWORK_SENTINEL")
                .build()
                .err()
                .expect("invalid request URL should produce a reqwest error"),
        );
        let debug = format!("{error:?}");
        assert!(!debug.contains("NETWORK_SENTINEL"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn bounded_serializer_refuses_before_a_request_can_be_built() {
        let oversized = "x".repeat(MAX_REQUEST_BYTES + 1);
        assert!(matches!(
            serialize_bounded(&oversized),
            Err(IssueError::BodyLimit(_))
        ));
    }

    async fn fixture(request: Request) -> Response {
        let mut response = match request.uri().path() {
            "/ok" => (StatusCode::OK, Body::from(r#"{"ok":true}"#)).into_response(),
            "/redirect" => (StatusCode::FOUND, Body::empty()).into_response(),
            "/encoding" => (StatusCode::OK, Body::from(r#"{"ok":true}"#)).into_response(),
            "/bad-mime" => (StatusCode::OK, Body::from(r#"{"ok":true}"#)).into_response(),
            "/unauthorized" => (StatusCode::UNAUTHORIZED, Body::empty()).into_response(),
            "/large" => {
                (StatusCode::OK, Body::from(vec![b'x'; MAX_BODY_BYTES + 1])).into_response()
            }
            "/chunked" => {
                let chunks = futures_util::stream::iter(
                    (0..=(MAX_BODY_BYTES / 1024))
                        .map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024])),
                );
                (StatusCode::OK, Body::from_stream(chunks)).into_response()
            }
            _ => (StatusCode::NOT_FOUND, Body::empty()).into_response(),
        };
        match request.uri().path() {
            "/ok" | "/encoding" | "/bad-mime" | "/large" | "/chunked" => {
                response.headers_mut().insert(
                    reqwest::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
            }
            "/redirect" => {
                response
                    .headers_mut()
                    .insert(reqwest::header::LOCATION, HeaderValue::from_static("/ok"));
            }
            _ => {}
        }
        if request.uri().path() == "/encoding" {
            response.headers_mut().insert(
                reqwest::header::CONTENT_ENCODING,
                HeaderValue::from_static("gzip"),
            );
        }
        if request.uri().path() == "/bad-mime" {
            response.headers_mut().insert(
                reqwest::header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain"),
            );
        }
        response
    }

    #[tokio::test]
    async fn fake_http_enforces_redirect_encoding_and_stream_caps() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(any(fixture)))
                .await
                .unwrap();
        });
        let budget = Arc::new(TrackerHttpBudget::with_permits(8));
        let client = TrackerHttpClient::new(
            "fixture",
            &format!("http://{address}"),
            "Bearer fixture-secret".into(),
            budget,
        )
        .unwrap();

        let mut ok = client.operation();
        let value: serde_json::Value = ok.get("/ok").await.unwrap();
        assert_eq!(value["ok"], true);

        let mut redirect = client.operation();
        assert!(matches!(
            redirect.get::<serde_json::Value>("/redirect").await,
            Err(IssueError::Policy("tracker redirect refused"))
        ));

        let mut encoding = client.operation();
        assert!(matches!(
            encoding.get::<serde_json::Value>("/encoding").await,
            Err(IssueError::Policy("tracker response encoding refused"))
        ));

        let mut mime = client.operation();
        assert!(matches!(
            mime.get::<serde_json::Value>("/bad-mime").await,
            Err(IssueError::Policy("tracker JSON content type refused"))
        ));

        let mut auth = client.operation();
        assert!(matches!(
            auth.get::<serde_json::Value>("/unauthorized").await,
            Err(IssueError::Auth(_))
        ));

        let mut large = client.operation();
        assert!(matches!(
            large.get::<serde_json::Value>("/large").await,
            Err(IssueError::BodyLimit("tracker response exceeds limit"))
        ));
        let mut chunked = client.operation();
        assert!(matches!(
            chunked.get::<serde_json::Value>("/chunked").await,
            Err(IssueError::BodyLimit("tracker response exceeds limit"))
        ));
        server.abort();
    }

    #[tokio::test]
    async fn redirect_target_receives_no_request_or_credentials() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let target_hits = Arc::new(AtomicUsize::new(0));
        let observed_hits = Arc::clone(&target_hits);
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(any(move |request: Request| {
                let observed_hits = Arc::clone(&observed_hits);
                async move {
                    if request.uri().path() == "/target" {
                        observed_hits.fetch_add(1, Ordering::SeqCst);
                    }
                    let mut response = (StatusCode::FOUND, Body::empty()).into_response();
                    response.headers_mut().insert(
                        reqwest::header::LOCATION,
                        HeaderValue::from_static("/target"),
                    );
                    response
                }
            }));
            axum::serve(listener, app).await.unwrap();
        });
        let client = TrackerHttpClient::new(
            "fixture",
            &format!("http://{address}"),
            "Bearer redirect-secret".into(),
            Arc::new(TrackerHttpBudget::with_permits(1)),
        )
        .unwrap();
        let mut operation = client.operation();
        assert!(matches!(
            operation.get::<serde_json::Value>("/redirect").await,
            Err(IssueError::Policy("tracker redirect refused"))
        ));
        assert_eq!(target_hits.load(Ordering::SeqCst), 0);
        server.abort();
    }

    #[tokio::test]
    async fn shared_budget_queue_cancellation_releases_capacity() {
        let budget = Arc::new(TrackerHttpBudget::with_permits(8));
        let client = TrackerHttpClient::new(
            "fixture",
            "http://127.0.0.1:1",
            "fixture-secret".into(),
            Arc::clone(&budget),
        )
        .unwrap();
        let second_client = TrackerHttpClient::new(
            "fixture",
            "http://127.0.0.1:2",
            "fixture-secret".into(),
            Arc::clone(&budget),
        )
        .unwrap();
        let mut held = Vec::new();
        for _ in 0..8 {
            let mut operation = client.operation_with_timeout(Duration::from_secs(1));
            operation.prepare().await.unwrap();
            held.push(operation);
        }
        let mut queued = second_client.operation_with_timeout(Duration::from_millis(20));
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(40), queued.prepare()).await,
            Ok(Err(IssueError::Timeout(
                "waiting for tracker HTTP capacity"
            )))
        ));
        drop(queued);
        let oversized = "x".repeat(MAX_REQUEST_BYTES + 1);
        let mut queued_body = second_client.operation_with_timeout(Duration::from_millis(20));
        let result = tokio::time::timeout(
            Duration::from_millis(40),
            queued_body.json::<_, serde_json::Value>(reqwest::Method::POST, "/queued", &oversized),
        )
        .await
        .expect("queued request operation returns");
        assert!(matches!(result, Err(IssueError::Timeout(_))));
        drop(queued_body);
        drop(held);
        let mut next = client.operation_with_timeout(Duration::from_millis(100));
        next.prepare().await.expect("released global capacity");
    }

    fn slow_response() -> Response {
        let first =
            futures_util::stream::once(async { Ok::<_, std::io::Error>(b"{\"ok\":true".to_vec()) });
        let never = futures_util::stream::pending::<Result<Vec<u8>, std::io::Error>>();
        let mut response = (StatusCode::OK, Body::from_stream(first.chain(never))).into_response();
        response.headers_mut().insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        response
    }

    #[tokio::test]
    async fn slow_stream_timeout_drops_body_and_releases_permit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/slow", any(|_: Request| async { slow_response() })),
            )
            .await
            .unwrap();
        });
        let client = TrackerHttpClient::new(
            "fixture",
            &format!("http://{address}"),
            "fixture-secret".into(),
            Arc::new(TrackerHttpBudget::with_permits(1)),
        )
        .unwrap();
        let mut operation = client.operation_with_timeout(Duration::from_millis(40));
        assert!(matches!(
            operation.get::<serde_json::Value>("/slow").await,
            Err(IssueError::Timeout("reading tracker response"))
        ));
        drop(operation);
        let mut released = client.operation_with_timeout(Duration::from_millis(100));
        released.prepare().await.expect("slow body released permit");
        server.abort();
    }

    #[tokio::test]
    async fn multirequest_operation_keeps_one_absolute_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let route_calls = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(any(move |_: Request| {
                let route_calls = Arc::clone(&route_calls);
                async move {
                    if route_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        let mut response =
                            (StatusCode::OK, Body::from(r#"{"ok":true}"#)).into_response();
                        response.headers_mut().insert(
                            reqwest::header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        );
                        response
                    } else {
                        slow_response()
                    }
                }
            }));
            axum::serve(listener, app).await.unwrap();
        });
        let client = TrackerHttpClient::new(
            "fixture",
            &format!("http://{address}"),
            "fixture-secret".into(),
            Arc::new(TrackerHttpBudget::with_permits(1)),
        )
        .unwrap();
        let mut operation = client.operation_with_timeout(Duration::from_millis(60));
        let first: serde_json::Value = operation.get("/sequence").await.unwrap();
        assert_eq!(first["ok"], true);
        assert!(matches!(
            operation.get::<serde_json::Value>("/sequence").await,
            Err(IssueError::Timeout("reading tracker response"))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }
}
