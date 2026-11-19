use super::*;

fn one(wire: &[u8]) -> Response {
    let (response, used) = parse_response(wire).expect("parses").expect("complete");
    assert_eq!(used, wire.len(), "consumed the whole response");
    response
}

#[test]
fn reader_splits_tagged_untagged_and_continuation_lines() {
    let wire = b"* OK ready\r\nA1 OK done\r\n+ go\r\n";
    let (first, used) = parse_response(wire).unwrap().unwrap();
    assert_eq!(first.tag, Tag::Untagged);
    assert_eq!(first.body, b"OK ready");
    let (second, used2) = parse_response(&wire[used..]).unwrap().unwrap();
    assert_eq!(second.tag, Tag::Tagged("A1".into()));
    assert_eq!(status(&second).unwrap().kind, StatusKind::Ok);
    let (third, _) = parse_response(&wire[used + used2..]).unwrap().unwrap();
    assert_eq!(third.tag, Tag::Continuation);
    assert_eq!(status(&third), None);
}

#[test]
fn reader_waits_for_a_whole_line_and_a_whole_literal() {
    assert_eq!(parse_response(b"* OK rea").unwrap(), None);
    assert_eq!(
        parse_response(b"* 1 FETCH (BODY[] {10}\r\nhello").unwrap(),
        None,
        "literal not complete yet"
    );
    assert_eq!(
        parse_response(b"* 1 FETCH (BODY[] {5}\r\nhello").unwrap(),
        None,
        "the line after the literal is not complete yet"
    );
}

#[test]
fn literals_may_hold_crlf_and_braces() {
    let wire = b"* 1 FETCH (UID 7 BODY[HEADER.FIELDS (SUBJECT)] {15}\r\nSubject: a}\r\n\r\n)\r\n";
    let response = one(wire);
    let parsed = fetch(&response).unwrap().unwrap();
    assert_eq!(parsed.uid, Some(7));
    assert_eq!(parsed.header.as_deref(), Some(&b"Subject: a}\r\n\r\n"[..]));
}

#[test]
fn oversize_literal_is_a_body_limit_before_it_is_buffered() {
    let wire = format!("* 1 FETCH (BODY[] {{{}}}\r\n", MAX_LITERAL + 1);
    assert_eq!(
        parse_response(wire.as_bytes()),
        Err(ProtoError::BodyLimit("literal"))
    );
    assert_eq!(
        parse_response(b"* 1 FETCH (BODY[] {99999999999999999999}\r\n"),
        Err(ProtoError::BodyLimit("literal"))
    );
    // At the cap is fine (just incomplete).
    let wire = format!("* 1 FETCH (BODY[] {{{MAX_LITERAL}}}\r\n");
    assert_eq!(parse_response(wire.as_bytes()), Ok(None));
}

#[test]
fn oversize_line_is_a_body_limit_even_without_a_crlf() {
    let long = vec![b'a'; MAX_LINE + 1];
    assert_eq!(parse_response(&long), Err(ProtoError::BodyLimit("line")));
    let mut line = b"* OK ".to_vec();
    line.extend(vec![b'a'; MAX_LINE]);
    line.extend_from_slice(b"\r\n");
    assert_eq!(parse_response(&line), Err(ProtoError::BodyLimit("line")));
}

#[test]
fn many_literals_hit_the_response_cap() {
    let mut wire = Vec::new();
    let chunk = vec![b'x'; MAX_LITERAL];
    while wire.len() <= MAX_RESPONSE {
        wire.extend_from_slice(format!("x {{{MAX_LITERAL}}}\r\n").as_bytes());
        wire.extend_from_slice(&chunk);
    }
    let mut response = b"* 1 FETCH (".to_vec();
    response.extend(wire);
    assert_eq!(
        parse_response(&response),
        Err(ProtoError::BodyLimit("response"))
    );
}

#[test]
fn lexer_rejects_hostile_nesting_and_garbage() {
    let deep = format!(
        "1 FETCH {}{}",
        "(".repeat(MAX_DEPTH + 2),
        ")".repeat(MAX_DEPTH + 2)
    );
    assert!(parse_values(deep.as_bytes()).is_err());
    assert!(parse_values(b"1 FETCH (UID 1").is_err());
    assert!(parse_values(b"1 FETCH (UID 1))").is_err());
    assert!(parse_values(b"\"unterminated").is_err());
    assert!(parse_values(b"{5}\r\nab").is_err(), "truncated literal");
}

#[test]
fn search_and_uidvalidity_and_capability() {
    let r = one(b"* SEARCH 4 9 12\r\n");
    assert_eq!(search(&r).unwrap().unwrap(), vec![4, 9, 12]);
    assert_eq!(
        search(&one(b"* SEARCH\r\n")).unwrap().unwrap(),
        Vec::<u32>::new()
    );
    assert!(search(&one(b"* SEARCH 4 x\r\n")).unwrap().is_err());
    assert!(search(&one(b"* SEARCH 0\r\n")).unwrap().is_err());
    assert!(search(&one(b"* OK fine\r\n")).is_none());

    assert_eq!(
        uidvalidity(&one(b"* OK [UIDVALIDITY 3857529045] UIDs valid\r\n")),
        Some(3_857_529_045)
    );
    assert_eq!(uidvalidity(&one(b"* OK [UIDNEXT 4] next\r\n")), None);
    assert_eq!(uidvalidity(&one(b"A1 OK [UIDVALIDITY 5] tagged\r\n")), None);

    let caps = capabilities(&one(b"* CAPABILITY IMAP4rev1 SASL-IR auth=plain\r\n")).unwrap();
    assert_eq!(caps, ["IMAP4REV1", "SASL-IR", "AUTH=PLAIN"]);
    let caps = capabilities(&one(b"* OK [CAPABILITY IMAP4rev1 LOGINDISABLED] hi\r\n")).unwrap();
    assert!(caps.contains(&"LOGINDISABLED".to_owned()));
}

#[test]
fn fetch_parses_uid_flags_internaldate_and_header() {
    let header = "From: Ann <ann@example.com>\r\nSubject: Hi\r\n\r\n";
    let wire = format!(
        "* 3 FETCH (UID 42 FLAGS (\\Seen $Label) INTERNALDATE \" 7-Jul-1996 02:44:25 -0700\" \
         BODY[HEADER.FIELDS (FROM SUBJECT MESSAGE-ID)] {{{}}}\r\n{header})\r\n",
        header.len()
    );
    let parsed = fetch(&one(wire.as_bytes())).unwrap().unwrap();
    assert_eq!(parsed.seq, 3);
    assert_eq!(parsed.uid, Some(42));
    assert_eq!(parsed.flags, ["\\Seen", "$Label"]);
    // 1996-07-07T09:44:25Z
    assert_eq!(parsed.internal_date, Some(836_732_665));
    assert_eq!(parsed.header.as_deref(), Some(header.as_bytes()));

    // A flag-only unsolicited FETCH has no header.
    let parsed = fetch(&one(b"* 4 FETCH (FLAGS (\\Seen))\r\n"))
        .unwrap()
        .unwrap();
    assert_eq!((parsed.uid, parsed.header), (None, None));
    assert!(fetch(&one(b"* 4 FETCH (UID)\r\n")).unwrap().is_err());
    assert!(fetch(&one(b"* 4 EXISTS\r\n")).is_none());
}

#[test]
fn internal_date_rejects_junk() {
    assert_eq!(
        parse_internal_date("17-Jul-1996 02:44:25 +0000"),
        Some(837_571_465)
    );
    for junk in [
        "",
        "17-Foo-1996 02:44:25 +0000",
        "31-Feb-1996 02:44:25 +0000",
        "17-Jul-1996 02:44:25 0700",
    ] {
        assert_eq!(parse_internal_date(junk), None, "{junk}");
    }
}

#[test]
fn headers_unfold_and_keep_the_first_field() {
    let raw = b"Subject: a long\r\n  folded subject\r\nFROM: \"Doe, Jane\"\r\n\t<jane@example.com>\r\nSubject: second\r\nMessage-ID: <abc@host>\r\n\r\nX-After: ignored\r\n";
    let h = parse_headers(raw);
    assert_eq!(h.subject.as_deref(), Some("a long  folded subject"));
    assert_eq!(h.from.as_deref(), Some("\"Doe, Jane\"\t<jane@example.com>"));
    assert_eq!(h.message_id.as_deref(), Some("abc@host"));
    assert_eq!(sender_display(h.from.as_deref().unwrap()), "Doe, Jane");
}

#[test]
fn encoded_words_b_q_and_mixed() {
    assert_eq!(decode_words("=?UTF-8?B?w6l0w6k=?="), "été");
    assert_eq!(
        decode_words("=?utf-8?q?caf=C3=A9_au_lait?="),
        "café au lait"
    );
    assert_eq!(decode_words("=?ISO-8859-1?Q?caf=E9?="), "café");
    assert_eq!(decode_words("=?us-ascii?Q?plain?="), "plain");
    // Whitespace between adjacent encoded words is dropped; elsewhere kept.
    assert_eq!(
        decode_words("Re: =?UTF-8?B?w6k=?= =?UTF-8?Q?t=C3=A9?= done"),
        "Re: été done"
    );
    // Unpadded base64 still decodes.
    assert_eq!(decode_words("=?UTF-8?B?w6l0w6k?="), "été");
    // Unknown charset, bad encoding, bad bytes: left raw.
    assert_eq!(decode_words("=?koi8-r?B?w6k=?="), "=?koi8-r?B?w6k=?=");
    assert_eq!(decode_words("=?UTF-8?X?abc?="), "=?UTF-8?X?abc?=");
    assert_eq!(decode_words("=?us-ascii?Q?=E9?="), "=?us-ascii?Q?=E9?=");
    assert_eq!(decode_words("=?UTF-8?Q?bad=Z?="), "=?UTF-8?Q?bad=Z?=");
    assert_eq!(decode_words("no words"), "no words");
}

#[test]
fn sender_display_prefers_the_name_then_the_address() {
    assert_eq!(sender_display("=?UTF-8?Q?Ren=C3=A9e?= <r@x.org>"), "Renée");
    assert_eq!(sender_display("\"=?UTF-8?B?w6l0w6k=?=\" <e@x.org>"), "été");
    assert_eq!(sender_display("<only@x.org>"), "only@x.org");
    assert_eq!(sender_display("bare@x.org (Comment)"), "bare@x.org");
    assert_eq!(sender_display("\"Q \\\"T\\\"\" <q@x.org>"), "Q \"T\"");
}

#[test]
fn quoting_refuses_crlf_and_nul_and_escapes() {
    assert_eq!(quote("a\"b\\c").unwrap(), "\"a\\\"b\\\\c\"");
    for bad in ["a\r\nA2 STORE 1 +FLAGS (\\Deleted)", "a\nb", "a\rb", "a\0b"] {
        assert_eq!(quote(bad).err(), Some(ProtoError::Unquotable), "{bad:?}");
        assert!(cmd_login("A1", "u", bad).is_err());
        assert!(cmd_login("A1", bad, "p").is_err());
        assert!(cmd_authenticate_plain("A1", "u", bad).is_err());
        assert!(cmd_examine("A1", bad).is_err());
    }
}

#[test]
fn builders_emit_only_read_only_commands() {
    let since = chrono::NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
    let all = [
        cmd_capability("A1").to_vec(),
        cmd_login("A2", "me", "pw").unwrap().to_vec(),
        cmd_authenticate_plain("A2", "me", "pw").unwrap().to_vec(),
        cmd_examine("A3", "INBOX").unwrap().to_vec(),
        cmd_uid_search_unseen("A4").to_vec(),
        cmd_uid_search_since("A4", since).to_vec(),
        cmd_uid_fetch("A5", &[1, 2]).unwrap().to_vec(),
        cmd_logout("A6").to_vec(),
    ];
    for cmd in &all {
        let text = String::from_utf8(cmd.clone()).unwrap();
        assert!(
            text.ends_with("\r\n") && text.matches("\r\n").count() == 1,
            "{text:?}"
        );
        let upper = text.to_ascii_uppercase();
        for verb in ["SELECT", "STORE", "EXPUNGE", "COPY", "MOVE", "BODY["] {
            assert!(!upper.contains(verb), "{verb} in {text:?}");
        }
    }
    assert_eq!(
        String::from_utf8(cmd_uid_search_since("A4", since).to_vec()).unwrap(),
        "A4 UID SEARCH SINCE 4-Nov-2026\r\n"
    );
    assert_eq!(
        String::from_utf8(cmd_uid_fetch("A5", &[1, 2]).unwrap().to_vec()).unwrap(),
        "A5 UID FETCH 1,2 (UID FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT MESSAGE-ID)])\r\n"
    );
    assert_eq!(
        String::from_utf8(cmd_authenticate_plain("A2", "me", "pw").unwrap().to_vec()).unwrap(),
        "A2 AUTHENTICATE PLAIN AG1lAHB3\r\n"
    );
    assert_eq!(
        cmd_uid_fetch("A5", &vec![1; MAX_UIDS + 1]).err(),
        Some(ProtoError::BodyLimit("uids"))
    );
    assert!(cmd_uid_fetch("A5", &[]).is_err());
}

#[test]
fn status_code_word_is_scrubbed() {
    let st = status(&one(
        b"A2 NO [AUTHENTICATIONFAILED] Invalid credentials hunter2\r\n",
    ))
    .unwrap();
    assert_eq!(st.kind, StatusKind::No);
    assert_eq!(st.code_word().as_deref(), Some("AUTHENTICATIONFAILED"));
    let st = status(&one(b"A2 NO [\x1b[31mX\x07 y] z\r\n")).unwrap();
    assert_eq!(st.code_word().as_deref(), Some("31MX"));
}

#[test]
fn percent_encoding_keeps_message_ids_link_safe() {
    assert_eq!(percent_encode("abc.1@host"), "abc.1@host");
    assert_eq!(percent_encode("a b#c/\"<>"), "a%20b%23c%2F%22%3C%3E");
}

#[test]
fn hostile_encoded_word_run_stays_raw_and_bounded() {
    let hostile = "=?a?Q? ".repeat(10_000);
    assert_eq!(decode_words(&hostile), hostile);
}
