//! Shared deterministic plugin controller and renderer. No I/O or terminal ownership.
mod editor;
mod paint;
pub use editor::{Editor, FIELD_BYTES};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use tg_kit::ratatui::{buffer::Buffer, layout::Rect};
use tg_kit::{InputEvent, InputResult, Key, Modifiers, Theme};
use thegn_core::plugin_app::*;

pub type UiIntent = AppEvent;
pub const EVENT_BYTES: usize = 1024 * 1024;
const HISTORY_BYTES: usize = 1024 * 1024;
const RECOVERY_BYTES: usize = 8 * 1024 * 1024;
const RECOVERY_RECORDS: usize = 8;

#[derive(Debug)]
pub struct UiOutcome {
    pub result: InputResult,
    pub intents: Vec<UiIntent>,
}
impl UiOutcome {
    fn consumed(intents: Vec<UiIntent>) -> Self {
        Self {
            result: InputResult::Consumed,
            intents,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FormKey {
    scope: String,
    widget: String,
    instance: String,
}
#[derive(Debug, Clone)]
struct FormState {
    fields: BTreeMap<String, Editor>,
    baseline: BTreeMap<String, String>,
    version: u64,
    errors: BTreeMap<String, String>,
    pending: bool,
    field: usize,
    scroll: u16,
}
impl FormState {
    fn values(&self) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|(id, e)| (id.clone(), e.text().into()))
            .collect()
    }
    fn dirty(&self) -> bool {
        self.pending
            || self
                .fields
                .iter()
                .any(|(id, e)| self.baseline.get(id).is_none_or(|v| v != e.text()))
    }
    fn bytes(&self) -> usize {
        self.fields.values().map(|e| e.text().len()).sum::<usize>()
            + self.baseline.values().map(String::len).sum::<usize>()
    }
}
#[derive(Debug, Clone, Default)]
struct SelectionState {
    current: Option<String>,
    marked: BTreeSet<String>,
    expanded: BTreeSet<String>,
    initialized: bool,
    action: usize,
    context: bool,
    column: usize,
    descending: bool,
    hscroll: u16,
}
#[derive(Debug, Clone)]
struct ChoicePopup {
    widget: String,
    field: String,
    query: Editor,
    index: usize,
}

#[derive(Clone)]
pub struct UiEngine {
    view: Option<AppView>,
    theme: Theme,
    focus: usize,
    forms: BTreeMap<FormKey, FormState>,
    selection: BTreeMap<(String, String), SelectionState>,
    scroll: BTreeMap<String, u16>,
    layout: BTreeMap<String, Rect>,
    viewports: BTreeMap<String, (u16, u16)>,
    area: Rect,
    popup: Option<ChoicePopup>,
    cursor: Option<(u16, u16)>,
    message: Option<String>,
    recovery_open: bool,
    recovery_index: usize,
    recovery_scroll: u16,
    external_records: usize,
    external_bytes: usize,
    scopes: VecDeque<String>,
    glyphs: &'static thegn_core::termcaps::GlyphSet,
}
impl UiEngine {
    pub fn new(theme: Theme) -> Self {
        Self {
            view: None,
            theme,
            focus: 0,
            forms: BTreeMap::new(),
            selection: BTreeMap::new(),
            scroll: BTreeMap::new(),
            layout: BTreeMap::new(),
            viewports: BTreeMap::new(),
            area: Rect::default(),
            popup: None,
            cursor: None,
            message: None,
            recovery_open: false,
            recovery_index: 0,
            recovery_scroll: 0,
            external_records: 0,
            external_bytes: 0,
            scopes: VecDeque::new(),
            glyphs: thegn_core::termcaps::glyphs(thegn_core::termcaps::UnicodeLevel::Full),
        }
    }
    pub fn view(&self) -> Option<&AppView> {
        self.view.as_ref()
    }
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }
    pub fn set_glyphs(&mut self, glyphs: &'static thegn_core::termcaps::GlyphSet) {
        self.glyphs = glyphs;
    }
    pub fn status_line(&self) -> Option<String> {
        self.message.clone().or_else(|| {
            if self.recovery_count() > 0 {
                Some(format!(
                    "{} unsaved drafts {} plugin-app-drafts action (F2 by default): inspect recovery",
                    self.recovery_count(),
                    self.glyphs.middot,
                ))
            } else {
                self.view.as_ref().map(|v| v.status.clone())
            }
        })
    }
    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }
    pub fn delivery_failed(&mut self, message: impl Into<String>) {
        self.message = Some(diagnostic(&message.into()));
    }
    pub fn reset_viewports(&mut self) {
        self.viewports.clear();
    }
    pub fn clear_message(&mut self) {
        self.message = None;
    }
    /// Adapter-owned submitted/unknown snapshots share the same plugin budget.
    pub fn set_external_recovery_usage(&mut self, records: usize, bytes: usize) {
        self.external_records = records;
        self.external_bytes = bytes;
    }
    pub fn retained_bytes(&self) -> usize {
        self.forms.values().map(FormState::bytes).sum()
    }
    pub fn dirty_records(&self) -> usize {
        self.forms.values().filter(|s| s.dirty()).count()
    }
    fn current_form_fields(&self) -> BTreeMap<FormKey, BTreeSet<String>> {
        self.widgets()
            .into_iter()
            .filter_map(|w| {
                if let Widget::Form {
                    id,
                    instance,
                    fields,
                    ..
                } = w
                {
                    Some((
                        self.key(&id, &instance),
                        fields.into_iter().map(|f| f.id).collect(),
                    ))
                } else {
                    None
                }
            })
            .collect()
    }
    pub fn recovery_count(&self) -> usize {
        self.recoveries().len()
    }
    pub fn recoveries(&self) -> Vec<(String, String, String)> {
        let current = self.current_form_fields();
        self.forms
            .iter()
            .filter(|(key, state)| {
                state.dirty()
                    && current
                        .get(*key)
                        .is_none_or(|fields| state.fields.keys().any(|id| !fields.contains(id)))
            })
            .map(|(key, _)| (key.scope.clone(), key.widget.clone(), key.instance.clone()))
            .collect()
    }
    /// Recovery is explicit and returns a complete snapshot for export/reopening.
    pub fn recovery_values(
        &self,
        scope: &str,
        widget: &str,
        instance: &str,
    ) -> Option<BTreeMap<String, String>> {
        self.forms
            .get(&FormKey {
                scope: scope.into(),
                widget: widget.into(),
                instance: instance.into(),
            })
            .map(FormState::values)
    }
    pub fn discard_recovery(&mut self, scope: &str, widget: &str, instance: &str) -> bool {
        let key = FormKey {
            scope: scope.into(),
            widget: widget.into(),
            instance: instance.into(),
        };
        if let Some(fields) = self.current_form_fields().get(&key) {
            if let Some(state) = self.forms.get_mut(&key) {
                let before = state.fields.len();
                state.fields.retain(|id, _| fields.contains(id));
                state.baseline.retain(|id, _| fields.contains(id));
                return state.fields.len() != before;
            }
            false
        } else {
            self.forms.remove(&key).is_some()
        }
    }
    fn key(&self, widget: &str, instance: &str) -> FormKey {
        FormKey {
            scope: self
                .view
                .as_ref()
                .map(|v| v.state_scope.clone())
                .unwrap_or_default(),
            widget: widget.into(),
            instance: instance.into(),
        }
    }
    fn current_form_keys(&self) -> BTreeSet<FormKey> {
        self.widgets()
            .into_iter()
            .filter_map(|w| {
                if let Widget::Form { id, instance, .. } = w {
                    Some(self.key(&id, &instance))
                } else {
                    None
                }
            })
            .collect()
    }
    fn selection_key(&self, id: &str) -> (String, String) {
        (
            self.view
                .as_ref()
                .map(|v| v.state_scope.clone())
                .unwrap_or_default(),
            id.into(),
        )
    }
    fn widgets(&self) -> Vec<Widget> {
        fn walk(w: &Widget, out: &mut Vec<Widget>) {
            match w {
                Widget::Split { children, .. } => {
                    for c in children {
                        walk(c, out)
                    }
                }
                Widget::WeightedSplit { children, .. } => {
                    for c in children {
                        walk(&c.widget, out)
                    }
                }
                _ => out.push(w.clone()),
            }
        }
        let mut out = Vec::new();
        if let Some(v) = &self.view {
            walk(&v.root, &mut out);
        }
        out
    }
    pub fn replace_view(&mut self, view: AppView) -> Result<(), String> {
        view.validate()?;
        let snapshot = self.clone();
        let old_focus = self.widgets().get(self.focus).map(|w| w.id().to_string());
        let old_scope = self.view.as_ref().map(|v| v.state_scope.as_str());
        let same_scope = old_scope == Some(view.state_scope.as_str());
        self.view = Some(view);
        let widgets = self.widgets();
        let current = self.current_form_keys();
        self.forms.retain(|k, v| current.contains(k) || v.dirty());
        if !same_scope {
            self.viewports.clear();
            self.popup = None;
            self.scroll.clear();
        }
        let scope = self
            .view
            .as_ref()
            .map(|v| v.state_scope.clone())
            .unwrap_or_default();
        self.scopes.retain(|s| s != &scope);
        self.scopes.push_back(scope.clone());
        while self.scopes.len() > 8 {
            self.scopes.pop_front();
        }
        let widget_ids: BTreeSet<_> = widgets.iter().map(|w| w.id().to_string()).collect();
        self.selection.retain(|(s, id), _| {
            self.scopes.contains(s) && (s != &scope || widget_ids.contains(id))
        });
        self.scroll.retain(|id, _| widget_ids.contains(id));
        if self.popup.as_ref().is_some_and(|p|!widgets.iter().any(|w|matches!(w,Widget::Form{id,fields,..}if *id==p.widget&&fields.iter().any(|f|f.id==p.field)))){self.popup=None;}
        for w in &widgets {
            if let Widget::Form {
                id,
                instance,
                fields,
                ..
            } = w
            {
                let key = self.key(id, instance);
                let state = self.forms.entry(key).or_insert_with(|| FormState {
                    fields: BTreeMap::new(),
                    baseline: BTreeMap::new(),
                    version: 0,
                    errors: BTreeMap::new(),
                    pending: false,
                    field: 0,
                    scroll: 0,
                });
                for f in fields {
                    let clean = state
                        .fields
                        .get(&f.id)
                        .is_none_or(|e| state.baseline.get(&f.id).is_some_and(|b| b == e.text()));
                    if clean {
                        state
                            .fields
                            .insert(f.id.clone(), Editor::new(f.value.clone()));
                        state.baseline.insert(f.id.clone(), f.value.clone());
                    }
                }
                // Removed dirty fields remain recoverable within the form snapshot.
                state.fields.retain(|id, e| {
                    fields.iter().any(|f| &f.id == id)
                        || state.baseline.get(id).is_none_or(|v| v != e.text())
                });
                state.field = state.field.min(fields.len().saturating_sub(1));
            }
            let key = self.selection_key(w.id());
            if matches!(
                w,
                Widget::List { .. }
                    | Widget::Tree { .. }
                    | Widget::Table { .. }
                    | Widget::Actions { .. }
                    | Widget::Presentation { .. }
                    | Widget::Progress { .. }
            ) {
                let rows = rows(w, self.selection.get(&key));
                let state = self.selection.entry(key).or_default();
                if !state.initialized {
                    if let Widget::Tree { items, .. } = w {
                        state.expanded = items
                            .iter()
                            .filter(|i| i.expanded)
                            .map(|i| i.id.clone())
                            .collect();
                    }
                    state.initialized = true;
                }
                let ids: BTreeSet<_> = all_row_ids(w);
                state.marked.retain(|id| ids.contains(id));
                state.expanded.retain(|id| ids.contains(id));
                if state.current.as_ref().is_none_or(|id| !ids.contains(id)) {
                    state.current = rows.first().map(|r| r.id.clone());
                }
            }
        }
        self.focus = if same_scope {
            old_focus
                .and_then(|id| widgets.iter().position(|w| w.id() == id))
                .unwrap_or(0)
        } else {
            0
        };
        if self.dirty_records().saturating_add(self.external_records) > RECOVERY_RECORDS
            || self.retained_bytes().saturating_add(self.external_bytes) > RECOVERY_BYTES
        {
            *self = snapshot;
            return Err(
                "View would exceed recovery limits; inspect or discard a saved draft with the plugin-app-drafts action (F2 by default)"
                    .into(),
            );
        }
        self.trim_history();
        Ok(())
    }
    /// Acknowledge only the original form instance/scope and submitted version.
    /// Later typing remains dirty; successful acknowledgments never clear it.
    #[allow(
        clippy::too_many_arguments,
        reason = "Acknowledgments must retain the original scope, instance, draft version and submitted snapshot"
    )]
    pub fn acknowledge(
        &mut self,
        scope: &str,
        widget: &str,
        instance: &str,
        draft_version: u64,
        submitted: &BTreeMap<String, String>,
        result: &AppEventResult,
        accepted: bool,
    ) {
        if let Some(form) = self.forms.get_mut(&FormKey {
            scope: scope.into(),
            widget: widget.into(),
            instance: instance.into(),
        }) {
            form.pending = false;
            if accepted && result.field_errors.is_empty() {
                for (id, value) in submitted {
                    form.baseline.insert(id.clone(), value.clone());
                }
                if form.version == draft_version {
                    form.errors.clear();
                    for (id, value) in submitted {
                        if let Some(editor) = form.fields.get_mut(id)
                            && editor.text() != value
                        {
                            editor.replace_external(value.clone());
                        }
                    }
                }
            } else if form.version == draft_version {
                form.errors = result
                    .field_errors
                    .iter()
                    .filter(|(id, _)| form.fields.contains_key(*id))
                    .map(|(id, error)| (id.clone(), diagnostic(error)))
                    .collect();
            }
        }
        if !result.message.is_empty() {
            self.message = Some(diagnostic(&result.message));
        }
        self.prepare(self.area);
    }
    pub fn mark_submitted(&mut self, widget: &str, instance: &str) {
        let key = self.key(widget, instance);
        if let Some(f) = self.forms.get_mut(&key) {
            f.pending = true;
        }
    }
    pub fn submission_failed(&mut self, scope: &str, widget: &str, instance: &str, message: &str) {
        if let Some(f) = self.forms.get_mut(&FormKey {
            scope: scope.into(),
            widget: widget.into(),
            instance: instance.into(),
        }) {
            f.pending = false;
        }
        self.delivery_failed(message);
    }
    /// Calculate geometry and viewport notifications outside painting.
    pub fn prepare(&mut self, area: Rect) -> Vec<UiIntent> {
        self.area = area;
        self.layout.clear();
        self.cursor = None;
        let widgets = self.widgets();
        let focus = widgets.get(self.focus).map(|w| w.id().to_string());
        if let Some(view) = &self.view {
            layout(&view.root, area, focus.as_deref(), &mut self.layout);
        }
        let mut intents = Vec::new();
        let mut next = BTreeMap::new();
        for w in &widgets {
            if let Widget::Grid { id, .. } = w {
                let rect = self.layout.get(id).copied().unwrap_or_default();
                let size = (rect.width, rect.height);
                next.insert(id.clone(), size);
                if self.viewports.get(id) != Some(&size) {
                    intents.push(AppEvent::Resize {
                        widget: id.clone(),
                        width: size.0,
                        height: size.1,
                    });
                }
            }
            if focus.as_deref() == Some(w.id()) {
                match w {
                    Widget::Grid { id, cursor, .. } => {
                        if let Some(area) = self.layout.get(id) {
                            self.cursor = cursor
                                .filter(|(x, y)| *x < area.width && *y < area.height)
                                .map(|(x, y)| (area.x + x, area.y + y));
                        }
                    }
                    Widget::Form {
                        id,
                        instance,
                        fields,
                        ..
                    } => {
                        let key = self.key(id, instance);
                        if let (Some(area), Some(form)) =
                            (self.layout.get(id), self.forms.get_mut(&key))
                        {
                            let inner = inner(*area);
                            let mut y = 0u16;
                            let active = form.field;
                            for (i, f) in fields.iter().enumerate() {
                                let h = effective_field_height(f, form);
                                if i == active {
                                    if y < form.scroll {
                                        form.scroll = y;
                                    }
                                    if y.saturating_add(h)
                                        > form.scroll.saturating_add(inner.height)
                                    {
                                        form.scroll =
                                            y.saturating_add(h).saturating_sub(inner.height);
                                    }
                                    if let Some(e) = form.fields.get_mut(&f.id) {
                                        let label = field_label_width(f);
                                        let input_width = inner
                                            .width
                                            .saturating_sub(if f.multiline { 2 } else { label });
                                        e.follow_caret(
                                            input_width as usize,
                                            if f.multiline { 3 } else { 1 },
                                        );
                                        let (line, col) = e.line_column();
                                        let x = inner
                                            .x
                                            .saturating_add(if f.multiline { 2 } else { label })
                                            .saturating_add(
                                                col.saturating_sub(e.scroll_column)
                                                    .min(u16::MAX as usize)
                                                    as u16,
                                            );
                                        let cy = inner
                                            .y
                                            .saturating_add(y.saturating_sub(form.scroll))
                                            .saturating_add(if f.multiline { 1 } else { 0 })
                                            .saturating_add(
                                                line.saturating_sub(e.scroll_line)
                                                    .min(u16::MAX as usize)
                                                    as u16,
                                            );
                                        if !f.disabled
                                            && !f.read_only
                                            && f.choices.is_empty()
                                            && f.kind != FieldKind::Toggle
                                            && x < inner.right()
                                            && cy < inner.bottom()
                                        {
                                            self.cursor = Some((x, cy));
                                        }
                                    }
                                }
                                y = y.saturating_add(h);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.viewports = next;
        if let Some(popup) = &mut self.popup
            && let Some(rect) = self.layout.get(&popup.widget)
        {
            let area = inner(*rect);
            let width = area.width.saturating_sub(8);
            popup.query.follow_caret(width as usize, 1);
            let (_, column) = popup.query.line_column();
            let x = area
                .x
                .saturating_add(8)
                .saturating_add(column.saturating_sub(popup.query.scroll_column) as u16);
            self.cursor = if width > 0 && area.height > 0 && x < area.right() {
                Some((x, area.y))
            } else {
                None
            };
        }
        if self.recovery_open {
            self.cursor = None;
        }
        intents
    }
    /// Rendering reads prepared state. It cannot enqueue events or mutate drafts.
    pub fn paint(&self, buf: &mut Buffer) {
        paint::paint(self, buf)
    }

    /// Open or close the local draft-recovery inspector. The host binds this to
    /// the `plugin-app-drafts` keymap action; the engine never interprets F2.
    pub fn toggle_recovery(&mut self) {
        self.recovery_open = !self.recovery_open;
        self.recovery_scroll = 0;
    }

    pub fn recovery_open(&self) -> bool {
        self.recovery_open
    }

    pub fn input(&mut self, event: InputEvent) -> UiOutcome {
        let mut out = self.input_inner(event);
        out.intents.extend(self.prepare(self.area));
        out
    }
    fn input_inner(&mut self, event: InputEvent) -> UiOutcome {
        if let InputEvent::Paste(text) = &event
            && text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
            self.delivery_failed(
                "Paste contains unsupported control characters; text was preserved",
            );
            return UiOutcome::consumed(vec![]);
        }
        if self.recovery_open {
            let entries = self.recoveries();
            if let InputEvent::Key { key, modifiers } = event {
                match key {
                    Key::Escape => self.recovery_open = false,
                    Key::Left => {
                        self.recovery_index = self.recovery_index.saturating_sub(1);
                        self.recovery_scroll = 0;
                    }
                    Key::Right => {
                        self.recovery_index =
                            (self.recovery_index + 1).min(entries.len().saturating_sub(1));
                        self.recovery_scroll = 0;
                    }
                    Key::Down => self.recovery_scroll = self.recovery_scroll.saturating_add(1),
                    Key::Up => self.recovery_scroll = self.recovery_scroll.saturating_sub(1),
                    Key::PageDown => self.recovery_scroll = self.recovery_scroll.saturating_add(10),
                    Key::PageUp => self.recovery_scroll = self.recovery_scroll.saturating_sub(10),
                    Key::Delete if modifiers.ctrl => {
                        if let Some((scope, widget, instance)) = entries.get(self.recovery_index) {
                            self.discard_recovery(scope, widget, instance);
                            self.recovery_index =
                                self.recovery_index.min(entries.len().saturating_sub(2));
                        }
                    }
                    _ => {}
                }
            }
            return UiOutcome::consumed(vec![]);
        }
        let widgets = self.widgets();
        if widgets.is_empty() {
            return UiOutcome::consumed(vec![]);
        }
        self.focus %= widgets.len();
        let w = widgets[self.focus].clone();
        if self.popup.is_some() {
            return self.popup_input(&w, event);
        }
        if matches!(
            &event,
            InputEvent::Key {
                key: Key::Tab | Key::BackTab,
                ..
            }
        ) {
            let back = matches!(
                &event,
                InputEvent::Key {
                    key: Key::BackTab,
                    ..
                } | InputEvent::Key {
                    key: Key::Tab,
                    modifiers: Modifiers { shift: true, .. }
                }
            );
            if let Widget::Form {
                id,
                instance,
                fields,
                ..
            } = &w
            {
                let k = self.key(id, instance);
                if let Some(s) = self.forms.get_mut(&k) {
                    if !back && s.field + 1 < fields.len() {
                        s.field += 1;
                        return UiOutcome::consumed(vec![]);
                    }
                    if back && s.field > 0 {
                        s.field -= 1;
                        return UiOutcome::consumed(vec![]);
                    }
                }
            }
            self.focus = if back {
                (self.focus + widgets.len() - 1) % widgets.len()
            } else {
                (self.focus + 1) % widgets.len()
            };
            if let Some(Widget::Form {
                id,
                instance,
                fields,
                ..
            }) = widgets.get(self.focus)
            {
                let k = self.key(id, instance);
                if let Some(s) = self.forms.get_mut(&k) {
                    s.field = if back {
                        fields.len().saturating_sub(1)
                    } else {
                        0
                    };
                }
            }
            return UiOutcome::consumed(vec![]);
        }
        if matches!(
            &event,
            InputEvent::Key {
                key: Key::Escape,
                ..
            }
        ) {
            if let Widget::Form { id, instance, .. } = &w {
                return UiOutcome::consumed(vec![AppEvent::Cancel(FormIdentity {
                    widget: id.clone(),
                    instance: instance.clone(),
                })]);
            }
            if let Widget::Confirm { id, .. } = &w {
                return UiOutcome::consumed(vec![AppEvent::Cancel(FormIdentity {
                    widget: id.clone(),
                    instance: String::new(),
                })]);
            }
            let key = self.selection_key(w.id());
            if let Some(s) = self.selection.get_mut(&key)
                && s.context
            {
                s.context = false;
                return UiOutcome::consumed(vec![]);
            }
            return UiOutcome {
                result: InputResult::Exit,
                intents: vec![],
            };
        }
        if let Widget::Grid { id, .. } = &w {
            return UiOutcome::consumed(match event {
                InputEvent::Key { key, modifiers } => vec![AppEvent::Key(KeyInput {
                    widget: id.clone(),
                    key: named_key(key),
                    modifiers: KeyModifiers {
                        control: modifiers.ctrl,
                        alt: modifiers.alt,
                        shift: modifiers.shift,
                        super_key: false,
                    },
                })],
                InputEvent::Paste(text) => {
                    if text.len() <= FIELD_BYTES {
                        vec![AppEvent::Paste {
                            widget: id.clone(),
                            text,
                        }]
                    } else {
                        self.message = Some("Paste exceeds event limit".into());
                        vec![]
                    }
                }
                _ => vec![],
            });
        }
        if let Widget::Form { .. } = &w {
            return self.form_input(&w, event);
        }
        let InputEvent::Key { key, modifiers } = event else {
            return UiOutcome::consumed(vec![]);
        };
        if let Widget::Text { id, .. } = &w {
            let scroll = self.scroll.entry(id.clone()).or_default();
            match key {
                Key::Down => *scroll = scroll.saturating_add(1),
                Key::Up => *scroll = scroll.saturating_sub(1),
                Key::PageDown => *scroll = scroll.saturating_add(10),
                Key::PageUp => *scroll = scroll.saturating_sub(10),
                Key::Home => *scroll = 0,
                _ => {}
            }
            return UiOutcome::consumed(vec![]);
        }
        if let Widget::Confirm {
            id,
            accept,
            binding,
            ..
        } = &w
        {
            return UiOutcome::consumed(if key == Key::Enter {
                vec![AppEvent::Action(Interaction {
                    widget: id.clone(),
                    id: accept.clone(),
                    binding: binding.clone(),
                })]
            } else {
                vec![]
            });
        }
        let sk = self.selection_key(w.id());
        let state = self.selection.entry(sk.clone()).or_default();
        let choices = rows(&w, Some(state));
        let actions = actions(&w);
        let multiple = matches!(
            w,
            Widget::Table { multiple: true, .. } | Widget::Tree { multiple: true, .. }
        );
        if (key == Key::Function(10) && modifiers.shift) || key == Key::Char(':') {
            state.context = !state.context;
            state.action = 0;
            return UiOutcome::consumed(vec![]);
        }
        if state.context && !actions.is_empty() {
            match key {
                Key::Down => state.action = (state.action + 1) % actions.len(),
                Key::Up => state.action = (state.action + actions.len() - 1) % actions.len(),
                Key::Enter => {
                    let a = &actions[state.action % actions.len()];
                    if !a.disabled {
                        let event = AppEvent::Action(Interaction {
                            widget: w.id().into(),
                            id: a.id.clone(),
                            binding: a.binding.clone(),
                        });
                        state.context = false;
                        return UiOutcome::consumed(vec![event]);
                    }
                    self.message = Some(a.disabled_reason.clone());
                }
                _ => {}
            }
            return UiOutcome::consumed(vec![]);
        }
        if choices.is_empty() {
            return UiOutcome::consumed(vec![]);
        }
        let mut index = state
            .current
            .as_ref()
            .and_then(|id| choices.iter().position(|r| &r.id == id))
            .unwrap_or(0);
        match key {
            Key::Down | Key::Char('j') => index = (index + 1).min(choices.len() - 1),
            Key::Up | Key::Char('k') => index = index.saturating_sub(1),
            Key::Home => index = 0,
            Key::End => index = choices.len() - 1,
            Key::PageDown => index = (index + 10).min(choices.len() - 1),
            Key::PageUp => index = index.saturating_sub(10),
            Key::Char(' ') if multiple => {
                let id = choices[index].id.clone();
                if !choices[index].disabled && !state.marked.remove(&id) {
                    state.marked.insert(id);
                }
            }
            Key::Char('a') if multiple && modifiers.ctrl => {
                state.marked = choices
                    .iter()
                    .filter(|r| !r.disabled)
                    .map(|r| r.id.clone())
                    .collect();
            }
            Key::Enter => {
                let r = &choices[index];
                if r.disabled {
                    return UiOutcome::consumed(vec![]);
                }
                let event = if matches!(
                    w,
                    Widget::Actions { .. } | Widget::Presentation { .. } | Widget::Progress { .. }
                ) {
                    AppEvent::Action(Interaction {
                        widget: w.id().into(),
                        id: r.id.clone(),
                        binding: r.binding.clone(),
                    })
                } else {
                    AppEvent::Activate(Interaction {
                        widget: w.id().into(),
                        id: r.id.clone(),
                        binding: r.binding.clone(),
                    })
                };
                return UiOutcome::consumed(vec![event]);
            }
            Key::Left | Key::Right if matches!(w, Widget::Tree { .. }) => {
                let id = &choices[index].id;
                if key == Key::Right {
                    state.expanded.insert(id.clone());
                } else {
                    state.expanded.remove(id);
                }
            }
            Key::Left | Key::Right => {
                if modifiers.shift {
                    state.hscroll = if key == Key::Right {
                        state.hscroll.saturating_add(4)
                    } else {
                        state.hscroll.saturating_sub(4)
                    };
                } else if let Widget::Table { columns, .. } = &w
                    && !columns.is_empty()
                {
                    state.column = if key == Key::Right {
                        (state.column + 1) % columns.len()
                    } else {
                        (state.column + columns.len() - 1) % columns.len()
                    };
                }
            }
            Key::Char('s') => {
                if let Widget::Table { columns, .. } = &w
                    && let Some(c) = columns.get(state.column)
                    && c.sortable
                {
                    state.descending = !state.descending;
                    return UiOutcome::consumed(vec![AppEvent::Sort {
                        widget: w.id().into(),
                        column: c.id.clone(),
                        descending: state.descending,
                    }]);
                }
            }
            _ => return UiOutcome::consumed(vec![]),
        }
        state.current = Some(choices[index].id.clone());
        UiOutcome::consumed(vec![AppEvent::Select(Selection {
            widget: w.id().into(),
            items: state.marked.iter().cloned().collect(),
            current: state.current.clone(),
        })])
    }
    fn form_input(&mut self, w: &Widget, event: InputEvent) -> UiOutcome {
        let Widget::Form {
            id,
            instance,
            fields,
            binding,
            ..
        } = w
        else {
            return UiOutcome::consumed(vec![]);
        };
        if fields.is_empty() {
            return UiOutcome::consumed(vec![]);
        }
        let k = self.key(id, instance);
        let Some(previous) = self.forms.get(&k).cloned() else {
            return UiOutcome::consumed(vec![]);
        };
        let active = previous.field.min(fields.len() - 1);
        let f = &fields[active];
        if matches!(
            &event,
            InputEvent::Key {
                key: Key::Enter,
                modifiers: Modifiers { ctrl: true, .. }
            }
        ) {
            let state = self.forms.get_mut(&k).expect("current form");
            if state.pending {
                self.message = Some("A submission is still pending".into());
                return UiOutcome::consumed(vec![]);
            }
            state.errors = validate(fields, &state.values());
            if !state.errors.is_empty() {
                state.field = fields
                    .iter()
                    .position(|f| state.errors.contains_key(&f.id))
                    .unwrap_or(active);
                return UiOutcome::consumed(vec![]);
            }
            let mut values = visible_values(fields, state);
            for f in fields {
                if f.kind == FieldKind::Integer
                    && let Some(v) = values.get_mut(&f.id)
                    && let Ok(n) = v.trim().parse::<i64>()
                {
                    *v = n.to_string();
                }
            }
            let event = AppEvent::Submit(Submission {
                widget: id.clone(),
                instance: instance.clone(),
                values,
                draft_version: state.version,
                binding: binding.clone(),
            });
            if encoded_len(&event) > EVENT_BYTES {
                self.message = Some("Encoded submission exceeds one MiB; shorten a field".into());
                return UiOutcome::consumed(vec![]);
            }
            return UiOutcome::consumed(vec![event]);
        }
        if matches!(
            &event,
            InputEvent::Key {
                key: Key::Enter,
                ..
            }
        ) && !f.multiline
            && f.choices.is_empty()
            && f.kind != FieldKind::Toggle
        {
            if let Some(s) = self.forms.get_mut(&k) {
                s.field = (active + 1) % fields.len();
            }
            return UiOutcome::consumed(vec![]);
        }
        if f.disabled || f.read_only {
            return UiOutcome::consumed(vec![]);
        }
        if (!f.choices.is_empty() || f.kind == FieldKind::Choice)
            && matches!(
                &event,
                InputEvent::Key {
                    key: Key::Enter | Key::Char(' '),
                    ..
                }
            )
        {
            self.popup = Some(ChoicePopup {
                widget: id.clone(),
                field: f.id.clone(),
                query: Editor::new(String::new()),
                index: 0,
            });
            return UiOutcome::consumed(vec![]);
        }
        let state = self.forms.get_mut(&k).expect("current form");
        let Some(e) = state.fields.get_mut(&f.id) else {
            return UiOutcome::consumed(vec![]);
        };
        let old = e.text().to_string();
        match event {
            InputEvent::Paste(text) if f.choices.is_empty() && f.kind != FieldKind::Toggle => {
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                let normalized = if f.multiline {
                    normalized
                } else {
                    normalized.replace('\n', " ")
                };
                if !e.can_insert(&normalized) {
                    self.message = Some("Field limit is 32 KiB; paste was not inserted".into());
                } else {
                    e.insert(&normalized);
                }
            }
            InputEvent::Key { key, modifiers } => {
                if modifiers.ctrl && matches!(key, Key::Char('z' | 'y' | 'Z' | 'Y')) {
                    e.key(key, modifiers, f.multiline);
                } else if f.kind == FieldKind::Toggle
                    && matches!(key, Key::Char(' ') | Key::Left | Key::Right | Key::Enter)
                {
                    let value = if e.text() == "true" { "false" } else { "true" };
                    e.replace_all(value);
                } else if !f.choices.is_empty() && matches!(key, Key::Left | Key::Right) {
                    let available: Vec<_> = f.choices.iter().filter(|c| !c.disabled).collect();
                    if !available.is_empty() {
                        let i = available
                            .iter()
                            .position(|c| c.value == e.text())
                            .unwrap_or(if key == Key::Right {
                                available.len() - 1
                            } else {
                                0
                            });
                        let next = if key == Key::Right {
                            (i + 1) % available.len()
                        } else {
                            (i + available.len() - 1) % available.len()
                        };
                        e.replace_all(&available[next].value);
                    }
                } else if f.choices.is_empty() && f.kind != FieldKind::Toggle {
                    e.key(key, modifiers, f.multiline);
                }
            }
            _ => {}
        }
        if e.text() == old {
            return UiOutcome::consumed(vec![]);
        }
        self.finish_edit(k, previous, id, instance)
    }
    fn finish_edit(
        &mut self,
        k: FormKey,
        previous: FormState,
        id: &str,
        instance: &str,
    ) -> UiOutcome {
        let current_fields = self.current_form_fields().remove(&k).unwrap_or_default();
        let state = self.forms.get_mut(&k).expect("current form");
        state.version = state.version.saturating_add(1);
        state.errors.clear();
        let event = AppEvent::Change(FormEdit {
            widget: id.into(),
            instance: instance.into(),
            values: state
                .values()
                .into_iter()
                .filter(|(id, _)| current_fields.contains(id))
                .collect(),
            draft_version: state.version,
        });
        let dirty = self.forms.values().filter(|s| s.dirty()).count();
        if encoded_len(&event) > EVENT_BYTES
            || dirty.saturating_add(self.external_records) > RECOVERY_RECORDS
            || self.retained_bytes().saturating_add(self.external_bytes) > RECOVERY_BYTES
        {
            self.forms.insert(k, previous);
            self.message =
                Some("Edit exceeds the event or recovery limit; text was preserved".into());
            return UiOutcome::consumed(vec![]);
        }
        self.trim_history();
        UiOutcome::consumed(vec![event])
    }
    fn history_bytes(&self) -> usize {
        self.forms
            .values()
            .flat_map(|s| s.fields.values())
            .map(Editor::history_bytes)
            .sum::<usize>()
            + self
                .popup
                .as_ref()
                .map_or(0, |popup| popup.query.history_bytes())
    }
    fn trim_history(&mut self) {
        while self.history_bytes() > HISTORY_BYTES {
            let mut removed = false;
            for state in self.forms.values_mut() {
                for editor in state.fields.values_mut() {
                    if editor.drop_oldest_history() {
                        removed = true;
                        break;
                    }
                }
                if removed {
                    break;
                }
            }
            if !removed {
                removed = self
                    .popup
                    .as_mut()
                    .is_some_and(|popup| popup.query.drop_oldest_history());
            }
            if !removed {
                break;
            }
        }
    }
    fn popup_input(&mut self, w: &Widget, event: InputEvent) -> UiOutcome {
        let Some(mut popup) = self.popup.take() else {
            return UiOutcome::consumed(vec![]);
        };
        let Widget::Form {
            id,
            instance,
            fields,
            ..
        } = w
        else {
            return UiOutcome::consumed(vec![]);
        };
        let Some(field) = fields.iter().find(|f| f.id == popup.field) else {
            return UiOutcome::consumed(vec![]);
        };
        let query = popup.query.text().to_lowercase();
        let filtered: Vec<_> = field
            .choices
            .iter()
            .filter(|c| c.label.to_lowercase().contains(&query))
            .collect();
        match event {
            event @ InputEvent::Key {
                key: Key::Tab | Key::BackTab,
                ..
            } => return self.input_inner(event),
            InputEvent::Key {
                key: Key::Escape, ..
            } => return UiOutcome::consumed(vec![]),
            InputEvent::Key { key: Key::Down, .. } => {
                popup.index = (popup.index + 1).min(filtered.len().saturating_sub(1))
            }
            InputEvent::Key { key: Key::Up, .. } => popup.index = popup.index.saturating_sub(1),
            InputEvent::Key {
                key: Key::Enter, ..
            } => {
                if let Some(c) = filtered.get(popup.index)
                    && !c.disabled
                {
                    let k = self.key(id, instance);
                    if let Some(previous) = self.forms.get(&k).cloned() {
                        self.forms
                            .get_mut(&k)
                            .expect("current form")
                            .fields
                            .get_mut(&field.id)
                            .expect("current field")
                            .replace_all(&c.value);
                        return self.finish_edit(k, previous, id, instance);
                    }
                }
            }
            InputEvent::Key { key, modifiers } => {
                popup.query.key(key, modifiers, false);
                popup.index = 0;
            }
            InputEvent::Paste(text) => {
                let text = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
                popup.query.insert(&text);
                popup.index = 0;
            }
            _ => {}
        }
        self.popup = Some(popup);
        self.trim_history();
        UiOutcome::consumed(vec![])
    }
}
// Four KiB reserves more than the complete worst-case escaped RPC envelope
// (bounded surface/session/scope/ticket IDs plus framing and a u64 request ID).
fn encoded_len(event: &AppEvent) -> usize {
    serde_json::to_vec(event).map_or(usize::MAX, |v| v.len() + 4096)
}
fn inner(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}
fn field_height(f: &Field) -> u16 {
    (if f.multiline { 4 } else { 1 })
        + u16::from(!f.helper.is_empty())
        + u16::from(!f.error.is_empty())
}
fn effective_field_height(f: &Field, state: &FormState) -> u16 {
    field_height(f)
        + u16::from(f.error.is_empty() && state.errors.get(&f.id).is_some_and(|e| !e.is_empty()))
}
fn validate(fields: &[Field], values: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut errors = BTreeMap::new();
    for f in fields {
        if f.disabled || f.read_only {
            continue;
        }
        let value = values.get(&f.id).map(String::as_str).unwrap_or("");
        let error = if f.required && value.trim().is_empty() {
            Some("Required")
        } else if !value.is_empty() {
            match f.kind {
                FieldKind::Integer if value.trim().parse::<i64>().is_err() => {
                    Some("Enter a whole number")
                }
                FieldKind::Date if !valid_date(value) => Some("Use YYYY-MM-DD"),
                FieldKind::DateTime if !valid_datetime(value) => {
                    Some("Use an RFC 3339 date and time")
                }
                FieldKind::Toggle if !matches!(value, "true" | "false") => {
                    Some("Choose true or false")
                }
                FieldKind::Choice if !f.choices.iter().any(|c| c.value == value && !c.disabled) => {
                    Some("Choose an available option")
                }
                _ if !f.choices.is_empty()
                    && !f.choices.iter().any(|c| c.value == value && !c.disabled) =>
                {
                    Some("Choose an available option")
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(e) = error {
            errors.insert(f.id.clone(), e.into());
        }
    }
    errors
}
fn valid_date(s: &str) -> bool {
    let parts: Vec<_> = s.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return false;
    }
    let (y, m, d) = (
        parts[0].parse::<u32>().unwrap_or(0),
        parts[1].parse::<u32>().unwrap_or(0),
        parts[2].parse::<u32>().unwrap_or(0),
    );
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let max = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    y > 0 && d > 0 && d <= max
}
fn valid_datetime(s: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(s).is_ok()
}
fn contains(w: &Widget, id: &str) -> bool {
    w.id() == id
        || match w {
            Widget::Split { children, .. } => children.iter().any(|c| contains(c, id)),
            Widget::WeightedSplit { children, .. } => {
                children.iter().any(|c| contains(&c.widget, id))
            }
            _ => false,
        }
}
fn layout(w: &Widget, area: Rect, focused: Option<&str>, out: &mut BTreeMap<String, Rect>) {
    match w {
        Widget::Split {
            children, vertical, ..
        } => {
            let entries: Vec<_> = children
                .iter()
                .map(|w| (w, 1, if *vertical { 3 } else { 24 }))
                .collect();
            split_layout(&entries, *vertical, area, focused, out);
        }
        Widget::WeightedSplit {
            children, vertical, ..
        } => {
            let entries: Vec<_> = children
                .iter()
                .map(|c| (c.widget.as_ref(), c.weight.max(1), c.min_size))
                .collect();
            split_layout(&entries, *vertical, area, focused, out);
        }
        _ => {
            out.insert(w.id().into(), area);
        }
    }
}
fn split_layout(
    children: &[(&Widget, u16, u16)],
    vertical: bool,
    area: Rect,
    focused: Option<&str>,
    out: &mut BTreeMap<String, Rect>,
) {
    if children.is_empty() {
        return;
    }
    let extent = if vertical { area.height } else { area.width };
    let min: usize = children.iter().map(|(_, _, m)| *m as usize).sum();
    if (extent as usize) < min {
        let child = children
            .iter()
            .find(|(w, _, _)| focused.is_some_and(|f| contains(w, f)))
            .unwrap_or(&children[0]);
        layout(child.0, area, focused, out);
        return;
    }
    let weights: usize = children.iter().map(|(_, w, _)| *w as usize).sum();
    let extra = extent as usize - min;
    let mut offset = 0usize;
    let mut remainder = 0usize;
    for (i, (child, weight, min)) in children.iter().enumerate() {
        let numerator = extra * (*weight as usize) + remainder;
        let mut size = *min as usize + numerator / weights;
        remainder = numerator % weights;
        if i + 1 == children.len() {
            size = extent as usize - offset;
        }
        let rect = if vertical {
            Rect::new(area.x, area.y + offset as u16, area.width, size as u16)
        } else {
            Rect::new(area.x + offset as u16, area.y, size as u16, area.height)
        };
        layout(child, rect, focused, out);
        offset += size;
    }
}
#[derive(Clone)]
struct Row {
    id: String,
    label: String,
    depth: usize,
    binding: InteractionBinding,
    disabled: bool,
}
fn all_row_ids(w: &Widget) -> BTreeSet<String> {
    match w {
        Widget::Tree { items, .. } => items.iter().map(|i| i.id.clone()).collect(),
        _ => rows(w, None).into_iter().map(|r| r.id).collect(),
    }
}
fn rows(w: &Widget, state: Option<&SelectionState>) -> Vec<Row> {
    match w {
        Widget::List { items, .. } => items
            .iter()
            .map(|i| Row {
                id: i.id.clone(),
                label: i.label.clone(),
                depth: i.depth as usize,
                binding: InteractionBinding::default(),
                disabled: false,
            })
            .collect(),
        Widget::Table { rows, .. } => rows
            .iter()
            .map(|i| Row {
                id: i.id.clone(),
                label: i.cells.join("  "),
                depth: 0,
                binding: i.binding.clone(),
                disabled: i.disabled,
            })
            .collect(),
        Widget::Tree { items, .. } => {
            let nodes: std::collections::HashMap<_, _> =
                items.iter().map(|item| (item.id.as_str(), item)).collect();
            let mut out = Vec::new();
            for i in items {
                let mut depth = 0;
                let mut parent = i.parent.as_deref();
                let mut visible = true;
                while let Some(p) = parent {
                    depth += 1;
                    if depth > items.len() {
                        visible = false;
                        break;
                    }
                    let Some(node) = nodes.get(p) else {
                        visible = false;
                        break;
                    };
                    if !state.map_or(node.expanded, |s| s.expanded.contains(p)) {
                        visible = false;
                        break;
                    }
                    parent = node.parent.as_deref();
                }
                if visible {
                    out.push(Row {
                        id: i.id.clone(),
                        label: i.label.clone(),
                        depth,
                        binding: i.binding.clone(),
                        disabled: i.disabled,
                    });
                }
            }
            out
        }
        Widget::Actions { actions, .. } | Widget::Presentation { actions, .. } => actions
            .iter()
            .map(|a| Row {
                id: a.id.clone(),
                label: format!(
                    "{}{}",
                    a.label,
                    if a.disabled && !a.disabled_reason.is_empty() {
                        format!(" ({})", a.disabled_reason)
                    } else {
                        String::new()
                    }
                ),
                depth: 0,
                binding: a.binding.clone(),
                disabled: a.disabled,
            })
            .collect(),
        Widget::Progress { cancel, .. } => cancel
            .iter()
            .map(|a| Row {
                id: a.id.clone(),
                label: a.label.clone(),
                depth: 0,
                binding: a.binding.clone(),
                disabled: a.disabled,
            })
            .collect(),
        _ => vec![],
    }
}
fn actions(w: &Widget) -> &[Action] {
    match w {
        Widget::Table { actions, .. }
        | Widget::Tree { actions, .. }
        | Widget::Presentation { actions, .. }
        | Widget::Actions { actions, .. } => actions,
        _ => &[],
    }
}
fn named_key(key: Key) -> NamedKey {
    match key {
        Key::Char(c) => NamedKey::Character(c.to_string()),
        Key::Enter => NamedKey::Enter,
        Key::Escape => NamedKey::Escape,
        Key::Backspace => NamedKey::Backspace,
        Key::Delete => NamedKey::Delete,
        Key::Tab => NamedKey::Tab,
        Key::BackTab => NamedKey::BackTab,
        Key::Up => NamedKey::Up,
        Key::Down => NamedKey::Down,
        Key::Left => NamedKey::Left,
        Key::Right => NamedKey::Right,
        Key::Home => NamedKey::Home,
        Key::End => NamedKey::End,
        Key::PageUp => NamedKey::PageUp,
        Key::PageDown => NamedKey::PageDown,
        Key::Function(n) => NamedKey::Function(n),
    }
}

#[cfg(test)]
mod tests;

fn visible_values(fields: &[Field], state: &FormState) -> BTreeMap<String, String> {
    fields
        .iter()
        .filter_map(|field| {
            state
                .fields
                .get(&field.id)
                .map(|editor| (field.id.clone(), editor.text().into()))
        })
        .collect()
}
fn diagnostic(message: &str) -> String {
    let mut text = String::new();
    for character in message.chars().filter(|c| !c.is_control()) {
        if text.len() + character.len_utf8() > 8192 {
            break;
        }
        text.push(character);
    }
    text
}

fn field_label_width(field: &Field) -> u16 {
    (unicode_width::UnicodeWidthStr::width(field.label.as_str()).min(u16::MAX as usize) as u16)
        .saturating_add(4)
        .saturating_add(u16::from(field.required))
}
