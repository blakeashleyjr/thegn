//! Shared bounded HTTP policy for authenticated issue providers.
//!
//! A logical provider operation owns one semaphore permit and one absolute
//! deadline.  Provider helpers pass the same operation through every request
//! in a mutation or expansion, so a second request cannot reset either limit.

use super::IssueError;
use crate::http::{BodyReadError, read_bounded_response};
#[cfg(test)]
use futures_util::StreamExt;
use reqwest::{Client, Method, RequestBuilder, Response, Url};
use serde::{Serialize, de::DeserializeOwned};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
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
    #[cfg(test)]
    expire_after_responses: Arc<AtomicUsize>,
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
            #[cfg(test)]
            expire_after_responses: Arc::new(AtomicUsize::new(usize::MAX)),
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_after_responses_for_test(&self, requests: usize) {
        self.expire_after_responses
            .store(requests, Ordering::Release);
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
        TrackerHttpOperation {
            client: self,
            deadline: Instant::now() + OPERATION_TIMEOUT,
            permit: None,
        }
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
    #[cfg(test)]
    fn expire_for_test(&mut self) {
        self.deadline = Instant::now() - Duration::from_nanos(1);
    }

    /// Simulate time exhaustion after a successfully consumed response. Only
    /// this operation expires: a mutant that creates a fresh operation for the
    /// next provider request will dispatch it and fail the sequence fixture.
    #[cfg(test)]
    fn response_consumed_for_test(&mut self) {
        let previous = self
            .client
            .budget
            .expire_after_responses
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .unwrap_or(0);
        if previous == 1 {
            self.expire_for_test();
        }
    }

    fn remaining(&self) -> Result<Duration, IssueError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .ok_or(IssueError::Timeout("operation deadline exceeded"))
    }

    async fn acquire(&mut self) -> Result<(), IssueError> {
        if self.permit.is_some() {
            return Ok(());
        }
        self.remaining()?;
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
        self.remaining()?;
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
        read_bounded(self.deadline, response).await?;
        self.remaining()?;
        #[cfg(test)]
        self.response_consumed_for_test();
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
        read_bounded(self.deadline, response).await?;
        self.remaining()?;
        #[cfg(test)]
        self.response_consumed_for_test();
        Ok(())
    }

    async fn decode_json<R: DeserializeOwned>(
        &mut self,
        response: Response,
    ) -> Result<R, IssueError> {
        self.check_response(&response, true)?;
        let bytes = read_bounded(self.deadline, response).await?;
        self.remaining()?;
        let value = serde_json::from_slice(&bytes)
            .map_err(|_| IssueError::Parse("tracker JSON decode failed".into()))?;
        self.remaining()?;
        #[cfg(test)]
        self.response_consumed_for_test();
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
    read_bounded_response(response, deadline, MAX_BODY_BYTES)
        .await
        .map_err(|error| match error {
            BodyReadError::Timeout => IssueError::Timeout("reading tracker response"),
            BodyReadError::Network(error) => IssueError::Network(error),
            BodyReadError::Limit => IssueError::BodyLimit("tracker response exceeds limit"),
        })
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
    let mut path = origin.path().trim_end_matches('/').to_owned();
    path.push('/');
    origin.set_path(&path);
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
    fn process_budget_is_shared_across_client_construction() {
        let first = TrackerHttpBudget::process();
        let second = TrackerHttpBudget::process();
        assert!(Arc::ptr_eq(&first, &second));
        let first_client = TrackerHttpClient::new(
            "first",
            "http://first.example",
            "first-secret".into(),
            Arc::clone(&first),
        )
        .unwrap();
        let second_client = TrackerHttpClient::new(
            "second",
            "http://second.example",
            "second-secret".into(),
            second,
        )
        .unwrap();
        assert!(Arc::ptr_eq(&first_client.budget, &second_client.budget));
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
                .expect_err("invalid request URL should produce a reqwest error"),
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
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn redirect_target_receives_no_request_or_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let source_hits = Arc::new(AtomicUsize::new(0));
        let target_hits = Arc::new(AtomicUsize::new(0));
        let authenticated_hits = Arc::new(AtomicUsize::new(0));
        let observed_targets = Arc::clone(&target_hits);
        let target_server = tokio::spawn(async move {
            axum::serve(
                target_listener,
                Router::new().fallback(any(move || {
                    observed_targets.fetch_add(1, Ordering::SeqCst);
                    async { (StatusCode::OK, "unexpected redirect target") }
                })),
            )
            .await
            .unwrap();
        });
        let observed_sources = Arc::clone(&source_hits);
        let observed_targets = Arc::clone(&target_hits);
        let observed_auth = Arc::clone(&authenticated_hits);
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(any(move |request: Request| {
                let observed_sources = Arc::clone(&observed_sources);
                let observed_targets = Arc::clone(&observed_targets);
                let observed_auth = Arc::clone(&observed_auth);
                async move {
                    observed_sources.fetch_add(1, Ordering::SeqCst);
                    if request.headers().get(reqwest::header::AUTHORIZATION)
                        == Some(&HeaderValue::from_static("Bearer redirect-secret"))
                    {
                        observed_auth.fetch_add(1, Ordering::SeqCst);
                    }
                    if request.uri().path() == "/target" {
                        observed_targets.fetch_add(1, Ordering::SeqCst);
                        return (StatusCode::OK, "unexpected redirect target").into_response();
                    }
                    let mut parts = request.uri().path().trim_start_matches('/').split('/');
                    let status =
                        StatusCode::from_u16(parts.next().unwrap().parse().unwrap()).unwrap();
                    let location = match parts.next().unwrap() {
                        "same" => "/target".to_owned(),
                        "cross" => format!("http://{target_address}/target"),
                        "loop" => request.uri().path().to_owned(),
                        _ => panic!("unexpected fixture redirect mode"),
                    };
                    let mut response = (status, Body::empty()).into_response();
                    response.headers_mut().insert(
                        reqwest::header::LOCATION,
                        HeaderValue::from_str(&location).unwrap(),
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
        let mut cases = 0;
        for status in [301, 302, 307, 308] {
            for destination in ["same", "cross", "loop"] {
                for method in [Method::GET, Method::POST] {
                    let path = format!("/{status}/{destination}");
                    let mut operation = client.operation();
                    let result = if method == Method::GET {
                        operation.get::<serde_json::Value>(&path).await
                    } else {
                        operation
                            .json::<_, serde_json::Value>(
                                method.clone(),
                                &path,
                                &serde_json::json!({"mutation": "replay-sentinel"}),
                            )
                            .await
                    };
                    assert!(
                        matches!(result, Err(IssueError::Policy("tracker redirect refused"))),
                        "{status} {destination} {method}: {result:?}"
                    );
                    cases += 1;
                    assert_eq!(source_hits.load(Ordering::SeqCst), cases);
                    assert_eq!(authenticated_hits.load(Ordering::SeqCst), cases);
                    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
                }
            }
        }
        assert_eq!(cases, 24);
        server.abort();
        target_server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        assert!(target_server.await.unwrap_err().is_cancelled());
    }

    struct CountingBody {
        serializations: Arc<AtomicUsize>,
    }

    impl serde::Serialize for CountingBody {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            self.serializations.fetch_add(1, Ordering::SeqCst);
            serializer.serialize_str("queued")
        }
    }

    #[tokio::test]
    async fn queued_request_does_not_serialize_body_before_capacity() {
        use tokio::sync::oneshot;

        let budget = Arc::new(TrackerHttpBudget::with_permits(1));
        let client = Arc::new(
            TrackerHttpClient::new(
                "fixture",
                "http://127.0.0.1:1",
                "fixture-secret".into(),
                Arc::clone(&budget),
            )
            .unwrap(),
        );
        let mut held = client.operation_with_timeout(Duration::from_secs(1));
        held.prepare().await.unwrap();
        let serializations = Arc::new(AtomicUsize::new(0));
        let body = CountingBody {
            serializations: Arc::clone(&serializations),
        };
        let (started, started_rx) = oneshot::channel();
        let queued_client = Arc::clone(&client);
        let queued = tokio::spawn(async move {
            let mut operation = queued_client.operation_with_timeout(Duration::from_secs(10));
            started.send(()).expect("fixture waits for queue admission");
            operation
                .json::<_, serde_json::Value>(reqwest::Method::POST, "/queued", &body)
                .await
        });
        started_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(serializations.load(Ordering::SeqCst), 0);
        queued.abort();
        assert!(queued.await.unwrap_err().is_cancelled());
        drop(held);
        let mut next = client.operation_with_timeout(Duration::from_secs(1));
        next.prepare()
            .await
            .expect("dropped queued request released capacity");
    }

    #[tokio::test]
    async fn shared_budget_queue_future_drop_releases_capacity() {
        use tokio::sync::oneshot;

        let budget = Arc::new(TrackerHttpBudget::with_permits(1));
        let client = Arc::new(
            TrackerHttpClient::new(
                "fixture",
                "http://127.0.0.1:1",
                "fixture-secret".into(),
                Arc::clone(&budget),
            )
            .unwrap(),
        );
        let mut held = client.operation_with_timeout(Duration::from_secs(1));
        held.prepare().await.unwrap();

        let (started, started_rx) = oneshot::channel();
        let queued_client = Arc::clone(&client);
        let queued = tokio::spawn(async move {
            let mut operation = queued_client.operation_with_timeout(Duration::from_secs(10));
            started.send(()).expect("fixture waits for queue admission");
            operation.prepare().await
        });
        started_rx.await.unwrap();
        tokio::task::yield_now().await;
        queued.abort();
        assert!(queued.await.unwrap_err().is_cancelled());

        drop(held);
        let mut next = client.operation_with_timeout(Duration::from_secs(1));
        next.prepare()
            .await
            .expect("dropped queued future released capacity");
    }

    fn slow_response_with_signal(
        signal: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    ) -> Response {
        let first = futures_util::stream::once(async move {
            if let Some(sender) = signal.lock().unwrap().take() {
                sender
                    .send(())
                    .expect("fixture waits for the first body chunk");
            }
            Ok::<_, std::io::Error>(b"{\"ok\":true".to_vec())
        });
        let never = futures_util::stream::pending::<Result<Vec<u8>, std::io::Error>>();
        let mut response = (StatusCode::OK, Body::from_stream(first.chain(never))).into_response();
        response.headers_mut().insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        response
    }

    #[tokio::test]
    async fn pending_stream_future_drop_releases_permit() {
        use tokio::sync::oneshot;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (signal, signal_rx) = oneshot::channel();
        let signal = Arc::new(std::sync::Mutex::new(Some(signal)));
        let route_signal = Arc::clone(&signal);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/slow",
                    any(move |_: Request| {
                        let signal = Arc::clone(&route_signal);
                        async move { slow_response_with_signal(signal) }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let client = Arc::new(
            TrackerHttpClient::new(
                "fixture",
                &format!("http://{address}"),
                "fixture-secret".into(),
                Arc::new(TrackerHttpBudget::with_permits(1)),
            )
            .unwrap(),
        );
        let request_client = Arc::clone(&client);
        let request = tokio::spawn(async move {
            let mut operation = request_client.operation_with_timeout(Duration::from_secs(10));
            operation.get::<serde_json::Value>("/slow").await
        });
        signal_rx.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());

        let mut released = client.operation_with_timeout(Duration::from_secs(1));
        released
            .prepare()
            .await
            .expect("dropped stream released permit");
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn repeated_provider_requests_share_one_explicit_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let route_calls = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            let app = Router::new().fallback(any(move |_: Request| {
                let route_calls = Arc::clone(&route_calls);
                async move {
                    route_calls.fetch_add(1, Ordering::SeqCst);
                    let mut response =
                        (StatusCode::OK, Body::from(r#"{"ok":true}"#)).into_response();
                    response.headers_mut().insert(
                        reqwest::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    response
                }
            }));
            axum::serve(listener, app).await.unwrap();
        });
        let budget = Arc::new(TrackerHttpBudget::with_permits(1));
        let client = TrackerHttpClient::new(
            "fixture",
            &format!("http://{address}"),
            "fixture-secret".into(),
            Arc::clone(&budget),
        )
        .unwrap();
        let mut operation = client.operation_with_timeout(Duration::from_secs(1));
        let first: serde_json::Value = operation.get("/provider/first").await.unwrap();
        assert_eq!(first["ok"], true);
        // Deterministically consume the operation's budget between provider
        // requests. A fresh-deadline-per-request implementation would send a
        // second request; the production operation must refuse before I/O.
        operation.expire_for_test();
        assert!(matches!(
            operation.get::<serde_json::Value>("/provider/second").await,
            Err(IssueError::Timeout("operation deadline exceeded"))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
