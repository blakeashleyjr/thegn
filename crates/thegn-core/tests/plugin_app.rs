//! Adversarial wire frames must fail before reaching the synchronous renderer.
use serde_json::{Value, json};
use thegn_core::plugin_app::AppView;
fn valid(root: Value) -> bool {
    serde_json::from_value::<AppView>(json!({"revision":1,"status":"","root":root}))
        .unwrap()
        .validate()
        .is_ok()
}
fn text(id: &str) -> Value {
    json!({"kind":"text","id":id,"title":"","text":"content"})
}
#[test]
fn widget_ids_and_nesting_are_bounded() {
    assert!(!valid(text("")));
    assert!(!valid(text(&"a".repeat(129))));
    assert!(!valid(
        json!({"kind":"split","id":"root","children":[text("same"),text("same")]})
    ));
    let mut root = text("leaf");
    for i in 0..14 {
        root = json!({"kind":"split","id":format!("split-{i}"),"children":[root]});
    }
    assert!(!valid(root));
    assert!(!valid(
        json!({"kind":"split","id":"root","children":(0..2049).map(|i|text(&i.to_string())).collect::<Vec<_>>()})
    ));
}
#[test]
fn wide_grid_cells_cannot_escape_or_overlap_the_frame() {
    let mut root = json!({"kind":"grid","id":"grid","width":4,"height":2,"cells":[{"x":2,"y":0,"span":{"text":"漢","role":"Default"}}],"cursor":null});
    assert!(valid(root.clone()));
    root["cells"][0]["x"] = json!(3);
    assert!(!valid(root.clone()));
    root["cells"][0]["x"] = json!(2);
    root["cells"]
        .as_array_mut()
        .unwrap()
        .push(json!({"x":3,"y":0,"span":{"text":"x","role":"Default"}}));
    assert!(!valid(root));
}
#[test]
fn grid_control_sequences_and_invalid_cursors_are_rejected() {
    let mut root = json!({"kind":"grid","id":"grid","width":4,"height":2,"cells":[{"x":0,"y":0,"span":{"text":"\u{1b}[31m","role":"Default"}}],"cursor":null});
    assert!(!valid(root.clone()));
    root["cells"] = json!([]);
    root["cursor"] = json!([4, 0]);
    assert!(!valid(root.clone()));
    root["cursor"] = Value::Null;
    root["width"] = json!(513);
    assert!(!valid(root));
}
#[test]
fn form_fields_and_list_items_have_identity_and_size_limits() {
    let field = json!({"id":"title","label":"Title","value":""});
    assert!(valid(
        json!({"kind":"form","id":"edit","title":"Edit","submit":"Save","fields":[field.clone()]})
    ));
    assert!(!valid(
        json!({"kind":"form","id":"edit","title":"Edit","submit":"Save","fields":[field.clone(),field]})
    ));
    assert!(!valid(
        json!({"kind":"list","id":"list","title":"Tasks","items":[{"id":"a","label":"A"},{"id":"a","label":"Duplicate"}]})
    ));
    assert!(!valid(
        json!({"kind":"list","id":"list","title":"Tasks","items":(0..4097).map(|i|json!({"id":i.to_string(),"label":"Task"})).collect::<Vec<_>>()})
    ));
}

#[test]
fn tables_and_trees_reject_ambiguous_identity_and_cycles() {
    let table = json!({"kind":"table","id":"tasks","title":"Tasks","columns":[{"id":"title","label":"Title","width":null}],"rows":[{"id":"one","cells":["Task"]}]});
    assert!(valid(table.clone()));
    let mut invalid = table.clone();
    invalid["rows"][0]["cells"] = json!([]);
    assert!(!valid(invalid));
    let mut invalid = table;
    invalid["columns"][0]["width"] = json!(0);
    assert!(!valid(invalid));
    assert!(!valid(
        json!({"kind":"tree","id":"tree","title":"","items":[{"id":"a","label":"A","parent":"b"},{"id":"b","label":"B","parent":"a"}]})
    ));
    assert!(!valid(
        json!({"kind":"tree","id":"tree","title":"","items":[{"id":"a","label":"A","parent":"missing"}]})
    ));
}

#[test]
fn choice_values_are_distinct_from_option_identity() {
    let root = json!({"kind":"form","id":"edit","instance":"form-1","title":"Edit","submit":"Save","fields":[{"id":"project","label":"Project","kind":"choice","choices":[{"id":"default","label":"Inbox","value":""},{"id":"home-option","label":"Home","value":"project-abc"}]}]});
    assert!(valid(root.clone()));
    let mut duplicate = root;
    duplicate["fields"][0]["choices"][1]["id"] = json!("default");
    assert!(!valid(duplicate));
}

#[test]
fn confirmation_actions_must_be_valid_event_routing_ids() {
    let mut root =
        json!({"kind":"confirm","id":"confirmation","message":"Delete?","accept":"delete"});
    assert!(valid(root.clone()));
    for accept in [String::new(), "x".repeat(129), "delete\n".into()] {
        root["accept"] = json!(accept);
        assert!(!valid(root.clone()));
    }
}

#[test]
fn event_budgets_include_json_escaping() {
    use thegn_core::plugin_app::{AppEvent, AppEventParams, Submission};
    let mut event = AppEventParams {
        surface: "app.workspace".into(),
        session: "test".into(),
        state_scope: "account-1".into(),
        revision: 1,
        focus_ticket: None,
        event: AppEvent::Submit(Submission {
            widget: "form".into(),
            instance: "form-1".into(),
            draft_version: 1,
            binding: Default::default(),
            values: [("title".into(), "Hello".into())].into(),
        }),
    };
    assert!(event.validate().is_ok());
    if let AppEvent::Submit(s) = &mut event.event {
        s.values = (0..16)
            .map(|n| (format!("field-{n}"), "\"".repeat(32 * 1024)))
            .collect();
    }
    assert!(event.validate().is_err());
}
