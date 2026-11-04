use super::*;
use serde_json::json;
fn form(scope: &str, instance: &str, value: &str) -> AppView {
    serde_json::from_value(json!({"state_scope":scope,"revision":1,"root":{"kind":"form","id":"edit","instance":instance,"title":"Edit","fields":[{"id":"title","label":"Title","value":value}],"submit":"Save"}})).unwrap()
}
fn ui(view: AppView) -> UiEngine {
    let mut ui = UiEngine::new(Theme::prism());
    ui.replace_view(view).unwrap();
    ui.prepare(Rect::new(5, 7, 50, 15));
    ui
}
fn ctrl(key: Key) -> InputEvent {
    InputEvent::Key {
        key,
        modifiers: Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        },
    }
}
fn value(ui: &UiEngine, scope: &str, instance: &str) -> String {
    ui.recovery_values(scope, "edit", instance).unwrap()["title"].clone()
}

#[test]
fn f2_is_not_interpreted_by_the_engine() {
    let mut ui = ui(form("a", "1", "base"));
    ui.input(InputEvent::key(Key::Function(2)));
    assert!(!ui.recovery_open());
}

#[test]
fn ascii_glyph_set_reaches_the_painter() {
    let mut ui = UiEngine::new(Theme::prism());
    ui.set_glyphs(thegn_core::termcaps::glyphs(
        thegn_core::termcaps::UnicodeLevel::Ascii,
    ));
    ui.prepare(Rect::new(0, 0, 40, 2));
    let mut buffer = Buffer::empty(ui.area);
    ui.paint(&mut buffer);
    let rendered = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Waiting for plugin..."));
    assert!(!rendered.contains('…'));
}

#[test]
fn dirty_refresh_preserves_typing_and_clean_fields_refresh() {
    let mut ui = ui(form("a", "1", "base"));
    ui.input(InputEvent::key(Key::Char('!')));
    ui.replace_view(form("a", "1", "remote")).unwrap();
    assert_eq!(value(&ui, "a", "1"), "base!");
    let mut clean = ui::UiEngine::new(Theme::prism());
    clean.replace_view(form("a", "1", "old")).unwrap();
    clean.replace_view(form("a", "1", "new")).unwrap();
    assert_eq!(value(&clean, "a", "1"), "new");
}
mod ui {
    pub use crate::UiEngine;
}
#[test]
fn account_scope_and_form_instance_isolate_drafts_and_keep_recovery() {
    let mut ui = ui(form("alice", "one", "Alice"));
    ui.input(InputEvent::key(Key::Char('!')));
    ui.replace_view(form("bob", "one", "Bob")).unwrap();
    assert_eq!(value(&ui, "bob", "one"), "Bob");
    assert_eq!(ui.recovery_count(), 1);
    ui.input(InputEvent::key(Key::Char('?')));
    ui.replace_view(form("alice", "two", "Different")).unwrap();
    assert_eq!(value(&ui, "alice", "two"), "Different");
    assert_eq!(ui.recovery_count(), 2);
    assert_eq!(value(&ui, "alice", "one"), "Alice!");
}
#[test]
fn acknowledgment_of_old_draft_never_erases_new_typing() {
    let mut ui = ui(form("a", "1", ""));
    ui.input(InputEvent::key(Key::Char('A')));
    let output = ui.input(ctrl(Key::Enter));
    let submission = output
        .intents
        .into_iter()
        .find_map(|e| {
            if let AppEvent::Submit(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();
    ui.mark_submitted("edit", "1");
    ui.input(InputEvent::key(Key::Char('B')));
    ui.acknowledge(
        "a",
        "edit",
        "1",
        submission.draft_version,
        &submission.values,
        &AppEventResult::default(),
        true,
    );
    assert_eq!(value(&ui, "a", "1"), "AB");
    assert_eq!(ui.dirty_records(), 1);
}
#[test]
fn malformed_replacement_keeps_last_good_view() {
    let mut ui = ui(form("a", "1", "kept"));
    let mut broken = form("a", "2", "other");
    if let Widget::Form { fields, .. } = &mut broken.root {
        fields.push(fields[0].clone());
    }
    assert!(ui.replace_view(broken).is_err());
    assert_eq!(ui.view().unwrap().root.id(), "edit");
    assert_eq!(value(&ui, "a", "1"), "kept");
}
#[test]
fn current_caret_respects_nonzero_origin_and_horizontal_scroll() {
    let mut ui = ui(form("a", "1", "abcdef"));
    assert_eq!(ui.cursor(), Some((21, 8)));
    ui.prepare(Rect::new(5, 7, 12, 6));
    let (x, y) = ui.cursor().unwrap();
    assert!((5..17).contains(&x) && (7..13).contains(&y));
    let mut buffer = Buffer::empty(Rect::new(5, 7, 12, 6));
    ui.paint(&mut buffer);
    assert_eq!(ui.cursor(), Some((x, y)));
}

#[test]
fn initial_zero_viewport_and_later_expansion_preserve_visible_draft_context() {
    let mut ui = UiEngine::new(Theme::prism());
    ui.replace_view(form("stable", "same", "baseline")).unwrap();
    ui.prepare(Rect::default());
    ui.prepare(Rect::new(0, 0, 80, 24));
    ui.input(InputEvent::Paste("kept".into()));
    let rendered = |ui: &UiEngine| {
        let mut buffer = Buffer::empty(ui.area);
        ui.paint(&mut buffer);
        buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    assert!(rendered(&ui).contains("baselinekept"));
    ui.prepare(Rect::new(0, 0, 16, 6));
    assert!(!rendered(&ui).contains("baselinekept"));
    ui.prepare(Rect::default());
    ui.prepare(Rect::new(0, 0, 80, 24));
    assert!(rendered(&ui).contains("baselinekept"));
    assert_eq!(value(&ui, "stable", "same"), "baselinekept");
}

#[test]
fn complete_encoded_event_budget_refuses_growth_without_truncation() {
    let mut view = form("a", "1", "");
    if let Widget::Form { fields, .. } = &mut view.root {
        *fields = (0..17)
            .map(|i| Field {
                id: format!("f{i}"),
                label: format!("F{i}"),
                value: "\"".repeat(30600),
                ..Field::default()
            })
            .collect();
    }
    let mut ui = ui(view);
    let before = ui.forms.values().next().unwrap().values();
    let outcome = ui.input(InputEvent::Paste("\"".repeat(2000)));
    assert!(
        outcome
            .intents
            .iter()
            .all(|e| !matches!(e, AppEvent::Change(_)))
    );
    assert_eq!(before, ui.forms.values().next().unwrap().values());
}
#[test]
fn recovery_record_cap_refuses_ninth_draft_and_f2_inspects() {
    let mut ui = ui(form("0", "1", ""));
    for i in 0..8 {
        if i > 0 {
            ui.replace_view(form(&i.to_string(), "1", "")).unwrap();
        }
        ui.input(InputEvent::key(Key::Char('x')));
    }
    ui.replace_view(form("8", "1", "")).unwrap();
    let out = ui.input(InputEvent::key(Key::Char('x')));
    assert!(!out.intents.iter().any(|e| matches!(e, AppEvent::Change(_))));
    assert_eq!(value(&ui, "8", "1"), "");
    ui.toggle_recovery();
    assert!(ui.recovery_open());
    let mut b = Buffer::empty(ui.area);
    ui.paint(&mut b);
    assert!(ui.status_line().unwrap().contains("limit"));
}
#[test]
fn dirty_removed_fields_remain_in_snapshot() {
    let mut view = form("a", "1", "");
    if let Widget::Form { fields, .. } = &mut view.root {
        fields.push(Field {
            id: "second".into(),
            label: "Second".into(),
            value: "seed".into(),
            ..Field::default()
        });
    }
    let mut ui = ui(view);
    ui.input(InputEvent::key(Key::Tab));
    ui.input(InputEvent::key(Key::Char('!')));
    ui.replace_view(form("a", "1", "")).unwrap();
    assert_eq!(
        ui.recovery_values("a", "edit", "1").unwrap()["second"],
        "seed!"
    );
}
#[test]
fn choice_search_matches_labels_and_returns_explicit_value() {
    let mut view = form("a", "1", "");
    if let Widget::Form { fields, .. } = &mut view.root {
        fields[0].kind = FieldKind::Choice;
        fields[0].choices = vec![
            ChoiceOption {
                id: "a".into(),
                label: "Alpha".into(),
                value: "raw-a".into(),
                disabled: false,
            },
            ChoiceOption {
                id: "b".into(),
                label: "Beta".into(),
                value: "raw-b".into(),
                disabled: false,
            },
        ];
    }
    let mut ui = ui(view);
    ui.input(InputEvent::key(Key::Enter));
    ui.input(InputEvent::key(Key::Char('b')));
    let out = ui.input(InputEvent::key(Key::Enter));
    assert!(out.intents.iter().any(|e| matches!(e, AppEvent::Change(_))));
    assert_eq!(value(&ui, "a", "1"), "raw-b");
}
#[test]
fn typed_validation_preserves_raw_intermediate_text() {
    let mut view = form("a", "1", "");
    if let Widget::Form { fields, .. } = &mut view.root {
        fields[0].kind = FieldKind::Integer;
        fields[0].required = true;
    }
    let mut ui = ui(view);
    ui.input(InputEvent::key(Key::Char('-')));
    assert_eq!(value(&ui, "a", "1"), "-");
    assert!(
        !ui.input(ctrl(Key::Enter))
            .intents
            .iter()
            .any(|e| matches!(e, AppEvent::Submit(_)))
    );
    assert!(!ui.forms.values().next().unwrap().errors.is_empty());
}
#[test]
fn weighted_splits_collapse_to_focused_branch_and_report_grid_viewport() {
    let view:AppView=serde_json::from_value(json!({"revision":1,"root":{"kind":"weighted_split","id":"root","children":[{"widget":{"kind":"text","id":"text","title":"Text","text":"hello"},"weight":1,"min_size":20},{"widget":{"kind":"grid","id":"grid","width":100,"height":20,"cells":[],"cursor":[2,2]},"weight":2,"min_size":30}]}})).unwrap();
    let mut ui = UiEngine::new(Theme::prism());
    ui.replace_view(view).unwrap();
    let first = ui.prepare(Rect::new(0, 0, 80, 10));
    assert!(
        first
            .iter()
            .any(|e| matches!(e,AppEvent::Resize{widget,width:50,height:10}if widget=="grid"))
    );
    let hidden = ui.prepare(Rect::new(0, 0, 40, 10));
    assert!(
        hidden
            .iter()
            .any(|e| matches!(e,AppEvent::Resize{widget,width:0,height:0}if widget=="grid"))
    );
    let shown = ui.input(InputEvent::key(Key::Tab));
    assert!(
        shown
            .intents
            .iter()
            .any(|e| matches!(e,AppEvent::Resize{widget,width:40,height:10}if widget=="grid"))
    );
    assert_eq!(ui.cursor(), Some((2, 2)));
}
#[test]
fn tree_expand_multiselect_and_context_action_bindings_are_stable() {
    let view:AppView=serde_json::from_value(json!({"revision":1,"root":{"kind":"tree","id":"tree","title":"Tree","multiple":true,"items":[{"id":"p","label":"Parent","parent":null,"expanded":false},{"id":"c","label":"Child","parent":"p","binding":{"target":{"entity":"captured"}}}],"actions":[{"id":"remove","label":"Remove","binding":{"target":{"entity":"original"}}}]}})).unwrap();
    let mut ui = ui(view);
    ui.input(InputEvent::key(Key::Right));
    ui.input(InputEvent::key(Key::Down));
    let selection = ui.input(InputEvent::key(Key::Char(' ')));
    assert!(
        selection
            .intents
            .iter()
            .any(|e| matches!(e,AppEvent::Select(s)if s.items==vec!["c"]))
    );
    let activate = ui.input(InputEvent::key(Key::Enter));
    assert!(
        activate
            .intents
            .iter()
            .any(|e| matches!(e,AppEvent::Activate(i)if i.binding.target["entity"]=="captured"))
    );
    ui.input(InputEvent::key(Key::Char(':')));
    let action = ui.input(InputEvent::key(Key::Enter));
    assert!(
        action
            .intents
            .iter()
            .any(|e| matches!(e,AppEvent::Action(i)if i.binding.target["entity"]=="original"))
    );
}
#[test]
fn required_field_caret_matches_marker_and_paste_rejection_is_visible() {
    let mut view = form("a", "1", &"x".repeat(FIELD_BYTES));
    if let Widget::Form { fields, .. } = &mut view.root {
        fields[0].required = true;
    }
    let mut ui = ui(view);
    let before = value(&ui, "a", "1");
    ui.input(InputEvent::Paste("one byte".into()));
    assert_eq!(value(&ui, "a", "1"), before);
    assert!(ui.status_line().unwrap().contains("32 KiB"));
    let mut view = form("b", "2", "ab");
    if let Widget::Form { fields, .. } = &mut view.root {
        fields[0].required = true;
    }
    ui.replace_view(view).unwrap();
    ui.prepare(Rect::new(5, 7, 50, 15));
    assert_eq!(ui.cursor(), Some((18, 8)));
}
#[test]
fn uppercase_ctrl_shift_z_redoes_and_scope_change_resends_viewports() {
    let mut ui = ui(form("a", "1", ""));
    ui.input(InputEvent::key(Key::Char('x')));
    ui.input(ctrl(Key::Char('z')));
    ui.input(InputEvent::Key {
        key: Key::Char('Z'),
        modifiers: Modifiers {
            ctrl: true,
            shift: true,
            alt: false,
        },
    });
    assert_eq!(value(&ui, "a", "1"), "x");
    let grid = |scope: &str| {
        serde_json::from_value::<AppView>(json!({"state_scope":scope,"revision":1,"root":{"kind":"grid","id":"g","width":20,"height":10,"cells":[],"cursor":null}})).unwrap()
    };
    ui.replace_view(grid("a")).unwrap();
    assert_eq!(ui.prepare(ui.area).len(), 1);
    assert!(ui.prepare(ui.area).is_empty());
    ui.replace_view(grid("b")).unwrap();
    assert_eq!(ui.prepare(ui.area).len(), 1);
    ui.reset_viewports();
    assert_eq!(ui.prepare(ui.area).len(), 1);
}
#[test]
fn repeated_widget_replacement_prunes_selection_and_scroll_state() {
    let mut ui = UiEngine::new(Theme::prism());
    for i in 0..100 {
        let view:AppView=serde_json::from_value(json!({"state_scope":format!("s{}",i/2),"revision":i,"root":{"kind":"list","id":format!("list{i}"),"title":"List","items":[{"id":"a","label":"A"}]}})).unwrap();
        ui.replace_view(view).unwrap();
        ui.input(InputEvent::key(Key::Down));
    }
    assert!(ui.selection.len() <= 16);
    assert!(ui.scopes.len() <= 8);
}
#[test]
fn diagnostics_are_bounded_and_cannot_inject_terminal_controls() {
    let mut ui = ui(form("a", "1", ""));
    ui.delivery_failed(format!("\x1b[31m\0{}", "界".repeat(10000)));
    let status = ui.status_line().unwrap();
    assert!(status.len() <= 8192);
    assert!(!status.chars().any(char::is_control));
    let result = AppEventResult {
        message: "\x1b[2Jbad".into(),
        field_errors: std::collections::BTreeMap::from([
            ("title".into(), "\x1b[0merr".into()),
            ("unknown".into(), "discard".into()),
        ]),
    };
    ui.acknowledge("a", "edit", "1", 0, &Default::default(), &result, false);
    assert!(!ui.status_line().unwrap().contains('\x1b'));
    let form = ui.forms.values().next().unwrap();
    assert!(!form.errors.contains_key("unknown"));
    assert!(!form.errors["title"].contains('\x1b'));
}
#[test]
fn removed_dirty_fields_are_recoverable_but_never_submitted_as_live_controls() {
    let mut view = form("a", "1", "");
    if let Widget::Form { fields, .. } = &mut view.root {
        fields.push(Field {
            id: "old".into(),
            label: "Old".into(),
            value: "old".into(),
            ..Field::default()
        });
    }
    let mut ui = ui(view);
    ui.input(InputEvent::key(Key::Tab));
    ui.input(InputEvent::key(Key::Char('!')));
    ui.replace_view(form("a", "1", "")).unwrap();
    assert_eq!(ui.recovery_count(), 1);
    let out = ui.input(ctrl(Key::Enter));
    let submission = out
        .intents
        .iter()
        .find_map(|e| {
            if let AppEvent::Submit(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();
    assert!(!submission.values.contains_key("old"));
    assert!(ui.discard_recovery("a", "edit", "1"));
    assert_eq!(value(&ui, "a", "1"), "");
    assert_eq!(ui.recovery_count(), 0);
}
#[test]
fn control_bearing_paste_is_rejected_before_editor_or_transport_changes() {
    let mut ui = ui(form("a", "1", "safe"));
    let result = ui.input(InputEvent::Paste("unsafe\x1b[2J".into()));
    assert_eq!(value(&ui, "a", "1"), "safe");
    assert!(
        !result
            .intents
            .iter()
            .any(|event| matches!(event, AppEvent::Change(_)))
    );
    assert!(ui.status_line().unwrap().contains("control characters"));
}
#[test]
fn accepted_integer_canonicalization_does_not_leave_a_false_dirty_draft() {
    let mut view = form("a", "1", "");
    if let Widget::Form { fields, .. } = &mut view.root {
        fields[0].kind = FieldKind::Integer;
    }
    let mut ui = ui(view);
    ui.input(InputEvent::Paste(" 01 ".into()));
    let output = ui.input(ctrl(Key::Enter));
    let submission = output
        .intents
        .into_iter()
        .find_map(|e| {
            if let AppEvent::Submit(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(submission.values["title"], "1");
    ui.acknowledge(
        "a",
        "edit",
        "1",
        submission.draft_version,
        &submission.values,
        &AppEventResult::default(),
        true,
    );
    assert_eq!(value(&ui, "a", "1"), "1");
    assert_eq!(ui.dirty_records(), 0);
}
#[test]
fn choice_search_and_form_editors_share_one_undo_budget() {
    let mut view = form("a", "1", "");
    if let Widget::Form { fields, .. } = &mut view.root {
        fields[0].kind = FieldKind::Choice;
        fields[0].choices = vec![ChoiceOption {
            id: "a".into(),
            label: "A".into(),
            value: "a".into(),
            disabled: false,
        }];
    }
    let mut ui = ui(view);
    ui.input(InputEvent::key(Key::Enter));
    for i in 0..100 {
        ui.input(ctrl(Key::Char('a')));
        ui.input(InputEvent::Paste(
            if i % 2 == 0 { "x" } else { "y" }.repeat(FIELD_BYTES),
        ));
        assert!(ui.history_bytes() <= HISTORY_BYTES);
    }
    assert_eq!(ui.popup.as_ref().unwrap().query.text().len(), FIELD_BYTES);
}
#[test]
fn searchable_choice_caret_matches_search_text_and_recovery_hides_it() {
    let mut view = form("a", "1", "");
    if let Widget::Form { fields, .. } = &mut view.root {
        fields[0].kind = FieldKind::Choice;
        fields[0].choices = vec![ChoiceOption {
            id: "a".into(),
            label: "Alpha".into(),
            value: "a".into(),
            disabled: false,
        }];
    }
    let mut ui = ui(view);
    ui.input(InputEvent::key(Key::Enter));
    ui.input(InputEvent::key(Key::Char('a')));
    assert_eq!(ui.cursor(), Some((15, 8)));
    ui.input(InputEvent::key(Key::Tab));
    assert!(ui.popup.is_none());
    ui.toggle_recovery();
    assert!(ui.cursor().is_none());
}
