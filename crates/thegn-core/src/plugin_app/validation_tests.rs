//! Public contract boundaries, including the rich-widget success paths.
use super::*;
use serde_json::{Value, json};

fn view(root: Value) -> AppView {
    serde_json::from_value(json!({"revision": 1, "state_scope": "account", "root": root})).unwrap()
}
fn text(id: &str) -> Value {
    json!({"kind":"text","id":id,"title":"Title","text":"Text\nwith tabs\tand 漢"})
}
fn action() -> Value {
    json!({"id":"save","label":"Save","binding":{"target":{"task":"captured-a"},"precondition":"etag-1"}})
}

#[test]
fn rich_widgets_validate_and_retain_routing_identity() {
    let roots = [
        text("text"),
        json!({"kind":"list","id":"list","title":"List","items":[{"id":"a","label":"Item"}]}),
        json!({"kind":"form","id":"form","instance":"edit-1","binding":{"target":{"task":"a"}},"title":"Edit","submit":"Save","fields":[{"id":"project","label":"Project","kind":"choice","choices":[{"id":"none","label":"None","value":""}]}]}),
        json!({"kind":"actions","id":"actions","actions":[action()]}),
        json!({"kind":"confirm","id":"confirm","message":"Delete?","accept":"Delete","binding":{"target":{"task":"a"}}}),
        json!({"kind":"table","id":"table","title":"Table","columns":[{"id":"name","label":"Name","width":20,"sortable":true}],"rows":[{"id":"a","cells":["One"],"binding":{"target":{"task":"a"}}}],"actions":[action()]}),
        json!({"kind":"tree","id":"tree","title":"Tree","items":[{"id":"a","label":"Parent","parent":null},{"id":"b","label":"Child","parent":"a"}],"actions":[action()]}),
        json!({"kind":"progress","id":"progress","label":"Sync","current":1,"total":2,"cancel":action()}),
        json!({"kind":"presentation","id":"presentation","title":"Offline","message":"Cached data","state":"offline","actions":[action()]}),
        json!({"kind":"grid","id":"grid","width":4,"height":2,"cells":[{"x":0,"y":0,"span":{"text":"漢","role":"Default"}}],"cursor":[2,1]}),
    ];
    for root in roots {
        let candidate = view(root.clone());
        candidate.validate().unwrap();
        assert_eq!(candidate.root.id(), root["id"].as_str().unwrap());
        assert_eq!(candidate.root.child_widgets().count(), 0);
        assert_eq!(
            serde_json::from_value::<Widget>(root).unwrap(),
            candidate.root
        );
    }
    for weighted in [false, true] {
        let root = if weighted {
            json!({"kind":"weighted_split","id":"root","children":[{"widget":text("leaf"),"weight":1,"min_size":2}]})
        } else {
            json!({"kind":"split","id":"root","children":[text("leaf")]})
        };
        let candidate = view(root);
        candidate.validate().unwrap();
        assert_eq!(candidate.root.child_widgets().next().unwrap().id(), "leaf");
    }
}

#[test]
fn invalid_widget_constraints_fail_before_rendering() {
    let cases = [
        json!({"kind":"weighted_split","id":"root","children":[]}),
        json!({"kind":"weighted_split","id":"root","children":[{"widget":text("leaf"),"weight":0,"min_size":1}]}),
        json!({"kind":"weighted_split","id":"root","children":[{"widget":text("leaf"),"weight":1,"min_size":4097}]}),
        json!({"kind":"table","id":"table","title":"","columns":[],"rows":[]}),
        json!({"kind":"tree","id":"tree","title":"","items":[{"id":"a","label":"","parent":"missing"}]}),
        json!({"kind":"tree","id":"tree","title":"","items":[{"id":"a","label":"","parent":"b"},{"id":"b","label":"","parent":"a"}]}),
        json!({"kind":"progress","id":"p","label":"","current":3,"total":2,"cancel":null}),
        json!({"kind":"actions","id":"a","actions":[action(),action()]}),
        json!({"kind":"presentation","id":"a","title":"","message":"","state":"error","actions":[{"id":"","label":"Bad"}]}),
        json!({"kind":"form","id":"f","instance":"bad\u{1b}","title":"","submit":"","fields":[]}),
        json!({"kind":"confirm","id":"c","message":"","accept":"Delete","binding":{"precondition":"bad\u{1b}"}}),
        json!({"kind":"confirm","id":"c","message":"Delete?","accept":""}),
        json!({"kind":"confirm","id":"c","message":"Delete?","accept":"x".repeat(129)}),
    ];
    for root in cases {
        assert!(view(root.clone()).validate().is_err(), "{root}");
    }
    let mut invalid = view(text("text"));
    invalid.state_scope = "bad\u{1b}".into();
    assert!(invalid.validate().is_err());
    invalid.state_scope = "ok".into();
    invalid.status = "bad\u{1b}".into();
    assert!(invalid.validate().is_err());
    invalid.status.clear();
    if let Widget::Text { text, .. } = &mut invalid.root {
        *text = "bad\u{1b}".into();
    }
    assert!(invalid.validate().is_err());
}

#[test]
fn bindings_and_action_metadata_are_bounded() {
    let mut binding = InteractionBinding::default();
    assert!(valid_binding(&binding));
    binding.target.insert("task".into(), "x".repeat(1025));
    assert!(!valid_binding(&binding));
    binding.target.clear();
    binding.precondition = Some("x".repeat(4097));
    assert!(!valid_binding(&binding));
    binding.precondition = None;
    binding.target = (0..17)
        .map(|i| (format!("key{i}"), "value".into()))
        .collect();
    assert!(!valid_binding(&binding));
    let mut a: Action = serde_json::from_value(action()).unwrap();
    a.disabled_reason = "bad\u{1b}".into();
    assert!(!valid_actions(&[a]));
}

#[test]
fn every_typed_event_checks_identity_and_delivery_class() {
    let cases = [
        (
            json!({"event":"action","data":{"widget":"actions","id":"save"}}),
            true,
        ),
        (
            json!({"event":"activate","data":{"widget":"list","id":"task"}}),
            true,
        ),
        (
            json!({"event":"submit","data":{"widget":"form","instance":"edit-1","values":{"title":"Draft"},"draft_version":2}}),
            true,
        ),
        (
            json!({"event":"cancel","data":{"widget":"form","instance":"edit-1"}}),
            true,
        ),
        (
            json!({"event":"change","data":{"widget":"form","instance":"edit-1","values":{"title":"Draft"},"draft_version":2}}),
            false,
        ),
        (
            json!({"event":"select","data":{"widget":"table","items":["a"],"current":"a"}}),
            false,
        ),
        (
            json!({"event":"key","data":{"widget":"grid","key":{"name":"character","value":"漢"}}}),
            false,
        ),
        (
            json!({"event":"key","data":{"widget":"grid","key":{"name":"function","value":2}}}),
            false,
        ),
        (
            json!({"event":"key","data":{"widget":"grid","key":{"name":"enter"}}}),
            false,
        ),
        (
            json!({"event":"paste","data":{"widget":"grid","text":"hello"}}),
            false,
        ),
        (
            json!({"event":"resize","data":{"widget":"grid","width":80,"height":24}}),
            false,
        ),
        (json!({"event":"focus","data":{"focused":true}}), false),
        (json!({"event":"visibility","data":{"visible":true}}), false),
        (json!({"event":"invoke","data":{"id":"open"}}), true),
        (
            json!({"event":"sort","data":{"widget":"table","column":"title","descending":false}}),
            true,
        ),
        (json!({"event":"closed"}), false),
    ];
    for (wire, reliable) in cases {
        let event: AppEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(event.reliable(), reliable);
        let mut params = AppEventParams {
            surface: "app.workspace".into(),
            session: "session".into(),
            state_scope: "account".into(),
            revision: 1,
            focus_ticket: None,
            event,
        };
        params.validate().unwrap();
        params.session = "bad\u{1b}".into();
        assert!(params.validate().is_err());
    }
}

#[test]
fn event_data_rejects_control_sequences_duplicates_and_oversized_edits() {
    let cases = [
        json!({"event":"action","data":{"widget":"actions","id":""}}),
        json!({"event":"submit","data":{"widget":"form","instance":"edit-1","values":{"title":"x".repeat(32769)},"draft_version":1}}),
        json!({"event":"change","data":{"widget":"form","instance":"edit-1","values":{"title":"bad\u{1b}"},"draft_version":1}}),
        json!({"event":"cancel","data":{"widget":"form","instance":"bad\u{1b}"}}),
        json!({"event":"select","data":{"widget":"table","items":["a","a"],"current":"a"}}),
        json!({"event":"key","data":{"widget":"grid","key":{"name":"function","value":0}}}),
        json!({"event":"key","data":{"widget":"grid","key":{"name":"character","value":"\u{1b}"}}}),
        json!({"event":"paste","data":{"widget":"grid","text":"\u{1b}"}}),
        json!({"event":"sort","data":{"widget":"table","column":"","descending":false}}),
    ];
    for wire in cases {
        let event = serde_json::from_value(wire).unwrap();
        let params = AppEventParams {
            surface: "app.workspace".into(),
            session: "session".into(),
            state_scope: "account".into(),
            revision: 1,
            focus_ticket: None,
            event,
        };
        assert!(params.validate().is_err());
    }
}

#[test]
fn focus_tickets_reject_empty_controlled_and_oversized_values() {
    let mut params = AppEventParams {
        surface: "app.workspace".into(),
        session: "session".into(),
        state_scope: "account".into(),
        revision: 1,
        focus_ticket: Some(String::new()),
        event: AppEvent::Focus { focused: true },
    };
    assert!(params.validate().is_err(), "empty focus ticket");
    params.focus_ticket = Some("ticket\u{1b}".into());
    assert!(
        params.validate().is_err(),
        "control character in focus ticket"
    );
    params.focus_ticket = Some("t".repeat(257));
    assert!(params.validate().is_err(), "oversized focus ticket");
}
