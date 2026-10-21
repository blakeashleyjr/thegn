use super::*;
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use thegn_core::calendar::conformance::{FIXTURES, Verdict, ics_document};
use thegn_core::config_calendar::{CalendarAccount, CalendarProviderKind};

fn admission() -> AccountAdmission {
    AccountAdmission::isolated(AdmissionBudget::default(), thegn_core::calendar::Tz::UTC)
}

fn account(provider: CalendarProviderKind) -> CalendarAccount {
    CalendarAccount {
        name: "conformance".into(),
        provider,
        ..CalendarAccount::default()
    }
}

fn window() -> (chrono::NaiveDate, chrono::NaiveDate) {
    (
        chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
    )
}

async fn server(
    body: String,
    content_type: &'static str,
    status: StatusCode,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(any(move |_request: Request| {
                let body = body.clone();
                async move {
                    let mut response = (status, Body::from(body)).into_response();
                    response
                        .headers_mut()
                        .insert("content-type", HeaderValue::from_static(content_type));
                    response
                }
            })),
        )
        .await
        .unwrap();
    });
    (format!("http://{address}/calendar"), task)
}

fn verdict(result: &Result<EventPage, CalendarError>) -> (Verdict, &'static str) {
    match result {
        Ok(page) if page.events().len() == 1 => (Verdict::Admit, "one event"),
        Ok(_) => (Verdict::Reject, "empty page"),
        Err(_) => (Verdict::Reject, "error"),
    }
}

#[tokio::test]
#[allow(
    clippy::disallowed_macros,
    reason = "conformance matrix prints every fixture x seam verdict before asserting"
)]
async fn conformance_local_ics_file_matches_every_fixture() {
    let mut table = Vec::new();
    for fixture in FIXTURES {
        if let Some((properties, expected)) = fixture.ics {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fixture.ics");
            std::fs::write(&path, ics_document(properties)).unwrap();
            let backend = ics::IcsBackend::new(
                &CalendarAccount {
                    path: path.to_string_lossy().into_owned(),
                    ..account(CalendarProviderKind::Ics)
                },
                admission(),
            );
            let (from, to) = window();
            let result = backend.list_events(from, to, "").await;
            let (actual, shape) = verdict(&result);
            let shape_ok = match (&result, expected) {
                (Ok(page), Verdict::Admit) => page.events().len() == 1,
                (Ok(page), Verdict::Reject) => page.events().is_empty(),
                (Err(_), _) => false,
            };
            table.push((
                fixture.name,
                "local ICS file",
                expected,
                actual,
                shape,
                shape_ok,
            ));
        }
    }
    eprintln!("calendar conformance local ICS table: {table:#?}");
    assert!(
        table
            .iter()
            .all(|(_, _, expected, actual, _, shape_ok)| { expected == actual && *shape_ok }),
        "calendar conformance disagreement: {table:#?}"
    );
}

#[tokio::test]
#[allow(
    clippy::disallowed_macros,
    reason = "conformance matrix prints every fixture x seam verdict before asserting"
)]
async fn conformance_ics_url_matches_every_fixture() {
    let mut table = Vec::new();
    for fixture in FIXTURES {
        if let Some((properties, expected)) = fixture.ics {
            let (url, server) =
                server(ics_document(properties), "text/calendar", StatusCode::OK).await;
            let backend = ics_url::IcsUrlBackend::new(
                &CalendarAccount {
                    url,
                    allow_private_network: true,
                    ..account(CalendarProviderKind::IcsUrl)
                },
                admission(),
            );
            let (from, to) = window();
            let result = backend.list_events(from, to, "").await;
            let (actual, shape) = verdict(&result);
            table.push((fixture.name, "ICS URL", expected, actual, shape));
            server.abort();
            if let Err(error) = server.await {
                assert!(
                    error.is_cancelled(),
                    "calendar test server ended unexpectedly: {error}"
                );
            }
        }
    }
    eprintln!("calendar conformance ICS URL table: {table:#?}");
    assert!(
        table
            .iter()
            .all(|(_, _, expected, actual, _)| expected == actual),
        "calendar conformance disagreement: {table:#?}"
    );
}

#[tokio::test]
#[allow(
    clippy::disallowed_macros,
    reason = "conformance matrix prints every fixture x seam verdict before asserting"
)]
async fn conformance_caldav_matches_every_fixture() {
    let mut table = Vec::new();
    for fixture in FIXTURES {
        if let Some((properties, expected)) = fixture.ics {
            let document = ics_document(properties);
            let xml = format!(
                "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/calendar/event.ics</d:href><d:propstat><d:prop><c:calendar-data>{document}</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"
            );
            let (url, server) = server(xml, "application/xml", StatusCode::MULTI_STATUS).await;
            let backend = caldav::CalDavBackend::new(
                &CalendarAccount {
                    url,
                    allow_private_network: true,
                    ..account(CalendarProviderKind::CalDav)
                },
                admission(),
            );
            let (from, to) = window();
            let result = backend.list_events(from, to, "").await;
            let (actual, shape) = verdict(&result);
            table.push((fixture.name, "CalDAV", expected, actual, shape));
            server.abort();
            if let Err(error) = server.await {
                assert!(
                    error.is_cancelled(),
                    "calendar test server ended unexpectedly: {error}"
                );
            }
        }
    }
    eprintln!("calendar conformance CalDAV table: {table:#?}");
    assert!(
        table
            .iter()
            .all(|(_, _, expected, actual, _)| expected == actual),
        "calendar conformance disagreement: {table:#?}"
    );
}
