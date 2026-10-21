use super::*;

type CollectedRows = Vec<(String, String, bool)>;

fn collect(xml: &str) -> Result<(CollectedRows, String), CalendarError> {
    let mut rows = Vec::new();
    let token = walk_multistatus(xml, |row| {
        rows.push((row.href, row.ics_raw.to_string(), row.deleted));
        Ok(())
    })?;
    Ok((rows, token))
}

#[test]
fn multi_propstat_associates_each_property_with_its_status() {
    let body = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/a.ics</d:href><d:propstat><d:prop><d:getetag>"tag"</d:getetag></d:prop><d:status>HTTP/1.1 404 Missing</d:status></d:propstat><d:propstat><d:status>HTTP/1.1 200 OK</d:status><d:prop><c:calendar-data>VEVENT</c:calendar-data></d:prop></d:propstat></d:response></d:multistatus>"#;
    let (rows, _) = collect(body).unwrap();
    assert_eq!(rows, vec![("/a.ics".into(), "VEVENT".into(), false)]);
}

#[test]
fn server_error_and_unauthorized_propstats_are_errors() {
    for code in ["401 Unauthorized", "403 Forbidden", "503 Unavailable"] {
        let body = format!(
            "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/private.ics</d:href><d:propstat><d:prop><c:calendar-data>SECRET-CALENDAR</c:calendar-data></d:prop><d:status>HTTP/1.1 {code}</d:status></d:propstat></d:response></d:multistatus>"
        );
        let error = collect(&body).unwrap_err();
        assert!(matches!(error, CalendarError::Api(_)));
        assert!(!error.to_string().contains("SECRET-CALENDAR"));
    }
}

#[test]
fn missing_or_empty_calendar_data_is_an_error() {
    let missing = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/missing.ics</d:href><d:propstat><d:prop><d:getetag>"tag"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    assert!(matches!(collect(missing), Err(CalendarError::Api(_))));
    let empty = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/empty.ics</d:href><d:propstat><d:prop><c:calendar-data/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    assert!(matches!(collect(empty), Err(CalendarError::Parse(_))));
}

#[test]
fn only_a_response_level_404_is_a_tombstone() {
    let body = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/gone.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response></d:multistatus>"#;
    assert!(collect(body).unwrap().0[0].2);
    let propstat = body.replace("<d:status>HTTP/1.1 404 Not Found</d:status>", "<d:propstat><d:prop><c:calendar-data/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>");
    assert!(matches!(collect(&propstat), Err(CalendarError::Api(_))));
}

#[test]
fn malformed_inputs_fail_closed() {
    let examples = [
        "",
        "garbage",
        "<d:multistatus xmlns:d=\"DAV:\"><d:response>",
        "<d:multistatus xmlns:d=\"DAV:\"></d:wrong>",
        "<root xmlns=\"urn:wrong\"/>",
        "<d:multistatus xmlns:d=\"DAV:\"><d:response/></d:multistatus>",
        "<d:multistatus><d:response/></d:multistatus>",
        "<!DOCTYPE d:multistatus><d:multistatus xmlns:d=\"DAV:\"/>",
        "<d:multistatus xmlns:d=\"DAV:\" xmlns:x=\"urn:vendor\"><d:response><d:href>/secret</d:href><d:propstat><d:prop><x:a><x:b><x:c><x:d><x:e><x:f><x:g><x:h><x:i><x:j><x:k><x:l><x:m><x:n><x:o><x:p><x:q><x:r><x:s><x:t><x:u><x:v><x:w><x:x><x:y><x:z><x:aa><x:ab><x:ac><x:ad><x:ae><x:af><x:ag><x:ah><x:ai/></x:ah></x:ag></x:af></x:ae></x:ad></x:ac></x:ab></x:aa></x:z></x:y></x:x></x:w></x:v></x:u></x:t></x:s></x:r></x:q></x:p></x:o></x:n></x:m></x:l></x:k></x:j></x:i></x:h></x:g></x:f></x:e></x:d></x:c></x:b></x:a></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>",
        "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/secret</d:href><d:href>/again</d:href><d:status>HTTP/1.1 404 Gone</d:status></d:response></d:multistatus>",
        "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/secret</d:href><d:propstat><d:prop><c:calendar-data>private</c:calendar-data></d:prop></d:propstat></d:response></d:multistatus>",
        "<d:multistatus xmlns:d=\"DAV:\"><d:sync-token>one</d:sync-token><d:sync-token>two</d:sync-token></d:multistatus>",
        "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/secret</d:href><d:propstat><d:prop><c:calendar-data/><c:calendar-data/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>",
        "<d:multistatus xmlns:d=\"DAV:\"><d:response>private text</d:response></d:multistatus>",
    ];
    for body in examples {
        let result = collect(body);
        assert!(result.is_err(), "accepted malformed fixture: {body}");
        assert!(!result.unwrap_err().to_string().contains("secret"));
    }
}

#[test]
fn namespace_aliases_resolve_by_uri() {
    for body in [
        r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/a</d:href><d:propstat><d:prop><c:calendar-data>x</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#,
        r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:response><D:href>/a</D:href><D:propstat><D:status>HTTP/1.1 200 OK</D:status><D:prop><C:calendar-data>x</C:calendar-data></D:prop></D:propstat></D:response></D:multistatus>"#,
        r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><response><href>/a</href><propstat><prop><C:calendar-data>x</C:calendar-data></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#,
        r#"<x:multistatus xmlns:x="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><x:response><x:href>/a</x:href><x:propstat><x:prop><c:calendar-data>x</c:calendar-data></x:prop><x:status>HTTP/1.1 200 OK</x:status></x:propstat></x:response></x:multistatus>"#,
        r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/a</d:href><d:propstat><d:prop><calendar-data xmlns="urn:ietf:params:xml:ns:caldav">x</calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#,
    ] {
        assert_eq!(collect(body).unwrap().0.len(), 1);
    }
}

#[test]
fn unrelated_namespace_with_dav_local_names_is_rejected() {
    assert!(matches!(
        collect("<multistatus xmlns=\"urn:example\"><response/></multistatus>"),
        Err(CalendarError::Parse(_))
    ));
}

#[test]
fn numeric_and_named_entities_decode_without_expansion() {
    assert_eq!(decode_text("&#38;&#x26;&#x1F600;&lt;").unwrap(), "&&😀<");
    assert_eq!(decode_text("&amp;lt;").unwrap(), "&lt;");
    for raw in [
        "&#0;",
        "&#xD800;",
        "&#x110000;",
        "&#x+26;",
        "&#+38;",
        "&bogus;",
        "&amp",
    ] {
        assert!(decode_text(raw).is_err(), "accepted {raw}");
    }
}

#[test]
fn cdata_calendar_data_is_taken_literally() {
    let body = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/a</d:href><d:propstat><d:prop><c:calendar-data><![CDATA[SUMMARY:A & B <literal>]]></c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    let (rows, _) = collect(body).unwrap();
    let raw = &rows[0].1;
    assert_eq!(decode_text(raw).unwrap(), "SUMMARY:A & B <literal>");
}

#[test]
fn sync_token_is_read_only_as_a_direct_child_of_multistatus() {
    let nested = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/a</d:href><d:sync-token>nested</d:sync-token><d:status>HTTP/1.1 404 Gone</d:status></d:response></d:multistatus>"#;
    assert!(collect(nested).is_err());
    let foreign = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:x="urn:vendor"><d:response><d:href>/a</d:href><d:propstat><d:prop><x:stuff><d:sync-token>ignored</d:sync-token></x:stuff><c:calendar-data>ICS</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat><d:propstat><d:prop><d:getetag/></d:prop><d:status>HTTP/1.1 404 Missing</d:status></d:propstat></d:response></d:multistatus>"#;
    assert_eq!(collect(foreign).unwrap().1, "");
}

#[test]
fn foreign_root_rejects_wrapped_dav_responses_before_visiting() {
    let body = r#"<x:foo xmlns:x="urn:example" xmlns:d="DAV:"><d:response><d:href>/a</d:href><d:status>HTTP/1.1 404 Gone</d:status></d:response></x:foo>"#;
    let mut visits = 0;
    let result = walk_multistatus(body, |_| {
        visits += 1;
        Ok(())
    });
    assert!(matches!(result, Err(CalendarError::Parse(_))));
    assert_eq!(visits, 0);
}

#[test]
fn empty_http_reason_phrase_is_accepted() {
    let body = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/gone</d:href><d:status>HTTP/1.1 404 </d:status></d:response></d:multistatus>"#;
    assert!(collect(body).unwrap().0[0].2);
}

#[test]
fn response_after_sync_token_is_rejected() {
    let body = r#"<d:multistatus xmlns:d="DAV:"><d:sync-token>token</d:sync-token><d:response><d:href>/gone</d:href><d:status>HTTP/1.1 404 Gone</d:status></d:response></d:multistatus>"#;
    assert!(matches!(collect(body), Err(CalendarError::Parse(_))));
}

#[test]
fn response_after_response_description_is_rejected() {
    let body = r#"<d:multistatus xmlns:d="DAV:"><d:responsedescription>done</d:responsedescription><d:response><d:href>/gone</d:href><d:status>HTTP/1.1 404 Gone</d:status></d:response></d:multistatus>"#;
    assert!(matches!(collect(body), Err(CalendarError::Parse(_))));
}

#[test]
fn status_before_prop_is_accepted() {
    let body = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/a</d:href><d:propstat><d:status>HTTP/1.1 200 OK</d:status><d:prop><c:calendar-data>x</c:calendar-data></d:prop></d:propstat></d:response></d:multistatus>"#;
    assert_eq!(collect(body).unwrap().0.len(), 1);
}

#[test]
fn attribute_values_containing_gt_do_not_end_the_tag() {
    let body = r#"<d:multistatus xmlns:d="DAV:" note="x>y"><d:response><d:href>/gone</d:href><d:status>HTTP/1.1 404 Gone</d:status></d:response></d:multistatus>"#;
    assert!(collect(body).unwrap().0[0].2);
}

#[test]
fn comments_declaration_and_bom_are_skipped() {
    let body = "\u{feff}<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\"><!--safe--><d:response><d:href>/gone</d:href><d:status>HTTP/1.1 404 Gone</d:status></d:response></d:multistatus>";
    assert!(collect(body).unwrap().0[0].2);
}

#[test]
fn overlong_names_and_too_many_attributes_are_refused() {
    let long = "a".repeat(MAX_XML_NAME_BYTES + 1);
    let body = format!("<{long}/>");
    assert!(collect(&body).is_err());
    let attrs = (0..=MAX_XML_ATTRIBUTES)
        .map(|n| format!(" a{n}=\"x\""))
        .collect::<String>();
    let body = format!("<d:multistatus xmlns:d=\"DAV:\"{attrs}/>");
    assert!(collect(&body).is_err());
}

#[test]
fn sabredav_sync_collection_shape_parses() {
    let body = r#"<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:response><d:href>/x.ics</d:href><d:propstat><d:prop><cal:calendar-data>ICS</cal:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat><d:propstat><d:prop><d:getetag/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response><d:response><d:href>/gone.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response><d:sync-token>http://sabre.io/ns/sync/42</d:sync-token></d:multistatus>"#;
    let (rows, token) = collect(body).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(token, "http://sabre.io/ns/sync/42");
}

#[test]
fn radicale_default_namespace_shape_parses() {
    let body = r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><response><href>/a</href><propstat><prop><C:calendar-data>ICS</C:calendar-data></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
    assert_eq!(collect(body).unwrap().0.len(), 1);
}

#[test]
fn google_status_first_crlf_shape_parses() {
    let body = "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/a</D:href><D:propstat><D:status>HTTP/1.1 200 OK</D:status><D:prop><D:getetag>&quot;63831&quot;</D:getetag><calendar-data xmlns=\"urn:ietf:params:xml:ns:caldav\">BEGIN:VEVENT\r\nUID:a\r\nEND:VEVENT</calendar-data></D:prop></D:propstat></D:response></D:multistatus>";
    assert_eq!(collect(body).unwrap().0.len(), 1);
}
