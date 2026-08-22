use super::*;
use thegn_core::config_calendar::{CalendarAccount, CalendarConfig, CalendarProviderKind};

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
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An ICS backend reading `path` (a file or a vdir).
fn ics_backend(path: &str) -> ics::IcsBackend {
    ics::IcsBackend::new(&CalendarAccount {
        path: path.into(),
        ..account("t", CalendarProviderKind::Ics)
    })
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
    assert!(!CalendarError::Unsupported("create").is_transient());
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

// --- the ics backend --------------------------------------------------------

#[test]
fn an_ics_file_is_parsed() {
    let t = Tmp::new("file");
    let f = t.0.join("work.ics");
    std::fs::write(&f, ONE_EVENT).unwrap();
    let (from, to) = window();
    let page = block_on(ics_backend(&f.display().to_string()).list_events(from, to, "")).unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].title, "Standup");
    assert!(!page.partial);
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
    let mut titles: Vec<_> = page.events.iter().map(|e| e.title.clone()).collect();
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
    let r = CalendarRouter::from_config(&CalendarConfig::default());
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
    assert!(CalendarRouter::from_config(&cfg).is_configured());
    let b = caldav::CalDavBackend::new(&CalendarAccount {
        url: "https://dav.example.com/cal/".into(),
        ..account("dav", CalendarProviderKind::CalDav)
    });
    assert_eq!(b.provider_id(), "caldav");
    // `sync-collection` gives tombstones, not just a conditional refetch — the
    // only provider here that populates `EventPage::deleted`.
    assert!(b.caps().incremental);

    // A missing url is a config problem, not a network one.
    let bare = caldav::CalDavBackend::new(&account("dav", CalendarProviderKind::CalDav));
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
    let r = CalendarRouter::from_config(&cfg);
    let (from, to) = window();
    let out = block_on(r.list_events(from, to, &BTreeMap::new()));
    assert_eq!(out.len(), 1);
    let page = out[0].result.as_ref().unwrap();
    // Identity is what makes ids globally unique across accounts.
    assert_eq!(page.events[0].source.as_str(), "ics:work");
    assert_eq!(page.events[0].id().as_str(), "ics:work/e1");
    assert_eq!(page.events[0].color, Some(thegn_core::theme::Hue::Amber));
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
    let out = block_on(CalendarRouter::from_config(&cfg).list_events(from, to, &BTreeMap::new()));
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].account, "broken");
    assert!(out[0].result.is_err());
    assert_eq!(out[1].account, "good");
    assert_eq!(out[1].result.as_ref().unwrap().events.len(), 1);
}

// --- the command (plugin) backend -------------------------------------------

fn command_account(script: &str) -> CalendarAccount {
    CalendarAccount {
        command: vec!["sh".into(), "-c".into(), script.into()],
        ..account("plug", CalendarProviderKind::Command)
    }
}

fn run_plugin(script: &str) -> Result<EventPage, CalendarError> {
    let b = command::CommandBackend::new(&command_account(script));
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
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].title, "Standup");
}

#[test]
fn the_query_window_reaches_the_plugin_as_environment() {
    // The asymmetry that makes the surface writable in shell: env in, JSON out.
    let page = run_plugin(
        r#"echo "{\"method\":\"events\",\"params\":{\"sync_token\":\"$THEGN_CAL_FROM..$THEGN_CAL_TO\"}}""#,
    )
    .unwrap();
    assert_eq!(page.sync_token, "2026-08-01..2026-08-31");
}

#[test]
fn several_event_messages_accumulate() {
    let page = run_plugin(
        r#"echo '{"method":"events","params":{"events":[{"uid":"1","title":"A","start":{"kind":"date","date":"2026-08-21"},"end":{"kind":"date","date":"2026-08-22"}}]}}'
           echo '{"method":"events","params":{"events":[{"uid":"2","title":"B","start":{"kind":"date","date":"2026-08-22"},"end":{"kind":"date","date":"2026-08-23"}}],"sync_token":"t2"}}'"#,
    )
    .unwrap();
    assert_eq!(page.events.len(), 2, "pages accumulate");
    assert_eq!(page.sync_token, "t2", "the last token wins");
}

#[test]
fn a_manifest_is_negotiated_and_a_denied_capability_is_not_fatal() {
    // A plugin asking for more than it was granted should still deliver.
    let page = run_plugin(
        r#"echo '{"method":"manifest","params":{"id":"p","name":"p","version":"1","api":"0.1.0","capabilities":["run:khal","network:evil.example.com"],"contributions":[]}}'
           echo '{"method":"events","params":{"events":[]}}'"#,
    )
    .unwrap();
    assert!(page.events.is_empty());
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
    let b = command::CommandBackend::new(&CalendarAccount {
        timeout_secs: 1,
        ..command_account("sleep 30")
    });
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
    let b = command::CommandBackend::new(&account("p", CalendarProviderKind::Command));
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
    let b = ics_url::IcsUrlBackend::new(&account("u", CalendarProviderKind::IcsUrl));
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
    let b = ics_url::IcsUrlBackend::new(&CalendarAccount {
        url: "webcal://example.com/c.ics".into(),
        ..account("u", CalendarProviderKind::IcsUrl)
    });
    assert_eq!(b.provider_id(), "ics_url");
}
