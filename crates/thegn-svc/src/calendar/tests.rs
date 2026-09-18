use super::*;
use axum::serve::Listener;
use thegn_core::config_calendar::{CalendarAccount, CalendarConfig, CalendarProviderKind};

struct CountingListener {
    inner: tokio::net::TcpListener,
    accepts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl axum::serve::Listener for CountingListener {
    type Io = tokio::net::TcpStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, address)) => {
                    self.accepts
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    return (stream, address);
                }
                Err(_) => tokio::task::yield_now().await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// A default budget over a private pool, so parallel tests never contend for
/// the process-wide one.
fn adm() -> AccountAdmission {
    AccountAdmission::isolated(AdmissionBudget::default())
}

/// A router over a private pool.
fn router(cfg: &CalendarConfig) -> CalendarRouter {
    CalendarRouter::from_config_with_pool(
        cfg,
        AdmissionPool::new(
            thegn_core::calendar::admission::GLOBAL_MAX_RECORDS,
            thegn_core::calendar::admission::GLOBAL_MAX_BYTES,
        ),
    )
}

fn account(name: &str, provider: CalendarProviderKind) -> CalendarAccount {
    CalendarAccount {
        name: name.into(),
        provider,
        ..Default::default()
    }
}

const ONE_EVENT: &str = "\
BEGIN:VCALENDAR
BEGIN:VEVENT
UID:e1
SUMMARY:Standup
DTSTART;TZID=UTC:20260821T093000
DTEND;TZID=UTC:20260821T094500
END:VEVENT
END:VCALENDAR";

/// A temp dir that cleans up after itself.
struct Tmp(std::path::PathBuf);
impl Tmp {
    fn new(tag: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "thegn-cal-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p); // best-effort: test tmp cleanup
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0); // best-effort: test tmp cleanup
    }
}

/// An ICS backend reading `path` (a file or a vdir).
fn ics_backend(path: &str) -> ics::IcsBackend {
    ics::IcsBackend::new(
        &CalendarAccount {
            path: path.into(),
            ..account("t", CalendarProviderKind::Ics)
        },
        adm(),
    )
}

/// The window every test queries.
fn window() -> (chrono::NaiveDate, chrono::NaiveDate) {
    (
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
    )
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

// --- error classification ---------------------------------------------------

#[test]
fn only_network_failures_are_transient() {
    // Load-bearing: a MISSING .ics file is a configuration mistake. Calling it
    // transient would both hide the error and wrongly mark thegn as offline.
    assert!(CalendarError::Network("timeout".into()).is_transient());
    assert!(!CalendarError::Io("no such path".into()).is_transient());
    assert!(!CalendarError::Auth("401".into()).is_transient());
    assert!(!CalendarError::Parse("bad".into()).is_transient());
    assert!(!CalendarError::NotConfigured.is_transient());
    assert!(!CalendarError::Policy("calendar destination refused").is_transient());
    assert!(!CalendarError::Unsupported("create").is_transient());
}

#[test]
fn dns_policy_and_network_failures_keep_distinct_calendar_classifications() {
    assert!(matches!(
        ics_url::map_transport_error(crate::http::CalendarHttpError::DestinationRefused),
        CalendarError::Policy("calendar destination refused")
    ));
    assert!(matches!(
        caldav::map_transport_error(crate::http::CalendarHttpError::DestinationRefused),
        CalendarError::Policy("calendar destination refused")
    ));
    let network = ics_url::map_transport_error(crate::http::CalendarHttpError::Network);
    assert!(matches!(network, CalendarError::Network(_)));
    assert!(network.is_transient());
}

#[test]
fn errors_render_readably() {
    assert!(
        CalendarError::Unsupported("creating events")
            .to_string()
            .contains("not supported")
    );
    assert!(
        CalendarError::Auth("401".into())
            .to_string()
            .contains("401")
    );
}

#[tokio::test]
async fn every_redirect_status_is_refused_without_following_or_replaying_auth() {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let source_hits = Arc::new(AtomicUsize::new(0));
    let target_hits = Arc::new(AtomicUsize::new(0));
    let auth_hits = Arc::new(AtomicUsize::new(0));
    let source = Arc::clone(&source_hits);
    let target = Arc::clone(&target_hits);
    let auth = Arc::clone(&auth_hits);
    let absolute_target = format!("http://{address}/target?token=redirect-secret");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(move |request: Request| {
                let source = Arc::clone(&source);
                let target = Arc::clone(&target);
                let auth = Arc::clone(&auth);
                async move {
                    if request.uri().path() == "/target" {
                        target.fetch_add(1, Ordering::SeqCst);
                        return (StatusCode::OK, "must not be reached").into_response();
                    }
                    source.fetch_add(1, Ordering::SeqCst);
                    if request.headers().get("authorization")
                        == Some(&HeaderValue::from_static("Bearer feed-secret"))
                    {
                        auth.fetch_add(1, Ordering::SeqCst);
                    }
                    let status = request
                        .uri()
                        .path()
                        .trim_start_matches('/')
                        .parse::<u16>()
                        .unwrap();
                    let location = if status % 2 == 0 {
                        absolute_target.clone()
                    } else {
                        "/target?token=redirect-secret".to_owned()
                    };
                    let mut response =
                        (StatusCode::from_u16(status).unwrap(), Body::empty()).into_response();
                    response
                        .headers_mut()
                        .insert("location", HeaderValue::from_str(&location).unwrap());
                    response
                }
            })),
        )
        .await
        .unwrap();
    });

    let mut last_error = None;
    for status in [301, 302, 303, 307, 308] {
        let backend = ics_url::IcsUrlBackend::new(
            &CalendarAccount {
                url: format!("http://{address}/{status}"),
                token: "feed-secret".into(),
                allow_private_network: true,
                ..account("remote", CalendarProviderKind::IcsUrl)
            },
            adm(),
        );
        let error = backend
            .list_events(window().0, window().1, "sync-secret")
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            CalendarError::Policy("calendar redirect refused")
        ));
        last_error = Some(error);
    }
    assert_eq!(source_hits.load(Ordering::SeqCst), 5);
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
    assert_eq!(auth_hits.load(Ordering::SeqCst), 5);
    assert!(!last_error.unwrap().to_string().contains("redirect-secret"));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn caldav_redirect_never_replays_a_sync_token_body_for_any_status() {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let target_hits = Arc::new(AtomicUsize::new(0));
    let observed_token = Arc::new(AtomicUsize::new(0));
    let target = Arc::clone(&target_hits);
    let token = Arc::clone(&observed_token);
    let absolute_target = format!("http://{address}/target?token=redirect-secret");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(move |request: Request| {
                let target = Arc::clone(&target);
                let token = Arc::clone(&token);
                let absolute_target = absolute_target.clone();
                async move {
                    if request.uri().path() == "/target" {
                        target.fetch_add(1, Ordering::SeqCst);
                        return (StatusCode::OK, "unexpected target").into_response();
                    }
                    let path = request.uri().path().to_owned();
                    let body = to_bytes(request.into_body(), 128 * 1024).await.unwrap();
                    if body
                        .windows(b"sync-secret".len())
                        .any(|w| w == b"sync-secret")
                    {
                        token.fetch_add(1, Ordering::SeqCst);
                    }
                    let status = path
                        .rsplit('/')
                        .next()
                        .and_then(|value| value.parse::<u16>().ok())
                        .unwrap_or(307);
                    let location = if status % 2 == 0 {
                        absolute_target.clone()
                    } else {
                        "/target?token=redirect-secret".to_owned()
                    };
                    let mut response =
                        (StatusCode::from_u16(status).unwrap(), Body::empty()).into_response();
                    response
                        .headers_mut()
                        .insert("location", HeaderValue::from_str(&location).unwrap());
                    response
                }
            })),
        )
        .await
        .unwrap();
    });

    for status in [301, 302, 303, 307, 308] {
        let backend = caldav::CalDavBackend::new(
            &CalendarAccount {
                url: format!("http://{address}/report/{status}"),
                token: "dav-secret".into(),
                allow_private_network: true,
                ..account("dav", CalendarProviderKind::CalDav)
            },
            adm(),
        );
        let error = backend
            .list_events(window().0, window().1, "sync-secret")
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            CalendarError::Policy("calendar redirect refused")
        ));
        assert!(!error.to_string().contains("redirect-secret"));
    }
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
    assert_eq!(observed_token.load(Ordering::SeqCst), 5);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn redirect_locations_are_never_followed_across_origins_or_schemes() {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_address = target_listener.local_addr().unwrap();
    let target_hits = Arc::new(AtomicUsize::new(0));
    let observed_target_hits = Arc::clone(&target_hits);
    let target_server = tokio::spawn(async move {
        axum::serve(
            target_listener,
            Router::new().fallback(any(move |_request: Request| {
                observed_target_hits.fetch_add(1, Ordering::SeqCst);
                async { (StatusCode::OK, "must not be followed") }
            })),
        )
        .await
        .unwrap();
    });

    let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_address = source_listener.local_addr().unwrap();
    let source_hits = Arc::new(AtomicUsize::new(0));
    let observed_source_hits = Arc::clone(&source_hits);
    let target_location = format!("http://{target_address}/cross-origin?token=redirect-secret");
    let server = tokio::spawn(async move {
        axum::serve(
            source_listener,
            Router::new().fallback(any(move |request: Request| {
                let target_location = target_location.clone();
                observed_source_hits.fetch_add(1, Ordering::SeqCst);
                async move {
                    let location = match request.uri().path() {
                        "/301" => "/relative?token=redirect-secret".to_owned(),
                        "/302" => {
                            format!("http://{source_address}/same-origin?token=redirect-secret")
                        }
                        "/303" | "/307" => target_location,
                        // A Location can advertise a confidentiality downgrade;
                        // refusing the original response means it is never opened.
                        "/308" => {
                            "http://downgrade.invalid/calendar?token=redirect-secret".to_owned()
                        }
                        _ => String::new(),
                    };
                    let mut response =
                        (StatusCode::TEMPORARY_REDIRECT, Body::empty()).into_response();
                    *response.status_mut() = match request.uri().path() {
                        "/301" => StatusCode::MOVED_PERMANENTLY,
                        "/302" => StatusCode::FOUND,
                        "/303" => StatusCode::SEE_OTHER,
                        "/307" => StatusCode::TEMPORARY_REDIRECT,
                        _ => StatusCode::PERMANENT_REDIRECT,
                    };
                    response
                        .headers_mut()
                        .insert("location", HeaderValue::from_str(&location).unwrap());
                    response
                }
            })),
        )
        .await
        .unwrap();
    });

    for status in [301, 302, 303, 307, 308] {
        let backend = ics_url::IcsUrlBackend::new(
            &CalendarAccount {
                url: format!("http://{source_address}/{status}"),
                allow_private_network: true,
                ..account("remote", CalendarProviderKind::IcsUrl)
            },
            adm(),
        );
        assert!(matches!(
            backend
                .list_events(window().0, window().1, "sync-secret")
                .await
                .unwrap_err(),
            CalendarError::Policy("calendar redirect refused")
        ));
    }
    assert_eq!(source_hits.load(Ordering::SeqCst), 5);
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
    server.abort();
    target_server.abort();
    let _ = server.await;
    let _ = target_server.await;
}

#[tokio::test]
async fn redirect_self_loops_are_refused_without_a_second_request() {
    use axum::{Router, extract::Request, http::StatusCode, response::IntoResponse, routing::any};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&hits);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(move |_request: Request| {
                observed.fetch_add(1, Ordering::SeqCst);
                async {
                    let mut response = (StatusCode::FOUND, ()).into_response();
                    response
                        .headers_mut()
                        .insert("location", "/loop".parse().unwrap());
                    response
                }
            })),
        )
        .await
        .unwrap();
    });

    for provider in [CalendarProviderKind::IcsUrl, CalendarProviderKind::CalDav] {
        let cfg = CalendarAccount {
            url: format!("http://{address}/loop"),
            allow_private_network: true,
            ..account("loop", provider)
        };
        let error = match provider {
            CalendarProviderKind::IcsUrl => ics_url::IcsUrlBackend::new(&cfg, adm())
                .list_events(window().0, window().1, "")
                .await
                .unwrap_err(),
            CalendarProviderKind::CalDav => caldav::CalDavBackend::new(&cfg, adm())
                .list_events(window().0, window().1, "")
                .await
                .unwrap_err(),
            _ => unreachable!(),
        };
        assert!(matches!(
            error,
            CalendarError::Policy("calendar redirect refused")
        ));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn caldav_token_recovery_is_one_bounded_retry_with_shared_deadline() {
    use axum::Router;
    use axum::body::Body;
    use axum::body::to_bytes;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    for recovery_status in [StatusCode::CONFLICT, StatusCode::INSUFFICIENT_STORAGE] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = Arc::clone(&requests);
        let recorded = Arc::clone(&bodies);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(any(move |request: Request| {
                    let observed = Arc::clone(&observed);
                    let recorded = Arc::clone(&recorded);
                    async move {
                        let body = to_bytes(request.into_body(), 128 * 1024).await.unwrap();
                        recorded
                            .lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&body).into_owned());
                        if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                            return (recovery_status, Body::empty()).into_response();
                        }
                        let mut response = (
                            StatusCode::MULTI_STATUS,
                            Body::from(
                                "<multistatus><sync-token>new-token</sync-token></multistatus>",
                            ),
                        )
                            .into_response();
                        response
                            .headers_mut()
                            .insert("content-type", HeaderValue::from_static("application/xml"));
                        response
                    }
                })),
            )
            .await
            .unwrap();
        });

        let backend = caldav::CalDavBackend::new(
            &CalendarAccount {
                url: format!("http://{address}/report"),
                allow_private_network: true,
                ..account("dav", CalendarProviderKind::CalDav)
            },
            adm(),
        );
        let page = backend
            .list_events(window().0, window().1, "expired-token")
            .await
            .unwrap();
        assert_eq!(page.sync_token(), "new-token");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        {
            let bodies = bodies.lock().unwrap();
            assert!(bodies[0].contains("expired-token"));
            assert!(!bodies[1].contains("expired-token"));
        }
        server.abort();
        let _ = server.await;
    }

    for recovery_status in [StatusCode::CONFLICT, StatusCode::INSUFFICIENT_STORAGE] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(any(move |_request: Request| {
                    let observed = Arc::clone(&observed);
                    async move {
                        let count = observed.fetch_add(1, Ordering::SeqCst);
                        // Each response fits the operation budget separately;
                        // together they exceed it. Resetting the deadline on
                        // recovery would therefore make this fetch succeed.
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        if count == 0 {
                            return (recovery_status, Body::empty()).into_response();
                        }
                        (
                            StatusCode::MULTI_STATUS,
                            [("content-type", "application/xml")],
                            "<multistatus><sync-token>new-token</sync-token></multistatus>",
                        )
                            .into_response()
                    }
                })),
            )
            .await
            .unwrap();
        });
        let backend = caldav::CalDavBackend::new(
            &CalendarAccount {
                url: format!("http://{address}/report"),
                allow_private_network: true,
                ..account("dav", CalendarProviderKind::CalDav)
            },
            adm(),
        )
        .with_timeout_for_test(std::time::Duration::from_millis(250));
        let error = backend
            .list_events(window().0, window().1, "expired-token")
            .await
            .unwrap_err();
        assert!(matches!(error, CalendarError::Timeout(_)), "got {error:?}");
        assert_eq!(requests.load(Ordering::SeqCst), 2, "no third REPORT");
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn oversized_chunked_and_encoded_calendar_bodies_are_rejected_before_parse() {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let exact_body = {
        let prefix = "BEGIN:VCALENDAR\nX-PADDING:";
        let suffix = "\nEND:VCALENDAR";
        let padding = crate::http::MAX_BODY_BYTES - prefix.len() - suffix.len();
        format!("{prefix}{}{suffix}", "x".repeat(padding))
    };
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(move |request: Request| {
                let exact_body = exact_body.clone();
                async move {
                    if request.uri().path() == "/encoded" {
                        let mut response = (StatusCode::OK, "not decoded").into_response();
                        response
                            .headers_mut()
                            .insert("content-type", HeaderValue::from_static("text/calendar"));
                        response
                            .headers_mut()
                            .insert("content-encoding", HeaderValue::from_static("gzip"));
                        return response;
                    }
                    if request.uri().path() == "/error" {
                        let stream = futures_util::stream::once(async {
                            Ok::<_, std::io::Error>(vec![b'e'; (8 << 10) + 1])
                        });
                        let mut response = Body::from_stream(stream).into_response();
                        *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                        return response;
                    }
                    if request.uri().path() == "/exact" {
                        let mut response = Body::from(exact_body).into_response();
                        response
                            .headers_mut()
                            .insert("content-type", HeaderValue::from_static("text/calendar"));
                        return response;
                    }
                    let size = (crate::http::MAX_BODY_BYTES) + 1;
                    let stream = futures_util::stream::once(async move {
                        Ok::<_, std::io::Error>(vec![b'x'; size])
                    });
                    let mut response = Body::from_stream(stream).into_response();
                    response
                        .headers_mut()
                        .insert("content-type", HeaderValue::from_static("text/calendar"));
                    response
                }
            })),
        )
        .await
        .unwrap();
    });

    for path in ["/chunked", "/exact", "/error", "/encoded"] {
        let backend = ics_url::IcsUrlBackend::new(
            &CalendarAccount {
                url: format!("http://{address}{path}"),
                allow_private_network: true,
                ..account("remote", CalendarProviderKind::IcsUrl)
            },
            adm(),
        );
        let result = backend.list_events(window().0, window().1, "").await;
        if path == "/exact" {
            assert!(result.is_ok(), "exactly capped body should be readable");
            assert!(result.unwrap().events().is_empty());
        } else if path == "/encoded" {
            assert!(matches!(
                result.unwrap_err(),
                CalendarError::Policy("calendar response encoding refused")
            ));
        } else {
            assert!(
                matches!(result.unwrap_err(), CalendarError::BodyLimit(_)),
                "{path} must fail at the application-owned limit"
            );
        }
    }
    server.abort();
    let _ = server.await;
}

async fn raw_fixture_headers(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        assert!(headers.len() < crate::http::MAX_REQUEST_BYTES);
        headers.push(socket.read_u8().await.unwrap());
    }
    headers
}

#[tokio::test]
async fn declared_oversized_calendar_body_is_refused_without_reading_payload() {
    use tokio::io::AsyncWriteExt;
    for provider in [CalendarProviderKind::IcsUrl, CalendarProviderKind::CalDav] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _request = raw_fixture_headers(&mut socket).await;
            let (status, media) = if provider == CalendarProviderKind::CalDav {
                ("207 Multi-Status", "application/xml")
            } else {
                ("200 OK", "text/calendar")
            };
            let headers = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {media}\r\nContent-Length: {}\r\n\r\n",
                crate::http::MAX_BODY_BYTES + 1
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            // Never send the declared payload. Admission must reject headers
            // immediately instead of waiting for body/idle/operation timeout.
            std::future::pending::<()>().await;
        });
        let cfg = CalendarAccount {
            url: format!("http://{address}/declared"),
            allow_private_network: true,
            ..account("declared", provider)
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            match provider {
                CalendarProviderKind::IcsUrl => {
                    ics_url::IcsUrlBackend::new(&cfg, adm())
                        .list_events(window().0, window().1, "")
                        .await
                }
                _ => {
                    caldav::CalDavBackend::new(&cfg, adm())
                        .list_events(window().0, window().1, "")
                        .await
                }
            }
        })
        .await
        .expect("oversized headers must refuse without reading the body");
        assert!(
            matches!(result, Err(CalendarError::BodyLimit(_))),
            "{result:?}"
        );
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn declared_oversized_error_body_is_refused_without_waiting_for_payload() {
    use tokio::io::AsyncWriteExt;

    for provider in [CalendarProviderKind::IcsUrl, CalendarProviderKind::CalDav] {
        for status in [
            "401 Unauthorized",
            "403 Forbidden",
            "409 Conflict",
            "500 Internal Server Error",
            "507 Insufficient Storage",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = raw_fixture_headers(&mut socket).await;
                let (media, method) = if provider == CalendarProviderKind::CalDav {
                    ("application/xml", "REPORT")
                } else {
                    ("text/calendar", "GET")
                };
                let headers = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {media}\r\nContent-Length: {}\r\n\r\n",
                    crate::http::MAX_ERROR_BODY_BYTES + 1
                );
                assert!(
                    std::str::from_utf8(&request)
                        .unwrap_or_default()
                        .starts_with(method),
                    "backend must use the expected request method"
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                // A header-only error must be rejected at the diagnostic cap; do
                // not make the client wait for an absent body until its deadline.
                std::future::pending::<()>().await;
            });
            let cfg = CalendarAccount {
                url: format!("http://{address}/declared-error"),
                allow_private_network: true,
                ..account("declared-error", provider)
            };
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                match provider {
                    CalendarProviderKind::IcsUrl => {
                        ics_url::IcsUrlBackend::new(&cfg, adm())
                            .list_events(window().0, window().1, "prior-token")
                            .await
                    }
                    _ => {
                        caldav::CalDavBackend::new(&cfg, adm())
                            .list_events(window().0, window().1, "prior-token")
                            .await
                    }
                }
            })
            .await
            .expect("oversized error headers must refuse without reading the body");
            assert!(
                matches!(result, Err(CalendarError::BodyLimit(_))),
                "{result:?}"
            );
            server.abort();
            let _ = server.await;
        }
    }
}

#[tokio::test]
async fn gateway_error_media_is_discarded_without_parsing_or_disclosure() {
    use axum::{Router, http::StatusCode, routing::any};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    [("content-type", "text/html")],
                    "<html>secret-gateway-token</html>",
                )
            })),
        )
        .await
        .unwrap();
    });
    for provider in [CalendarProviderKind::IcsUrl, CalendarProviderKind::CalDav] {
        let cfg = CalendarAccount {
            url: format!("http://{address}/error"),
            allow_private_network: true,
            ..account("gateway", provider)
        };
        let result = match provider {
            CalendarProviderKind::IcsUrl => {
                ics_url::IcsUrlBackend::new(&cfg, adm())
                    .list_events(window().0, window().1, "")
                    .await
            }
            _ => {
                caldav::CalDavBackend::new(&cfg, adm())
                    .list_events(window().0, window().1, "")
                    .await
            }
        };
        let error = result.unwrap_err();
        assert!(matches!(error, CalendarError::Auth(_)), "{error:?}");
        assert!(!format!("{error:?}").contains("secret-gateway-token"));
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn caldav_body_limits_cover_exact_chunked_and_error_responses() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let exact_prefix = b"<multistatus><sync-token>exact</sync-token><!--";
    let exact_suffix = b"--></multistatus>";
    let mut exact = Vec::with_capacity(crate::http::MAX_BODY_BYTES);
    exact.extend_from_slice(exact_prefix);
    exact.extend(std::iter::repeat_n(
        b'x',
        crate::http::MAX_BODY_BYTES - exact_prefix.len() - exact_suffix.len(),
    ));
    exact.extend_from_slice(exact_suffix);
    let exact = Arc::new(exact);
    let oversized = Arc::new(vec![b'x'; crate::http::MAX_BODY_BYTES + 1]);
    let server = tokio::spawn({
        let exact = Arc::clone(&exact);
        let oversized = Arc::clone(&oversized);
        async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let exact = Arc::clone(&exact);
                let oversized = Arc::clone(&oversized);
                tokio::spawn(async move {
                    let request = raw_fixture_headers(&mut socket).await;
                    // Consume the REPORT body before closing the socket.
                    // Closing with unread request bytes can reset TCP and
                    // truncate an otherwise exactly-sized valid response.
                    let headers = std::str::from_utf8(&request).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    assert!(length <= crate::http::MAX_REQUEST_BYTES);
                    let mut request_body = vec![0; length];
                    socket.read_exact(&mut request_body).await.unwrap();
                    let path = request
                        .split(|byte| *byte == b' ')
                        .nth(1)
                        .and_then(|path| std::str::from_utf8(path).ok())
                        .unwrap_or_default();
                    let (status, body, chunked) = match path {
                        "/dav-exact" => ("207 Multi-Status", exact.as_slice(), false),
                        "/dav-error" => ("500 Internal Server Error", oversized.as_slice(), true),
                        _ => ("207 Multi-Status", oversized.as_slice(), true),
                    };
                    let transfer = if chunked {
                        "Transfer-Encoding: chunked\r\n"
                    } else {
                        ""
                    };
                    let length = if chunked {
                        String::new()
                    } else {
                        format!("Content-Length: {}\r\n", body.len())
                    };
                    let header = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\n{transfer}{length}Connection: close\r\n\r\n"
                    );
                    if socket.write_all(header.as_bytes()).await.is_err() {
                        return;
                    }
                    if chunked {
                        for chunk in body.chunks(64 * 1024) {
                            if socket
                                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                            if socket.write_all(chunk).await.is_err()
                                || socket.write_all(b"\r\n").await.is_err()
                            {
                                return;
                            }
                        }
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    } else {
                        let _ = socket.write_all(body).await;
                    }
                });
            }
        }
    });

    let exact_result = caldav::CalDavBackend::new(
        &CalendarAccount {
            url: format!("http://{address}/dav-exact"),
            allow_private_network: true,
            ..account("dav-exact", CalendarProviderKind::CalDav)
        },
        adm(),
    )
    .list_events(window().0, window().1, "")
    .await;
    assert!(
        exact_result.is_ok(),
        "exactly capped CalDAV body must parse: {exact_result:?}"
    );

    for path in ["/dav-oversized", "/dav-error"] {
        let error = caldav::CalDavBackend::new(
            &CalendarAccount {
                url: format!("http://{address}{path}"),
                allow_private_network: true,
                ..account("dav-limit", CalendarProviderKind::CalDav)
            },
            adm(),
        )
        .list_events(window().0, window().1, "")
        .await
        .unwrap_err();
        assert!(
            matches!(error, CalendarError::BodyLimit(_)),
            "{path} must stop at the application-owned cap: {error:?}"
        );
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn both_remote_backends_refuse_literal_non_public_addresses_before_connecting() {
    for (provider, url) in [
        (CalendarProviderKind::IcsUrl, "http://127.0.0.1:9/feed"),
        (CalendarProviderKind::IcsUrl, "http://0.0.0.0:9/feed"),
        (CalendarProviderKind::IcsUrl, "http://[::1]:9/feed"),
        (CalendarProviderKind::IcsUrl, "http://[::]:9/feed"),
        (
            CalendarProviderKind::IcsUrl,
            "http://[::ffff:127.0.0.1]:9/feed",
        ),
        (CalendarProviderKind::CalDav, "http://127.0.0.1:9/dav"),
        (CalendarProviderKind::CalDav, "http://0.0.0.0:9/dav"),
        (CalendarProviderKind::CalDav, "http://[::1]:9/dav"),
        (CalendarProviderKind::CalDav, "http://[::]:9/dav"),
        (
            CalendarProviderKind::CalDav,
            "http://[::ffff:127.0.0.1]:9/dav",
        ),
    ] {
        let backend = match provider {
            CalendarProviderKind::IcsUrl => ics_url::IcsUrlBackend::new(
                &CalendarAccount {
                    url: url.into(),
                    ..account("remote", provider)
                },
                adm(),
            ),
            CalendarProviderKind::CalDav => {
                let backend = caldav::CalDavBackend::new(
                    &CalendarAccount {
                        url: url.into(),
                        ..account("remote", provider)
                    },
                    adm(),
                );
                let error = backend
                    .list_events(window().0, window().1, "")
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    CalendarError::Policy("calendar destination refused")
                ));
                continue;
            }
            _ => unreachable!(),
        };
        let error = backend
            .list_events(window().0, window().1, "")
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CalendarError::Policy("calendar destination refused")
        ));
    }
}

#[tokio::test]
async fn media_encoding_and_shared_pool_policies_apply_to_real_backends() {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use std::sync::{Arc, Mutex};

    let records = Arc::new(Mutex::new(Vec::<(
        String,
        String,
        Option<String>,
        String,
        bool,
    )>::new()));
    let observed = Arc::clone(&records);
    let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = CountingListener {
        inner: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        accepts: Arc::clone(&accepts),
    };
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(move |request: Request| {
                let observed = Arc::clone(&observed);
                async move {
                    let path = request.uri().path().to_owned();
                    let uri = request.uri().to_string();
                    let auth = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    let etag = request
                        .headers()
                        .get("if-none-match")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let cookie = request.headers().contains_key("cookie");
                    let body = to_bytes(request.into_body(), 128 * 1024).await.unwrap();
                    let body = String::from_utf8_lossy(&body).into_owned();
                    observed
                        .lock()
                        .unwrap()
                        .push((uri, auth, etag, body, cookie));
                    let is_dav = path.starts_with("/dav-") || path == "/dav";
                    let mut response = if is_dav {
                        (
                            StatusCode::MULTI_STATUS,
                            Body::from(
                                "<multistatus><sync-token>dav-new</sync-token></multistatus>",
                            ),
                        )
                            .into_response()
                    } else {
                        (StatusCode::OK, Body::from(ONE_EVENT)).into_response()
                    };
                    let content_type = match path.as_str() {
                        "/ics-application-calendar" => "application/calendar; charset=utf-8",
                        "/ics-application-ics" => "application/ics; charset=utf-8",
                        "/ics-text-plain" => "text/plain; charset=utf-8",
                        "/dav-text-xml" => "text/xml; charset=utf-8",
                        "/dav-application-dav" => "application/dav+xml; charset=utf-8",
                        _ if is_dav => "Application/XML; charset=utf-8",
                        _ => "TEXT/CALENDAR; charset=utf-8",
                    };
                    response
                        .headers_mut()
                        .insert("content-type", HeaderValue::from_static(content_type));
                    if path == "/encoding" {
                        response
                            .headers_mut()
                            .append("content-encoding", HeaderValue::from_static("identity"));
                        response
                            .headers_mut()
                            .append("content-encoding", HeaderValue::from_static("gzip"));
                    } else {
                        response
                            .headers_mut()
                            .insert("set-cookie", HeaderValue::from_static("calendar=one"));
                        response
                            .headers_mut()
                            .insert("etag", HeaderValue::from_static("next-validator"));
                    }
                    if path == "/repeat-content-type" {
                        response
                            .headers_mut()
                            .append("content-type", HeaderValue::from_static("text/calendar"));
                    }
                    response
                }
            })),
        )
        .await
        .unwrap();
    });

    let config = |first_query: &str, second_query: &str| CalendarConfig {
        accounts: vec![
            CalendarAccount {
                name: "first".into(),
                provider: CalendarProviderKind::IcsUrl,
                url: format!("http://{address}/feed?account={first_query}"),
                username: "first-user".into(),
                token: "first-secret".into(),
                allow_private_network: true,
                ..Default::default()
            },
            CalendarAccount {
                name: "second".into(),
                provider: CalendarProviderKind::IcsUrl,
                url: format!("http://{address}/feed?account={second_query}"),
                username: "second-user".into(),
                token: "second-secret".into(),
                allow_private_network: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let tokens = std::collections::BTreeMap::from([
        ("first".to_owned(), "first-validator".to_owned()),
        ("second".to_owned(), "second-validator".to_owned()),
    ]);
    router(&config("first-query", "second-query"))
        .list_events(window().0, window().1, &tokens)
        .await;
    // Rebuilding the router/backends must still clone the same process pool;
    // credentials and validators remain request-local.
    router(&config("first-query", "second-query"))
        .list_events(window().0, window().1, &tokens)
        .await;

    let dav = caldav::CalDavBackend::new(
        &CalendarAccount {
            url: format!("http://{address}/dav?account=dav-query"),
            token: "dav-secret".into(),
            allow_private_network: true,
            ..account("dav", CalendarProviderKind::CalDav)
        },
        adm(),
    );
    dav.list_events(window().0, window().1, "dav-sync-secret")
        .await
        .unwrap();

    for path in [
        "/feed",
        "/ics-application-calendar",
        "/ics-application-ics",
        "/ics-text-plain",
    ] {
        ics_url::IcsUrlBackend::new(
            &CalendarAccount {
                url: format!("http://{address}{path}"),
                allow_private_network: true,
                ..account("mime", CalendarProviderKind::IcsUrl)
            },
            adm(),
        )
        .list_events(window().0, window().1, "")
        .await
        .unwrap();
    }
    for path in ["/dav", "/dav-text-xml", "/dav-application-dav"] {
        caldav::CalDavBackend::new(
            &CalendarAccount {
                url: format!("http://{address}{path}"),
                allow_private_network: true,
                ..account("mime-dav", CalendarProviderKind::CalDav)
            },
            adm(),
        )
        .list_events(window().0, window().1, "")
        .await
        .unwrap();
    }

    let encoded = ics_url::IcsUrlBackend::new(
        &CalendarAccount {
            url: format!("http://{address}/encoding"),
            allow_private_network: true,
            ..account("encoded", CalendarProviderKind::IcsUrl)
        },
        adm(),
    );
    let error = encoded
        .list_events(window().0, window().1, "")
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CalendarError::Policy("calendar response encoding refused")
    ));
    let repeated = ics_url::IcsUrlBackend::new(
        &CalendarAccount {
            url: format!("http://{address}/repeat-content-type"),
            allow_private_network: true,
            ..account("repeated", CalendarProviderKind::IcsUrl)
        },
        adm(),
    )
    .list_events(window().0, window().1, "")
    .await
    .unwrap_err();
    assert!(matches!(
        repeated,
        CalendarError::Policy("calendar response content type refused")
    ));

    {
        let records = records.lock().unwrap();
        let first_records: Vec<_> = records
            .iter()
            .filter(|record| record.0.contains("account=first-query"))
            .collect();
        let second_records: Vec<_> = records
            .iter()
            .filter(|record| record.0.contains("account=second-query"))
            .collect();
        assert_eq!(first_records.len(), 2);
        assert_eq!(second_records.len(), 2);
        assert!(
            first_records
                .iter()
                .all(|record| record.1.starts_with("Basic "))
        );
        assert!(
            second_records
                .iter()
                .all(|record| record.1.starts_with("Basic "))
        );
        assert_ne!(first_records[0].1, second_records[0].1);
        assert!(
            first_records
                .iter()
                .any(|record| record.2.as_deref() == Some("first-validator"))
        );
        assert!(
            second_records
                .iter()
                .any(|record| record.2.as_deref() == Some("second-validator"))
        );
        assert!(
            records
                .iter()
                .any(|record| record.0.contains("account=dav-query"))
        );
        assert!(
            records
                .iter()
                .any(|record| record.3.contains("dav-sync-secret"))
        );
        assert!(
            records.iter().all(|record| !record.4),
            "pool must not retain cookies"
        );
        assert!(accepts.load(std::sync::atomic::Ordering::SeqCst) < records.len());
    }
    server.abort();
    let _ = server.await;
}

// --- the ics backend --------------------------------------------------------

#[test]
fn an_ics_file_is_parsed() {
    let t = Tmp::new("file");
    let f = t.0.join("work.ics");
    std::fs::write(&f, ONE_EVENT).unwrap();
    let (from, to) = window();
    let page = block_on(ics_backend(&f.display().to_string()).list_events(from, to, "")).unwrap();
    assert_eq!(page.events().len(), 1);
    assert_eq!(page.events()[0].title, "Standup");
}

#[test]
fn a_directory_of_ics_files_is_read_as_one_calendar() {
    // This is the vdir layout vdirsyncer and khal write, so supporting it means
    // those users need no extra configuration at all.
    let t = Tmp::new("vdir");
    std::fs::write(t.0.join("a.ics"), ONE_EVENT).unwrap();
    std::fs::write(
        t.0.join("b.ics"),
        ONE_EVENT
            .replace("UID:e1", "UID:e2")
            .replace("Standup", "Retro"),
    )
    .unwrap();
    // A non-.ics file in the same directory is ignored, not parsed as junk.
    std::fs::write(t.0.join("color"), "#ff0000").unwrap();

    let (from, to) = window();
    let page = block_on(ics_backend(&t.0.display().to_string()).list_events(from, to, "")).unwrap();
    let mut titles: Vec<_> = page.events().iter().map(|e| e.title.clone()).collect();
    titles.sort();
    assert_eq!(titles, vec!["Retro", "Standup"]);
}

#[test]
fn a_missing_path_is_a_non_transient_io_error() {
    let (from, to) = window();
    let err =
        block_on(ics_backend("/definitely/not/here.ics").list_events(from, to, "")).unwrap_err();
    assert!(matches!(err, CalendarError::Io(_)), "got {err:?}");
    assert!(
        !err.is_transient(),
        "config mistakes must not look like blips"
    );
}

#[test]
fn the_ics_backend_advertises_no_write_capabilities() {
    let b = ics_backend("/tmp/x.ics");
    assert_eq!(b.caps(), CalendarCaps::default());
    assert_eq!(b.provider_id(), "ics");
    // And the defaulted write methods really do refuse.
    let e = thegn_core::calendar::CalEvent::new(
        "u",
        "t",
        thegn_core::calendar::EventTime::Date {
            date: chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
        },
        thegn_core::calendar::EventTime::Date {
            date: chrono::NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
        },
    );
    assert!(matches!(
        block_on(b.create_event(&e)),
        Err(CalendarError::Unsupported(_))
    ));
    assert!(matches!(
        block_on(b.delete_event("x", EditScope::AllInstances)),
        Err(CalendarError::Unsupported(_))
    ));
}

// --- the router -------------------------------------------------------------

#[test]
fn an_empty_config_builds_an_unconfigured_router() {
    let r = router(&CalendarConfig::default());
    assert!(!r.is_configured());
    let (from, to) = window();
    let out = block_on(r.list_events(from, to, &BTreeMap::new()));
    assert!(out.is_empty());
}

#[test]
fn caldav_reports_real_delta_support() {
    let cfg = CalendarConfig {
        accounts: vec![CalendarAccount {
            url: "https://dav.example.com/cal/".into(),
            ..account("dav", CalendarProviderKind::CalDav)
        }],
        ..CalendarConfig::default()
    };
    assert!(router(&cfg).is_configured());
    let b = caldav::CalDavBackend::new(
        &CalendarAccount {
            url: "https://dav.example.com/cal/".into(),
            ..account("dav", CalendarProviderKind::CalDav)
        },
        adm(),
    );
    assert_eq!(b.provider_id(), "caldav");
    // `sync-collection` gives tombstones, not just a conditional refetch — the
    // only provider here that populates `EventPage::deleted`.
    assert!(b.caps().incremental);

    // A missing url is a config problem, not a network one.
    let bare = caldav::CalDavBackend::new(&account("dav", CalendarProviderKind::CalDav), adm());
    let (from, to) = window();
    let err = block_on(bare.list_events(from, to, "")).unwrap_err();
    assert!(matches!(err, CalendarError::NotConfigured));
}

// --- caldav xml -------------------------------------------------------------

#[test]
fn a_multistatus_yields_events_and_its_sync_token() {
    let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/cal/e1.ics</d:href>
    <d:propstat><d:prop>
      <d:getetag>"abc"</d:getetag>
      <cal:calendar-data>BEGIN:VEVENT
UID:e1
SUMMARY:Standup
DTSTART:20260821T090000Z
END:VEVENT</cal:calendar-data>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:sync-token>http://example.com/ns/sync/42</d:sync-token>
</d:multistatus>"#;
    let (responses, token) = caldav::parse_multistatus(xml);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].href, "/cal/e1.ics");
    assert!(!responses[0].deleted);
    assert!(responses[0].ics.contains("UID:e1"));
    assert_eq!(token, "http://example.com/ns/sync/42");
}

#[test]
fn a_404_response_is_read_as_a_tombstone() {
    // How `sync-collection` reports a deletion — the href is all it carries.
    let xml = r#"<multistatus xmlns="DAV:">
  <response>
    <href>/cal/gone.ics</href>
    <status>HTTP/1.1 404 Not Found</status>
  </response>
  <sync-token>tok-2</sync-token>
</multistatus>"#;
    let (responses, token) = caldav::parse_multistatus(xml);
    assert_eq!(responses.len(), 1);
    assert!(responses[0].deleted);
    assert_eq!(token, "tok-2");
    assert_eq!(caldav::uid_from_href("/cal/gone.ics"), "gone");
}

#[test]
fn namespace_prefixes_do_not_matter() {
    // Servers differ: `d:`, `D:`, or no prefix at all. Matching on the local
    // name is what makes one parser work against all of them.
    for open in ["<D:href>", "<href>", "<x:href>"] {
        let close = open.replace('<', "</");
        let xml = format!(
            "<multistatus><response>{open}/cal/a.ics{close}             <calendar-data>BEGIN:VEVENT
UID:a
DTSTART:20260821T090000Z
END:VEVENT             </calendar-data></response></multistatus>"
        );
        let (r, _) = caldav::parse_multistatus(&xml);
        assert_eq!(r.len(), 1, "failed for {open}");
        assert_eq!(r[0].href, "/cal/a.ics");
    }
}

#[test]
fn xml_entities_in_calendar_data_are_unescaped() {
    // An `&` in a summary arrives as `&amp;`; leaving it escaped would show
    // "R&amp;D sync" in the agenda.
    let xml =
        "<multistatus><response><href>/c/a.ics</href><calendar-data>               BEGIN:VEVENT
UID:a
SUMMARY:R&amp;D &lt;sync&gt;
DTSTART:20260821T090000Z
               END:VEVENT</calendar-data></response></multistatus>";
    let (r, _) = caldav::parse_multistatus(xml);
    assert!(r[0].ics.contains("R&D <sync>"), "got {}", r[0].ics);
}

#[test]
fn an_empty_or_malformed_multistatus_is_not_a_panic() {
    assert_eq!(caldav::parse_multistatus("").0.len(), 0);
    assert_eq!(caldav::parse_multistatus("not xml at all").0.len(), 0);
    // An unterminated element must not hang or index out of bounds.
    assert_eq!(caldav::parse_multistatus("<response><href>/a").0.len(), 0);
    // A response with no href is skipped rather than becoming a blank id.
    assert_eq!(
        caldav::parse_multistatus("<multistatus><response></response></multistatus>")
            .0
            .len(),
        0
    );
}

#[test]
fn uid_from_href_handles_the_shapes_servers_actually_send() {
    assert_eq!(caldav::uid_from_href("/cal/abc-123.ics"), "abc-123");
    assert_eq!(
        caldav::uid_from_href("https://dav.example.com/u/cal/abc.ics"),
        "abc"
    );
    // No extension, and a trailing slash.
    assert_eq!(caldav::uid_from_href("/cal/abc"), "abc");
    assert_eq!(caldav::uid_from_href("/cal/abc/"), "abc");
}

#[test]
fn every_event_is_stamped_with_its_source_and_color() {
    let t = Tmp::new("stamp");
    std::fs::write(t.0.join("a.ics"), ONE_EVENT).unwrap();
    let cfg = CalendarConfig {
        accounts: vec![CalendarAccount {
            path: t.0.display().to_string(),
            color: "amber".into(),
            ..account("work", CalendarProviderKind::Ics)
        }],
        ..CalendarConfig::default()
    };
    let r = router(&cfg);
    let (from, to) = window();
    let out = block_on(r.list_events(from, to, &BTreeMap::new()));
    assert_eq!(out.len(), 1);
    let page = out[0].result.as_ref().unwrap();
    // Identity is what makes ids globally unique across accounts.
    assert_eq!(page.events()[0].source.as_str(), "ics:work");
    assert_eq!(page.events()[0].id().as_str(), "ics:work/e1");
    assert_eq!(page.events()[0].color, Some(thegn_core::theme::Hue::Amber));
}

#[test]
fn one_failing_account_does_not_affect_another() {
    // THE reason results are per-account: a broken source must not be able to
    // discard a working one's data.
    let t = Tmp::new("mixed");
    std::fs::write(t.0.join("a.ics"), ONE_EVENT).unwrap();
    let cfg = CalendarConfig {
        accounts: vec![
            CalendarAccount {
                path: "/definitely/not/here.ics".into(),
                ..account("broken", CalendarProviderKind::Ics)
            },
            CalendarAccount {
                path: t.0.display().to_string(),
                ..account("good", CalendarProviderKind::Ics)
            },
        ],
        ..CalendarConfig::default()
    };
    let (from, to) = window();
    let out = block_on(router(&cfg).list_events(from, to, &BTreeMap::new()));
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].account, "broken");
    assert!(out[0].result.is_err());
    assert_eq!(out[1].account, "good");
    assert_eq!(out[1].result.as_ref().unwrap().events().len(), 1);
}

// --- the command (plugin) backend -------------------------------------------

fn command_account(script: &str) -> CalendarAccount {
    CalendarAccount {
        command: vec!["sh".into(), "-c".into(), script.into()],
        ..account("plug", CalendarProviderKind::Command)
    }
}

fn run_plugin(script: &str) -> Result<EventPage, CalendarError> {
    let b = command::CommandBackend::new(&command_account(script), adm());
    let (from, to) = window();
    block_on(b.list_events(from, to, ""))
}

#[test]
fn a_four_field_event_is_a_complete_plugin_reply() {
    // The documented minimum a plugin has to emit.
    let page = run_plugin(
        r#"echo '{"method":"events","params":{"events":[{"uid":"1","title":"Standup","start":{"kind":"date","date":"2026-08-21"},"end":{"kind":"date","date":"2026-08-22"}}]}}'"#,
    )
    .unwrap();
    assert_eq!(page.events().len(), 1);
    assert_eq!(page.events()[0].title, "Standup");
}

#[test]
fn the_query_window_reaches_the_plugin_as_environment() {
    // The asymmetry that makes the surface writable in shell: env in, JSON out.
    let page = run_plugin(
        r#"echo "{\"method\":\"events\",\"params\":{\"sync_token\":\"$THEGN_CAL_FROM..$THEGN_CAL_TO\"}}""#,
    )
    .unwrap();
    assert_eq!(page.sync_token(), "2026-08-01..2026-08-31");
}

#[test]
fn several_event_messages_accumulate() {
    let page = run_plugin(
        r#"echo '{"method":"events","params":{"events":[{"uid":"1","title":"A","start":{"kind":"date","date":"2026-08-21"},"end":{"kind":"date","date":"2026-08-22"}}]}}'
           echo '{"method":"events","params":{"events":[{"uid":"2","title":"B","start":{"kind":"date","date":"2026-08-22"},"end":{"kind":"date","date":"2026-08-23"}}],"sync_token":"t2"}}'"#,
    )
    .unwrap();
    assert_eq!(page.events().len(), 2, "pages accumulate");
    assert_eq!(page.sync_token(), "t2", "the last token wins");
}

#[test]
fn a_manifest_is_negotiated_and_a_denied_capability_is_not_fatal() {
    // A plugin asking for more than it was granted should still deliver.
    let page = run_plugin(
        r#"echo '{"method":"manifest","params":{"id":"p","name":"p","version":"1","api":"0.1.0","capabilities":["run:khal","network:evil.example.com"],"contributions":[]}}'
           echo '{"method":"events","params":{"events":[]}}'"#,
    )
    .unwrap();
    assert!(page.events().is_empty());
}

#[test]
fn a_plugin_speaking_a_future_api_major_is_rejected() {
    let err = run_plugin(
        r#"echo '{"method":"manifest","params":{"id":"p","name":"p","version":"1","api":"9.0.0","capabilities":[],"contributions":[]}}'"#,
    )
    .unwrap_err();
    assert!(matches!(err, CalendarError::Api(_)), "got {err:?}");
}

#[test]
fn a_plugin_that_fails_reports_its_stderr() {
    let err = run_plugin("echo 'khal not found' >&2; exit 1").unwrap_err();
    assert!(
        err.to_string().contains("khal not found"),
        "the reason must survive: {err}"
    );
    assert!(!err.is_transient(), "a broken plugin is not a network blip");
}

#[test]
fn a_plugin_timeout_is_transient_so_the_cache_survives() {
    let b = command::CommandBackend::new(
        &CalendarAccount {
            timeout_secs: 1,
            ..command_account("sleep 30")
        },
        adm(),
    );
    let err = block_on(b.list_events(
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
        "",
    ))
    .unwrap_err();
    assert!(err.is_transient(), "a hang should be retried, not surfaced");
}

#[test]
fn an_unconfigured_command_account_is_not_configured() {
    let b = command::CommandBackend::new(&account("p", CalendarProviderKind::Command), adm());
    let err = block_on(b.list_events(
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
        "",
    ))
    .unwrap_err();
    assert!(matches!(err, CalendarError::NotConfigured));
}

#[test]
fn an_ics_url_account_with_no_url_is_not_configured() {
    let b = ics_url::IcsUrlBackend::new(&account("u", CalendarProviderKind::IcsUrl), adm());
    let err = block_on(b.list_events(
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
        "",
    ))
    .unwrap_err();
    assert!(matches!(err, CalendarError::NotConfigured));
    // ETag conditional fetching is the incremental story for subscribed URLs.
    assert!(b.caps().incremental);
}

#[test]
fn webcal_urls_are_fetched_over_https() {
    // `webcal://` only tells the OS to hand the link to a calendar app; over the
    // wire it is an ordinary GET.
    let b = ics_url::IcsUrlBackend::new(
        &CalendarAccount {
            url: "webcal://example.com/c.ics".into(),
            ..account("u", CalendarProviderKind::IcsUrl)
        },
        adm(),
    );
    assert_eq!(b.provider_id(), "ics_url");
}

// --- admission budget --------------------------------------------------------

fn budget_of(n: usize) -> AccountAdmission {
    AccountAdmission::isolated(AdmissionBudget::new(n).unwrap())
}

fn ics_feed(n: usize) -> String {
    let mut s = String::from("BEGIN:VCALENDAR\r\n");
    for i in 0..n {
        s.push_str(&format!(
            "BEGIN:VEVENT\r\nUID:e{i}\r\nSUMMARY:E{i}\r\nDTSTART:20260821T090000Z\r\nEND:VEVENT\r\n"
        ));
    }
    s.push_str("END:VCALENDAR\r\n");
    s
}

fn is_admission(e: &CalendarError, limit: thegn_core::calendar::AdmissionLimit) -> bool {
    matches!(e, CalendarError::Admission(a) if a.limit == limit)
}

#[test]
fn a_local_file_over_max_events_is_refused_whole() {
    use thegn_core::calendar::AdmissionLimit;
    let t = Tmp::new("adm-file");
    let f = t.0.join("cal.ics");
    std::fs::write(&f, ics_feed(3)).unwrap();
    let acct = CalendarAccount {
        path: f.display().to_string(),
        ..account("t", CalendarProviderKind::Ics)
    };
    let (from, to) = window();
    let err =
        block_on(ics::IcsBackend::new(&acct, budget_of(2)).list_events(from, to, "")).unwrap_err();
    assert!(
        is_admission(&err, AdmissionLimit::AccountRecords),
        "{err:?}"
    );
    assert!(!err.is_transient());
    let page =
        block_on(ics::IcsBackend::new(&acct, budget_of(3)).list_events(from, to, "")).unwrap();
    assert_eq!(page.events().len(), 3);
}

#[test]
fn a_vdir_over_max_events_is_refused_not_truncated() {
    use thegn_core::calendar::AdmissionLimit;
    let t = Tmp::new("adm-vdir");
    for i in 0..3 {
        std::fs::write(
            t.0.join(format!("{i}.ics")),
            ics_feed(1).replace("e0", &format!("f{i}")),
        )
        .unwrap();
    }
    let acct = CalendarAccount {
        path: t.0.display().to_string(),
        ..account("t", CalendarProviderKind::Ics)
    };
    let (from, to) = window();
    let err =
        block_on(ics::IcsBackend::new(&acct, budget_of(2)).list_events(from, to, "")).unwrap_err();
    assert!(
        is_admission(&err, AdmissionLimit::AccountRecords),
        "{err:?}"
    );
}

#[test]
fn an_oversized_local_file_is_refused_before_it_is_read() {
    use thegn_core::calendar::AdmissionLimit;
    let t = Tmp::new("adm-big");
    let f = t.0.join("big.ics");
    let file = std::fs::File::create(&f).unwrap();
    // Sparse: the size is declared without writing 32 MiB.
    file.set_len(thegn_core::calendar::admission::MAX_SOURCE_DOCUMENT_BYTES as u64 + 1)
        .unwrap();
    let (from, to) = window();
    let err =
        block_on(ics_backend(&f.display().to_string()).list_events(from, to, "")).unwrap_err();
    assert!(is_admission(&err, AdmissionLimit::DocumentBytes), "{err:?}");
}

#[test]
fn a_page_owns_its_reservation_until_dropped() {
    let t = Tmp::new("adm-lease");
    let f = t.0.join("cal.ics");
    std::fs::write(&f, ics_feed(3)).unwrap();
    let pool = AdmissionPool::new(100, 64 << 20);
    let acct = CalendarAccount {
        path: f.display().to_string(),
        ..account("t", CalendarProviderKind::Ics)
    };
    let backend = ics::IcsBackend::new(
        &acct,
        AccountAdmission::new(AdmissionBudget::default(), pool.clone()),
    );
    let (from, to) = window();
    let page = block_on(backend.list_events(from, to, "")).unwrap();
    // The transient file reservation is gone; the retained events are held.
    let (records, bytes) = pool.in_use();
    assert_eq!(records, 3);
    assert_eq!((records, bytes), page.reserved());
    assert!(bytes > 0 && bytes < 1 << 20, "{bytes}");
    drop(page);
    assert_eq!(pool.in_use(), (0, 0));
}

fn two_local_accounts(t: &Tmp) -> CalendarConfig {
    std::fs::write(t.0.join("a.ics"), ics_feed(1)).unwrap();
    std::fs::write(t.0.join("b.ics"), ics_feed(1)).unwrap();
    CalendarConfig {
        accounts: vec![
            CalendarAccount {
                path: t.0.join("a.ics").display().to_string(),
                ..account("a", CalendarProviderKind::Ics)
            },
            CalendarAccount {
                path: t.0.join("b.ics").display().to_string(),
                ..account("b", CalendarProviderKind::Ics)
            },
        ],
        ..CalendarConfig::default()
    }
}

#[test]
fn applying_each_page_before_the_next_fetch_prevents_starvation() {
    use thegn_core::calendar::AdmissionLimit;
    // A pool with room for exactly one record: two accounts can only both
    // succeed if the first page's lease is released before the second fetch.
    let t = Tmp::new("adm-each");
    let cfg = two_local_accounts(&t);
    let pool = AdmissionPool::new(1, 64 << 20);
    let r = CalendarRouter::from_config_with_pool(&cfg, pool.clone());
    let (from, to) = window();
    let mut ok = 0;
    block_on(r.list_events_each(from, to, &BTreeMap::new(), |res| {
        assert!(res.result.is_ok(), "{:?}", res.result.err());
        ok += 1;
    }));
    assert_eq!(ok, 2);
    assert_eq!(pool.in_use(), (0, 0));

    // Holding every page (the collecting form) keeps the first lease alive, so
    // the second account is refused — typed, immediate, never a wait.
    let out = block_on(r.list_events(from, to, &BTreeMap::new()));
    assert!(out[0].result.is_ok());
    let err = out[1].result.as_ref().unwrap_err();
    assert!(is_admission(err, AdmissionLimit::GlobalRecords), "{err:?}");
    drop(out);
    assert_eq!(pool.in_use(), (0, 0));
}

#[test]
fn the_router_forwards_the_configured_budget_and_charges_stamps() {
    use thegn_core::calendar::AdmissionLimit;
    let t = Tmp::new("adm-router");
    std::fs::write(t.0.join("a.ics"), ics_feed(3)).unwrap();
    let mut cfg = CalendarConfig {
        accounts: vec![CalendarAccount {
            path: t.0.join("a.ics").display().to_string(),
            ..account("work", CalendarProviderKind::Ics)
        }],
        max_events: 2,
        ..CalendarConfig::default()
    };
    let (from, to) = window();
    let out = block_on(router(&cfg).list_events(from, to, &BTreeMap::new()));
    let err = out[0].result.as_ref().unwrap_err();
    assert!(is_admission(err, AdmissionLimit::AccountRecords), "{err:?}");

    cfg.max_events = 3;
    let out = block_on(router(&cfg).list_events(from, to, &BTreeMap::new()));
    let page = out[0].result.as_ref().unwrap();
    let unstamped = {
        let direct = ics::IcsBackend::new(&cfg.accounts[0], adm());
        block_on(direct.list_events(from, to, ""))
            .unwrap()
            .reserved()
            .1
    };
    assert_eq!(page.reserved().1, unstamped + 3 * "ics:work".len());
}

async fn serve_fixed(
    status: axum::http::StatusCode,
    content_type: &'static str,
    body: String,
) -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use axum::response::IntoResponse;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = hits.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().fallback(axum::routing::any(move || {
                let body = body.clone();
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (status, [("content-type", content_type)], body).into_response()
                }
            })),
        )
        .await
        .unwrap();
    });
    (address, hits, server)
}

#[tokio::test]
async fn a_remote_feed_over_the_budget_never_advances_its_etag() {
    use thegn_core::calendar::AdmissionLimit;
    let (address, _hits, server) =
        serve_fixed(axum::http::StatusCode::OK, "text/calendar", ics_feed(50)).await;
    let acct = CalendarAccount {
        url: format!("http://{address}/feed.ics"),
        allow_private_network: true,
        ..account("u", CalendarProviderKind::IcsUrl)
    };
    let pool = AdmissionPool::new(1_000, 128 << 20);
    let b = ics_url::IcsUrlBackend::new(
        &acct,
        AccountAdmission::new(AdmissionBudget::new(10).unwrap(), pool.clone()),
    );
    let (from, to) = window();
    let err = b.list_events(from, to, "").await.unwrap_err();
    assert!(
        is_admission(&err, AdmissionLimit::AccountRecords),
        "{err:?}"
    );
    // Refusal released the body and every partial reservation.
    assert_eq!(pool.in_use(), (0, 0));

    let b = ics_url::IcsUrlBackend::new(
        &acct,
        AccountAdmission::new(AdmissionBudget::new(50).unwrap(), pool.clone()),
    );
    let page = b.list_events(from, to, "").await.unwrap();
    assert_eq!(page.events().len(), 50);
    // Only the retained page is held — the 32 MiB body reservation is gone.
    assert_eq!(pool.in_use(), page.reserved());
    assert!(page.reserved().1 < 1 << 20);
    drop(page);
    assert_eq!(pool.in_use(), (0, 0));
    server.abort();
}

#[tokio::test]
async fn a_saturated_global_budget_refuses_before_any_request() {
    let (address, hits, server) =
        serve_fixed(axum::http::StatusCode::OK, "text/calendar", ics_feed(1)).await;
    // Less room than one transport body: the fetch must not even be sent.
    let pool = AdmissionPool::new(1_000, 1 << 20);
    for provider in [CalendarProviderKind::IcsUrl, CalendarProviderKind::CalDav] {
        let acct = CalendarAccount {
            url: format!("http://{address}/x"),
            allow_private_network: true,
            ..account("r", provider)
        };
        let admission = AccountAdmission::new(AdmissionBudget::default(), pool.clone());
        let backend: Box<dyn CalendarBackend> = match provider {
            CalendarProviderKind::IcsUrl => Box::new(ics_url::IcsUrlBackend::new(&acct, admission)),
            _ => Box::new(caldav::CalDavBackend::new(&acct, admission)),
        };
        let (from, to) = window();
        let err = backend.list_events(from, to, "").await.unwrap_err();
        match err {
            CalendarError::Admission(a) => assert!(a.is_contention()),
            other => panic!("expected a contention refusal, got {other:?}"),
        }
    }
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(pool.in_use(), (0, 0));
    server.abort();
}

fn multistatus(events: usize, deletions: usize) -> String {
    let mut s = String::from("<d:multistatus xmlns:d=\"DAV:\">");
    for i in 0..events {
        s.push_str(&format!(
            "<d:response><d:href>/cal/e{i}.ics</d:href><d:propstat><d:prop><c:calendar-data>{}</c:calendar-data></d:prop></d:propstat></d:response>",
            ics_feed(1).replace("e0", &format!("e{i}"))
        ));
    }
    for i in 0..deletions {
        s.push_str(&format!(
            "<d:response><d:href>/cal/gone{i}.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>"
        ));
    }
    s.push_str("<d:sync-token>tok-next</d:sync-token></d:multistatus>");
    s
}

#[tokio::test]
async fn caldav_events_and_deletions_share_the_budget() {
    use thegn_core::calendar::AdmissionLimit;
    let (address, _hits, server) = serve_fixed(
        axum::http::StatusCode::MULTI_STATUS,
        "application/xml",
        multistatus(2, 3),
    )
    .await;
    let acct = CalendarAccount {
        url: format!("http://{address}/cal/"),
        allow_private_network: true,
        ..account("dav", CalendarProviderKind::CalDav)
    };
    let (from, to) = window();
    let err = caldav::CalDavBackend::new(&acct, budget_of(4))
        .list_events(from, to, "tok-prev")
        .await
        .unwrap_err();
    assert!(
        is_admission(&err, AdmissionLimit::AccountRecords),
        "{err:?}"
    );
    let page = caldav::CalDavBackend::new(&acct, budget_of(5))
        .list_events(from, to, "tok-prev")
        .await
        .unwrap();
    assert_eq!(page.events().len(), 2);
    assert_eq!(page.deleted().len(), 3);
    assert_eq!(page.sync_token(), "tok-next");
    server.abort();
}

#[tokio::test]
async fn caldav_many_deletions_are_refused_at_the_cap() {
    use thegn_core::calendar::AdmissionLimit;
    let (address, _hits, server) = serve_fixed(
        axum::http::StatusCode::MULTI_STATUS,
        "application/xml",
        multistatus(0, 5_000),
    )
    .await;
    let acct = CalendarAccount {
        url: format!("http://{address}/cal/"),
        allow_private_network: true,
        ..account("dav", CalendarProviderKind::CalDav)
    };
    let (from, to) = window();
    let err = caldav::CalDavBackend::new(&acct, budget_of(100))
        .list_events(from, to, "tok-prev")
        .await
        .unwrap_err();
    assert!(
        is_admission(&err, AdmissionLimit::AccountRecords),
        "{err:?}"
    );
    server.abort();
}

#[test]
fn caldav_calendar_data_is_unescaped_exactly_once() {
    let xml = "<multistatus><response><href>/a.ics</href><calendar-data>SUMMARY:a &amp;lt; b</calendar-data></response></multistatus>";
    let (r, _) = caldav::parse_multistatus(xml);
    assert_eq!(r[0].ics, "SUMMARY:a &lt; b");
}

fn plugin_with(budget: usize, script: &str) -> Result<EventPage, CalendarError> {
    let b = command::CommandBackend::new(&command_account(script), budget_of(budget));
    let (from, to) = window();
    block_on(b.list_events(from, to, ""))
}

const PLUGIN_EVENT: &str = r#"{"uid":"x","title":"T","start":{"kind":"date","date":"2026-08-21"},"end":{"kind":"date","date":"2026-08-22"}}"#;

#[test]
fn a_huge_plugin_stream_is_refused_at_the_budget() {
    use thegn_core::calendar::AdmissionLimit;
    let line =
        format!(r#"{{"method":"events","params":{{"events":[{PLUGIN_EVENT},{PLUGIN_EVENT}]}}}}"#);
    // 10 000 lines × 2 events; the plugin still exits cleanly because the
    // pipe is drained after refusal.
    let script = format!("yes '{line}' | head -n 10000");
    let err = plugin_with(10, &script).unwrap_err();
    assert!(
        is_admission(&err, AdmissionLimit::AccountRecords),
        "{err:?}"
    );
    let page = plugin_with(20, &format!("yes '{line}' | head -n 10")).unwrap();
    assert_eq!(page.events().len(), 20);
}

#[test]
fn plugin_deletions_count_against_the_budget() {
    use thegn_core::calendar::AdmissionLimit;
    let err = plugin_with(
        2,
        r#"echo '{"method":"events","params":{"deleted":["a","b","c"],"sync_token":"t"}}'"#,
    )
    .unwrap_err();
    assert!(
        is_admission(&err, AdmissionLimit::AccountRecords),
        "{err:?}"
    );
}

#[test]
fn the_plugin_sees_the_real_budget() {
    let page = plugin_with(
        7,
        r#"echo "{\"method\":\"events\",\"params\":{\"sync_token\":\"$THEGN_CAL_MAX_EVENTS\"}}""#,
    )
    .unwrap();
    assert_eq!(page.sync_token(), "7");
}

#[test]
fn a_truncated_plugin_run_is_an_error_not_a_partial_page() {
    use thegn_core::calendar::AdmissionLimit;
    let script = format!(
        "yes '{{\"method\":\"log\",\"params\":{{}}}}' | head -n {}",
        crate::plugin::proc::MAX_LINES + 1
    );
    let err = plugin_with(10, &script).unwrap_err();
    assert!(is_admission(&err, AdmissionLimit::Messages), "{err:?}");
}

#[test]
fn an_invalid_event_list_is_dropped_whole_and_uncharged() {
    let page = plugin_with(
        10,
        &format!(
            r#"echo '{{"method":"events","params":{{"events":[{PLUGIN_EVENT},{{"bogus":1}}]}}}}'"#
        ),
    )
    .unwrap();
    assert!(page.events().is_empty());
    assert_eq!(page.reserved().0, 0);
}
