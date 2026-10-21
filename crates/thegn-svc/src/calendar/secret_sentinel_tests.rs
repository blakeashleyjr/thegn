use super::*;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::any;
use std::sync::Arc;
use thegn_core::config_calendar::{CalendarAccount, CalendarConfig, CalendarProviderKind};

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

fn window() -> (chrono::NaiveDate, chrono::NaiveDate) {
    (
        chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
    )
}

fn sentinel_config(account: CalendarAccount) -> CalendarConfig {
    CalendarConfig {
        accounts: vec![account],
        ..Default::default()
    }
}

fn record_error(name: &str, error: &CalendarError, rows: &mut Vec<(String, String)>) {
    let text = format!("{error} {error:?}");
    rows.push((name.to_string(), text));
}

/// Positive control: HTTP failures keep URL and credential values out of both
/// Display and Debug, even when the response itself contains those sentinels.
#[tokio::test]
async fn calendar_errors_never_echo_url_or_credential_sentinels() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().fallback(any(|request: Request| async move {
                let path = request.uri().path();
                let mut response = match path {
                    "/unauthorized" => {
                        (StatusCode::UNAUTHORIZED, Body::empty()).into_response()
                    }
                    "/server-error" => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "url-sentinel-2b8 user-sentinel-5d0 token-sentinel-4e6",
                    )
                        .into_response(),
                    "/redirect" => {
                        let mut response = (StatusCode::FOUND, Body::empty()).into_response();
                        response.headers_mut().insert(
                            header::LOCATION,
                            HeaderValue::from_static(
                                "http://127.0.0.1:1/x?sig=url-sentinel-2b8",
                            ),
                        );
                        response
                    }
                    "/html" => {
                        let mut response = (
                            StatusCode::OK,
                            "url-sentinel-2b8 user-sentinel-5d0 token-sentinel-4e6",
                        )
                            .into_response();
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/html"),
                        );
                        response
                    }
                    "/malformed" => {
                        let mut response = (
                            StatusCode::OK,
                            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:token-sentinel-4e6\r\n",
                        )
                            .into_response();
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/calendar"),
                        );
                        response
                    }
                    "/malformed-dav" => {
                        let mut response = (
                            StatusCode::MULTI_STATUS,
                            r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/token-sentinel-4e6</d:href>"#,
                        )
                            .into_response();
                        response.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/xml"),
                        );
                        response
                    }
                    _ => (StatusCode::NOT_FOUND, Body::empty()).into_response(),
                };
                if path == "/server-error" {
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/plain"),
                    );
                }
                response
            })),
        )
        .await
    });

    let mut rows = Vec::new();
    let (from, to) = window();
    for (provider, path) in [
        (CalendarProviderKind::IcsUrl, "/unauthorized"),
        (CalendarProviderKind::IcsUrl, "/server-error"),
        (CalendarProviderKind::IcsUrl, "/redirect"),
        (CalendarProviderKind::IcsUrl, "/html"),
        (CalendarProviderKind::IcsUrl, "/malformed"),
        (CalendarProviderKind::CalDav, "/unauthorized"),
        (CalendarProviderKind::CalDav, "/server-error"),
        (CalendarProviderKind::CalDav, "/redirect"),
        (CalendarProviderKind::CalDav, "/html"),
        (CalendarProviderKind::CalDav, "/malformed-dav"),
    ] {
        let mut source = account("sentinel", provider);
        source.url = format!("http://{address}{path}?sig=url-sentinel-2b8");
        source.token = "token-sentinel-4e6".into();
        source.username = "user-sentinel-5d0".into();
        source.allow_private_network = true;
        let name = format!("{provider:?}:{path}");
        let results = router(&sentinel_config(source))
            .list_events(from, to, &BTreeMap::new())
            .await;
        assert_eq!(results.len(), 1, "{name}");
        match results[0].result.as_ref() {
            Err(error) => record_error(&name, error, &mut rows),
            Ok(_) => panic!("{name} unexpectedly succeeded"),
        }
    }

    let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unused_address = unused.local_addr().unwrap();
    drop(unused);
    let mut unreachable = account("unreachable", CalendarProviderKind::IcsUrl);
    unreachable.url = format!("http://{unused_address}/feed?sig=url-sentinel-2b8");
    unreachable.token = "token-sentinel-4e6".into();
    unreachable.allow_private_network = true;
    let result = router(&sentinel_config(unreachable))
        .list_events(from, to, &BTreeMap::new())
        .await;
    assert_eq!(result.len(), 1, "unreachable");
    match result[0].result.as_ref() {
        Err(error) => record_error("unreachable", error, &mut rows),
        Ok(_) => panic!("unreachable unexpectedly succeeded"),
    }

    let invalid = CalendarAccount {
        url: "https://user-sentinel-5d0:token-sentinel-4e6@calendar.example/feed?sig=url-sentinel-2b8".into(),
        username: "user-sentinel-5d0".into(),
        token: "token-sentinel-4e6".into(),
        ..account("invalid", CalendarProviderKind::IcsUrl)
    };
    let result = router(&sentinel_config(invalid))
        .list_events(from, to, &BTreeMap::new())
        .await;
    assert_eq!(result.len(), 1, "invalid URL");
    match result[0].result.as_ref() {
        Err(error) => record_error("invalid-url", error, &mut rows),
        Ok(_) => panic!("invalid URL unexpectedly succeeded"),
    }

    let unresolved = CalendarAccount {
        url: format!("http://{address}/unauthorized?sig=url-sentinel-2b8"),
        username: "user-sentinel-5d0".into(),
        token: "env:THEGN_TEST_CALENDAR_SENTINEL_UNSET_6a2".into(),
        allow_private_network: true,
        ..account("unresolved", CalendarProviderKind::IcsUrl)
    };
    let result = router(&sentinel_config(unresolved))
        .list_events(from, to, &BTreeMap::new())
        .await;
    assert_eq!(result.len(), 1, "unresolved token");
    match result[0].result.as_ref() {
        Err(error) => record_error("unresolved-token", error, &mut rows),
        Ok(_) => panic!("unresolved token unexpectedly succeeded"),
    }

    server.abort();
    assert!(server.await.is_err());
    assert_eq!(rows.len(), 13, "{rows:?}");
    for (name, text) in &rows {
        for sentinel in [
            "url-sentinel-2b8",
            "user-sentinel-5d0",
            "token-sentinel-4e6",
        ] {
            assert!(
                !text.contains(sentinel),
                "{name} leaked {sentinel}: {rows:?}"
            );
        }
    }
}

/// Positive control: concurrent account fetches and a rebuilt router bind each
/// request to the credential configured for that account at construction.
#[tokio::test]
async fn concurrent_accounts_each_send_only_their_own_credential() {
    use std::sync::Mutex;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let observed = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let capture = Arc::clone(&observed);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().fallback(any(move |request: Request| {
                let capture = Arc::clone(&capture);
                async move {
                    let auth = request
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("<missing>")
                        .to_string();
                    capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((request.uri().path().to_string(), auth));
                    let mut response = (
                        StatusCode::OK,
                        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n",
                    )
                        .into_response();
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/calendar"),
                    );
                    response
                }
            })),
        )
        .await
    });

    let (from, to) = window();
    let build_config = |a_token: &str, b_token: &str| CalendarConfig {
        accounts: vec![
            CalendarAccount {
                url: format!("http://{address}/a"),
                token: a_token.into(),
                allow_private_network: true,
                ..account("a", CalendarProviderKind::IcsUrl)
            },
            CalendarAccount {
                url: format!("http://{address}/b"),
                token: b_token.into(),
                allow_private_network: true,
                ..account("b", CalendarProviderKind::IcsUrl)
            },
        ],
        ..Default::default()
    };

    for (a_token, b_token) in [("token-a", "token-b"), ("token-b", "token-a")] {
        let cfg = build_config(a_token, b_token);
        let results = router(&cfg).list_events(from, to, &BTreeMap::new()).await;
        assert!(results.iter().all(|row| row.result.is_ok()));
    }
    server.abort();
    assert!(server.await.is_err());

    let records = observed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let expected = [
        ("/a", "Bearer token-a"),
        ("/b", "Bearer token-b"),
        ("/a", "Bearer token-b"),
        ("/b", "Bearer token-a"),
    ];
    assert_eq!(records.len(), expected.len(), "{records:?}");
    for (path, authorization) in expected {
        let found: Vec<_> = records
            .iter()
            .filter(|(seen_path, _)| seen_path == path)
            .map(|(_, seen_auth)| seen_auth.as_str())
            .collect();
        let expected_auths: Vec<_> = expected
            .iter()
            .filter(|(expected_path, _)| *expected_path == path)
            .map(|(_, expected_auth)| *expected_auth)
            .collect();
        assert_eq!(found.len(), 2, "{records:?}");
        assert!(expected_auths.contains(&authorization));
        assert!(found.contains(&authorization), "{path}: {records:?}");
    }
}
