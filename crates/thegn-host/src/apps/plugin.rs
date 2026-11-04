//! Thin supervised app adapter; the shared UI owns layout and editing.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tg_kit::ratatui::widgets::{Block, Borders, Paragraph, Widget as RenderWidget, Wrap};
use tg_kit::ratatui::{buffer::Buffer, layout::Rect};
use tg_kit::{AppTile, InputEvent, InputResult, Key, Theme};
use thegn_core::plugin_api::{PluginCallback, RpcResponse};
use thegn_core::plugin_app::{AppEvent, AppEventParams, AppEventResult, AppView};
use thegn_plugin_ui::UiEngine;
use thegn_svc::plugin::SessionWriter;

pub(crate) const PLUGIN_APP_REQUESTS: &str = "plugin-app-requests";
pub(crate) const PLUGIN_APP_DRAFTS: &str = "plugin-app-drafts";

const MAX_REQUESTS: usize = 8;
const MAX_RECOVERY_BYTES: usize = 8 * 1024 * 1024;

struct Pending {
    event: AppEventParams,
    deadline: Instant,
    session: crate::plugins::SessionGen,
    unknown: bool,
    bytes: usize,
}
#[derive(Default)]
pub(crate) struct RequestLedger {
    pending: BTreeMap<(crate::plugins::SessionGen, u64), Pending>,
    local_usage: BTreeMap<String, (usize, usize)>,
}
impl RequestLedger {
    fn bytes(&self) -> usize {
        self.pending.values().map(|pending| pending.bytes).sum()
    }
    fn retained(&self) -> (usize, usize) {
        self.local_usage.values().fold(
            (self.pending.len(), self.bytes()),
            |(records, bytes), usage| {
                (
                    records.saturating_add(usage.0),
                    bytes.saturating_add(usage.1),
                )
            },
        )
    }
    fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .filter(|pending| !pending.unknown)
            .map(|pending| pending.deadline)
            .min()
    }
}

enum Delivery {
    View(AppView),
    Response {
        session: crate::plugins::SessionGen,
        response: RpcResponse,
    },
}
pub(crate) struct AppState {
    pub view: Option<AppView>,
    pub writer: Option<SessionWriter>,
    pub background: bool,
    pub timeout: Duration,
    pub session: Option<crate::plugins::SessionGen>,
    pub epoch: u64,
    pub focus_requested: bool,
    pub ready: bool,
    pub requests: Arc<Mutex<RequestLedger>>,
    pending: BTreeSet<(crate::plugins::SessionGen, u64)>,
    deliveries: VecDeque<Delivery>,
    delivery_bytes: usize,
    error: Option<String>,
}
impl AppState {
    pub(crate) fn new(background: bool) -> Self {
        Self {
            view: None,
            writer: None,
            background,
            timeout: Duration::from_secs(15),
            session: None,
            epoch: 0,
            focus_requested: false,
            ready: false,
            requests: Arc::new(Mutex::new(RequestLedger::default())),
            pending: BTreeSet::new(),
            deliveries: VecDeque::new(),
            delivery_bytes: 0,
            error: None,
        }
    }
    /// Accepted frames and responses retain their originating wire order.
    pub(crate) fn apply_view(&mut self, view: AppView) -> Result<(), String> {
        view.validate()?;
        if self
            .view
            .as_ref()
            .is_some_and(|previous| view.revision <= previous.revision)
        {
            return Err("app revision must increase".into());
        }
        let bytes = serde_json::to_vec(&view)
            .map_err(|error| error.to_string())?
            .len();
        let replaced = if let Some(Delivery::View(previous)) = self.deliveries.back() {
            Some(serde_json::to_vec(previous).map_or(0, |wire| wire.len()))
        } else {
            None
        };
        if self.deliveries.len() - usize::from(replaced.is_some()) >= 32
            || self.delivery_bytes - replaced.unwrap_or(0) + bytes > 2 * 1024 * 1024
        {
            self.error = Some("Plugin app ingress overflow; session closed".into());
            if let Some(writer) = &self.writer {
                writer.close();
            }
            return Err("app ingress overflow".into());
        }
        if let Some(replaced) = replaced {
            self.deliveries.pop_back();
            self.delivery_bytes -= replaced;
        }
        self.view = Some(view.clone());
        self.ready = true;
        self.delivery_bytes += bytes;
        self.deliveries.push_back(Delivery::View(view));
        Ok(())
    }
    pub(crate) fn push_response(
        &mut self,
        session: crate::plugins::SessionGen,
        response: RpcResponse,
    ) -> bool {
        if !self.pending.contains(&(session, response.id)) {
            return false;
        }
        let bytes = serde_json::to_vec(&response).map_or(2 * 1024 * 1024 + 1, |wire| wire.len());
        if bytes >= thegn_svc::plugin::proc::MAX_LINE_BYTES
            || self.deliveries.len() >= 32
            || bytes > (2 * 1024 * 1024usize).saturating_sub(self.delivery_bytes)
        {
            self.error = Some("Plugin acknowledgment overflow; outcome unknown".into());
            if let Some(writer) = &self.writer {
                writer.close();
            }
            return false;
        }
        self.delivery_bytes += bytes;
        self.deliveries
            .push_back(Delivery::Response { session, response });
        true
    }
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .next_deadline()
    }
    #[cfg(test)]
    pub(crate) fn response_queued(&self, session: crate::plugins::SessionGen, id: u64) -> bool {
        self.deliveries.iter().any(|delivery| {
            matches!(delivery,
                Delivery::Response { session: queued_session, response }
                    if *queued_session == session && response.id == id
            )
        })
    }
    pub(crate) fn request_event(&mut self, event: AppEventParams) -> Result<u64, String> {
        event.validate()?;
        let session = self.session.ok_or("plugin is unavailable")?;
        let writer = self.writer.as_ref().ok_or("plugin is unavailable")?;
        let wire = serde_json::json!({"kind":"App","payload": event});
        let bytes = serde_json::to_vec(&wire)
            .map_err(|error| error.to_string())?
            .len();
        let mut ledger = self
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (records, retained) = ledger.retained();
        if records >= MAX_REQUESTS || bytes > MAX_RECOVERY_BYTES.saturating_sub(retained) {
            return Err("Pending/recovery limit reached (8 requests / 8 MiB); inspect drafts: plugin-app-drafts (F2 by default) or requests: plugin-app-requests (F3 by default)".into());
        }
        let result = if matches!(&event.event, AppEvent::Cancel(_)) {
            writer.request_control(PluginCallback::OnEvent, wire)
        } else {
            writer.request(PluginCallback::OnEvent, wire)
        };
        let id =
            result.map_err(|error| format!("Input was not admitted: {error}; draft retained"))?;
        ledger.pending.insert(
            (session, id),
            Pending {
                event,
                deadline: Instant::now() + self.timeout,
                session,
                unknown: false,
                bytes,
            },
        );
        self.pending.insert((session, id));
        Ok(id)
    }
}
pub(crate) type SharedApp = Arc<Mutex<AppState>>;
pub(crate) type Rebind = Arc<Mutex<Option<(String, String, SharedApp)>>>;

#[derive(Clone)]
pub(crate) struct AppTabDescriptor {
    pub id: String,
    pub label: String,
    pub surface: String,
    pub shared: SharedApp,
}

pub(crate) struct PluginTile {
    id: String,
    surface: String,
    label: String,
    shared: SharedApp,
    rebind: Rebind,
    ui: UiEngine,
    epoch: u64,
    dirty: bool,
    area: Rect,
    recovery_open: bool,
    recovery_selected: usize,
    recovery_scroll: u16,
    notified_unknown: BTreeSet<(crate::plugins::SessionGen, u64)>,
}
impl PluginTile {
    pub(crate) fn new(
        id: String,
        surface: String,
        label: String,
        shared: SharedApp,
        theme: Theme,
    ) -> Self {
        Self {
            id,
            surface,
            label,
            shared,
            rebind: Arc::new(Mutex::new(None)),
            ui: UiEngine::new(theme),
            epoch: 0,
            dirty: true,
            area: Rect::default(),
            recovery_open: false,
            recovery_selected: 0,
            recovery_scroll: 0,
            notified_unknown: BTreeSet::new(),
        }
    }
    pub(crate) fn rebind_handle(&self) -> Rebind {
        self.rebind.clone()
    }
    fn send(&mut self, event: AppEvent) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(writer) = state.writer.clone() else {
            self.ui
                .delivery_failed("Plugin unavailable; input retained locally");
            return;
        };
        if !state.ready
            && !matches!(
                &event,
                AppEvent::Focus { .. }
                    | AppEvent::Visibility { .. }
                    | AppEvent::Resize { .. }
                    | AppEvent::Closed
            )
        {
            self.ui
                .delivery_failed("Plugin is restarting; wait for its fresh view before editing");
            return;
        }
        let params = AppEventParams {
            surface: self.surface.clone(),
            session: writer.session_token().to_owned(),
            state_scope: self
                .ui
                .view()
                .map(|view| view.state_scope.clone())
                .unwrap_or_default(),
            revision: self.ui.view().map_or(0, |view| view.revision),
            focus_ticket: None,
            event,
        };
        if let Err(error) = params.validate() {
            self.ui.delivery_failed(error);
            return;
        }
        let wire = serde_json::json!({"kind":"App","payload":params});
        if params.event.reliable() {
            if let AppEvent::Submit(submission) = &params.event {
                self.ui
                    .mark_submitted(&submission.widget, &submission.instance);
            }
            state
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .local_usage
                .insert(
                    self.surface.clone(),
                    (self.ui.dirty_records(), self.ui.retained_bytes()),
                );
            match state.request_event(params.clone()) {
                Ok(_) => {}
                Err(error) => {
                    if let AppEvent::Submit(submission) = &params.event {
                        self.ui.submission_failed(
                            &params.state_scope,
                            &submission.widget,
                            &submission.instance,
                            &error,
                        );
                    } else {
                        self.ui.delivery_failed(error);
                    }
                    state
                        .requests
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .local_usage
                        .insert(
                            self.surface.clone(),
                            (self.ui.dirty_records(), self.ui.retained_bytes()),
                        );
                }
            }
        } else {
            let result = match &params.event {
                AppEvent::Change(change) => writer.notify_coalesced(
                    PluginCallback::OnEvent,
                    &format!(
                        "{}:change:{}:{}",
                        self.surface, params.state_scope, change.widget
                    ),
                    wire,
                ),
                AppEvent::Select(selection) => writer.notify_coalesced(
                    PluginCallback::OnEvent,
                    &format!("{}:select:{}", self.surface, selection.widget),
                    wire,
                ),
                AppEvent::Resize { widget, .. } => writer.notify_coalesced(
                    PluginCallback::OnEvent,
                    &format!("{}:resize:{widget}", self.surface),
                    wire,
                ),
                AppEvent::Visibility { .. } | AppEvent::Focus { .. } | AppEvent::Closed => {
                    writer.notify_control(PluginCallback::OnEvent, wire)
                }
                _ => writer.notify(PluginCallback::OnEvent, wire),
            };
            if let Err(error) = result {
                if matches!(
                    &params.event,
                    AppEvent::Visibility { .. } | AppEvent::Focus { .. } | AppEvent::Closed
                ) {
                    writer.close();
                }
                self.ui
                    .delivery_failed(format!("Input was not delivered: {error}"));
            }
        }
    }
    fn prepare(&mut self) {
        for event in self.ui.prepare(self.area) {
            self.send(event);
        }
    }
    fn sync_budget(&mut self) {
        let state = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut ledger = state
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ledger.local_usage.insert(
            self.surface.clone(),
            (self.ui.dirty_records(), self.ui.retained_bytes()),
        );
        let (records, bytes) = ledger
            .local_usage
            .iter()
            .filter(|(surface, _)| *surface != &self.surface)
            .fold(
                (ledger.pending.len(), ledger.bytes()),
                |(records, bytes), (_, usage)| {
                    (
                        records.saturating_add(usage.0),
                        bytes.saturating_add(usage.1),
                    )
                },
            );
        self.ui.set_external_recovery_usage(records, bytes);
    }
    fn recovery_input(&mut self, event: &InputEvent) -> bool {
        if !self.recovery_open {
            return false;
        }
        if let InputEvent::Key { key, modifiers } = event {
            match key {
                Key::Escape => self.recovery_open = false,
                Key::Down => {
                    self.recovery_selected = self.recovery_selected.saturating_add(1);
                    self.recovery_scroll = 0;
                }
                Key::Up => {
                    self.recovery_selected = self.recovery_selected.saturating_sub(1);
                    self.recovery_scroll = 0;
                }
                Key::PageDown => self.recovery_scroll = self.recovery_scroll.saturating_add(10),
                Key::PageUp => self.recovery_scroll = self.recovery_scroll.saturating_sub(10),
                Key::Home => self.recovery_scroll = 0,
                Key::Delete if modifiers.ctrl => {
                    let mut state = self
                        .shared
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let mut ledger = state
                        .requests
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    let key = ledger
                        .pending
                        .iter()
                        .filter(|(_, pending)| pending.unknown)
                        .nth(
                            self.recovery_selected.min(
                                ledger
                                    .pending
                                    .values()
                                    .filter(|pending| pending.unknown)
                                    .count()
                                    .saturating_sub(1),
                            ),
                        )
                        .map(|(key, _)| *key);
                    if let Some(key) = key {
                        ledger.pending.remove(&key);
                        self.notified_unknown.remove(&key);
                        drop(ledger);
                        state.pending.remove(&key);
                        self.ui.delivery_failed(
                            "Unknown-outcome recovery discarded locally; no retry was sent",
                        );
                    }
                }
                _ => {}
            }
        }
        self.dirty = true;
        true
    }
}
impl AppTile for PluginTile {
    fn id(&self) -> &str {
        &self.id
    }
    fn title(&self) -> String {
        self.label.clone()
    }
    fn pump(&mut self) -> bool {
        let binding = self
            .rebind
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some((surface, label, shared)) = binding {
            let old = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut ledger = old
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            for (key, pending) in ledger.pending.iter_mut() {
                pending.unknown = true;
                if pending.event.surface == self.surface
                    && self.notified_unknown.insert(*key)
                    && let AppEvent::Submit(submission) = &pending.event.event
                {
                    self.ui.submission_failed(
                        &pending.event.state_scope,
                        &submission.widget,
                        &submission.instance,
                        "Plugin reloaded; outcome unknown, draft retained (request recovery: plugin-app-requests, F3 by default)",
                    );
                }
            }
            drop(ledger);
            {
                let mut replacement = shared.lock().unwrap_or_else(|error| error.into_inner());
                replacement.requests = old.requests.clone();
                replacement.pending.extend(old.pending.iter().copied());
            }
            drop(old);
            if surface != self.surface {
                shared
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .requests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .local_usage
                    .remove(&self.surface);
            }
            self.shared = shared;
            self.surface = surface;
            self.label = label;
            self.dirty = true;
        }
        let (epoch, live_session, deliveries, error, ledger) = {
            let mut state = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let mut deliveries = state.deliveries.drain(..).collect::<Vec<_>>();
            if deliveries.is_empty()
                && (state.epoch != self.epoch || self.ui.view().is_none())
                && let Some(view) = &state.view
            {
                deliveries.push(Delivery::View(view.clone()));
            }
            state.delivery_bytes = 0;
            (
                state.epoch,
                state.session,
                deliveries,
                state.error.take(),
                state.requests.clone(),
            )
        };
        self.sync_budget();
        let mut changed = epoch != self.epoch || !deliveries.is_empty() || error.is_some();
        if epoch != self.epoch {
            self.ui.reset_viewports();
        }
        self.epoch = epoch;
        if let Some(error) = error {
            self.ui.delivery_failed(error);
        }
        for delivery in deliveries {
            match delivery {
                Delivery::View(view) => {
                    if !ledger
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .pending
                        .values()
                        .any(|pending| pending.unknown && pending.event.surface == self.surface)
                    {
                        self.ui.clear_message();
                    }
                    if let Err(error) = self.ui.replace_view(view) {
                        self.ui.delivery_failed(error);
                    }
                }
                Delivery::Response { session, response } => {
                    let pending = ledger
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .pending
                        .remove(&(session, response.id));
                    self.shared
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .pending
                        .remove(&(session, response.id));
                    if let Some(mut pending) = pending {
                        let accepted = response.error.is_none();
                        let result = if let Some(error) = response.error {
                            AppEventResult {
                                message: error.message,
                                ..Default::default()
                            }
                        } else {
                            match response
                                .result
                                .and_then(|value| serde_json::from_value(value).ok())
                            {
                                Some(result) => result,
                                None => {
                                    pending.unknown = true;
                                    self.notified_unknown.insert((session, response.id));
                                    let message = "Malformed acknowledgment; outcome unknown, draft retained (request recovery: plugin-app-requests, F3 by default)";
                                    if let AppEvent::Submit(submission) = &pending.event.event {
                                        self.ui.submission_failed(
                                            &pending.event.state_scope,
                                            &submission.widget,
                                            &submission.instance,
                                            message,
                                        );
                                    } else {
                                        self.ui.delivery_failed(message);
                                    }
                                    ledger
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .pending
                                        .insert((session, response.id), pending);
                                    self.shared
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .pending
                                        .insert((session, response.id));
                                    continue;
                                }
                            }
                        };
                        if let AppEvent::Submit(submission) = &pending.event.event {
                            self.ui.acknowledge(
                                &pending.event.state_scope,
                                &submission.widget,
                                &submission.instance,
                                submission.draft_version,
                                &submission.values,
                                &result,
                                accepted,
                            );
                        } else if !result.message.is_empty() {
                            self.ui.delivery_failed(result.message);
                        }
                    }
                }
            }
        }
        let now = Instant::now();
        for (key, pending) in ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .iter_mut()
        {
            if !pending.unknown
                && (live_session != Some(pending.session) || pending.deadline <= now)
            {
                pending.unknown = true;
                changed = true;
            }
            if pending.unknown
                && pending.event.surface == self.surface
                && self.notified_unknown.insert(*key)
            {
                let message = "Outcome unknown: plugin stopped or acknowledgment timed out; draft retained, no automatic retry (request recovery: plugin-app-requests, F3 by default)";
                if let AppEvent::Submit(submission) = &pending.event.event {
                    self.ui.submission_failed(
                        &pending.event.state_scope,
                        &submission.widget,
                        &submission.instance,
                        message,
                    );
                } else {
                    self.ui.delivery_failed(message);
                }
                changed = true;
            }
        }
        let live: BTreeSet<_> = ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .keys()
            .copied()
            .collect();
        self.notified_unknown.retain(|key| live.contains(key));
        self.shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .retain(|key| live.contains(key));
        self.sync_budget();
        if changed {
            self.prepare();
            self.dirty = true;
        }
        changed
    }
    fn command(&mut self, command: &str) -> bool {
        match command {
            PLUGIN_APP_REQUESTS => {
                self.recovery_open = !self.recovery_open;
                self.dirty = true;
                true
            }
            PLUGIN_APP_DRAFTS => {
                self.ui.toggle_recovery();
                self.dirty = true;
                true
            }
            _ => false,
        }
    }
    fn wants_redraw(&self) -> bool {
        self.dirty
    }
    fn handle_input(&mut self, event: InputEvent) -> InputResult {
        if self.recovery_input(&event) {
            return InputResult::Consumed;
        }
        self.sync_budget();
        let outcome = self.ui.input(event);
        self.sync_budget();
        for intent in outcome.intents {
            self.send(intent);
        }
        self.dirty = true;
        outcome.result
    }
    fn render(&mut self, _area: Rect, buffer: &mut Buffer) {
        self.ui.paint(buffer);
        if self.recovery_open {
            let state = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let ledger = state
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let records: Vec<_> = ledger
                .pending
                .iter()
                .filter(|(_, pending)| pending.unknown)
                .collect();
            let selected = self.recovery_selected.min(records.len().saturating_sub(1));
            let text = records.get(selected).map_or_else(
                || "No requests have an unknown outcome.".to_owned(),
                |((session, id), pending)| {
                    format!(
                        "Record {}/{} · session {session} request {id}\n{}",
                        selected + 1,
                        records.len(),
                        serde_json::to_string_pretty(&pending.event).unwrap_or_default()
                    )
                },
            );
            Paragraph::new(text).wrap(Wrap { trim: false }).scroll((self.recovery_scroll, 0))
                .block(Block::default().borders(Borders::ALL).title("Request recovery · Up/Down inspect · PgUp/PgDn scroll · Ctrl+Delete discard · Esc close"))
                .render(self.area, buffer);
        }
        self.dirty = false;
    }
    fn cursor(&self) -> Option<(u16, u16)> {
        if self.recovery_open {
            return None;
        }
        self.ui
            .cursor()
            .map(|(x, y)| (x.saturating_sub(self.area.x), y.saturating_sub(self.area.y)))
    }
    fn status_line(&self) -> Option<String> {
        self.ui.status_line()
    }
    fn on_resize(&mut self, cols: u16, rows: u16) {
        self.area = Rect::new(0, 0, cols, rows);
        self.prepare();
        self.dirty = true;
    }
    fn on_focus(&mut self, focused: bool) {
        self.send(AppEvent::Focus { focused });
    }
    fn on_visible(&mut self, visible: bool) {
        let background = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .background;
        self.send(AppEvent::Visibility {
            visible,
            background,
        });
    }
    fn on_theme(&mut self, theme: &Theme) {
        self.ui.set_theme(theme.clone());
        self.dirty = true;
    }
    fn shutdown(&mut self) {
        self.send(AppEvent::Closed);
        self.shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .local_usage
            .remove(&self.surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::plugin_app::{Submission, Widget};
    fn view(revision: u64, id: &str) -> AppView {
        AppView {
            state_scope: "account-one".into(),
            revision,
            root: Widget::Text {
                id: id.into(),
                title: String::new(),
                text: "cached".into(),
            },
            status: String::new(),
        }
    }
    fn request() -> AppEventParams {
        AppEventParams {
            surface: "app".into(),
            session: "1".into(),
            state_scope: "account-one".into(),
            revision: 1,
            focus_ticket: None,
            event: AppEvent::Submit(Submission {
                widget: "form".into(),
                instance: "one".into(),
                values: BTreeMap::from([("title".into(), "unsaved".into())]),
                draft_version: 1,
                binding: Default::default(),
            }),
        }
    }
    fn tile(state: AppState) -> PluginTile {
        PluginTile::new(
            "plugin:p:app".into(),
            "app".into(),
            "App".into(),
            Arc::new(Mutex::new(state)),
            Theme::prism(),
        )
    }
    #[test]
    fn push_response_rejects_unknown_session_or_id() {
        let mut state = AppState::new(false);
        state.pending.insert((4, 9));
        assert!(!state.push_response(3, RpcResponse::ok(9, serde_json::json!({}))));
        assert!(!state.push_response(4, RpcResponse::ok(10, serde_json::json!({}))));
        assert!(state.push_response(4, RpcResponse::ok(9, serde_json::json!({}))));
    }

    #[test]
    fn stale_session_marks_pending_unknown_without_retry() {
        let mut state = AppState::new(false);
        state.session = Some(2);
        state.pending.insert((1, 17));
        state
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .insert(
                (1, 17),
                Pending {
                    event: request(),
                    deadline: Instant::now() + Duration::from_secs(30),
                    session: 1,
                    unknown: false,
                    bytes: 64,
                },
            );
        let mut tile = tile(state);
        assert!(tile.pump());
        let app = tile
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(app.writer.is_none());
        assert!(
            app.requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending[&(1, 17)]
                .unknown
        );
        assert!(
            app.deliveries.is_empty(),
            "stale requests are not retried or resent"
        );
    }

    #[test]
    fn request_budget_refuses_the_ninth_request() {
        with_writer(|writer| {
            let mut state = AppState::new(false);
            state.session = Some(1);
            state.writer = Some(writer);
            for _ in 0..8 {
                let mut event = request();
                event.session = state.writer.as_ref().unwrap().session_token().to_owned();
                state.request_event(event).unwrap();
            }
            let mut ninth = request();
            ninth.session = state.writer.as_ref().unwrap().session_token().to_owned();
            let error = state.request_event(ninth).unwrap_err();
            assert!(error.contains("8 requests / 8 MiB"));
        });
    }

    #[test]
    fn command_toggles_recovery_panels_and_f3_key_is_inert() {
        let mut tile = tile(AppState::new(false));
        tile.handle_input(InputEvent::key(Key::Function(3)));
        assert!(
            !tile.recovery_open,
            "F3 is handled by the host command routing"
        );
        assert!(tile.command(PLUGIN_APP_REQUESTS));
        assert!(tile.recovery_open);
        assert!(tile.command(PLUGIN_APP_DRAFTS));
        assert!(!tile.command("unknown-command"));
    }

    #[test]
    fn next_deadline_is_none_when_nothing_is_pending() {
        assert_eq!(AppState::new(false).next_deadline(), None);
    }

    #[test]
    fn apply_view_rejects_non_increasing_revision() {
        let mut state = AppState::new(false);
        state.apply_view(view(5, "first")).unwrap();
        assert!(state.apply_view(view(5, "same")).is_err());
        assert!(state.apply_view(view(4, "older")).is_err());
        assert_eq!(state.view.as_ref().unwrap().revision, 5);
    }

    #[test]
    fn invalid_and_stale_frames_preserve_cache_and_queued_valid_view() {
        let mut state = AppState::new(false);
        state.apply_view(view(2, "valid")).unwrap();
        assert!(state.apply_view(view(1, "old")).is_err());
        assert!(state.apply_view(view(3, "")).is_err());
        assert_eq!(state.view.as_ref().unwrap().revision, 2);
        assert_eq!(state.deliveries.len(), 1);
    }
    #[test]
    fn app_timeline_keeps_response_barriers_and_counts_response_bytes() {
        let mut state = AppState::new(false);
        state.pending.extend([(0, 1), (0, 2), (0, 3)]);
        state.apply_view(view(1, "first")).unwrap();
        assert!(state.push_response(0, RpcResponse::ok(1, serde_json::json!({}))));
        state.apply_view(view(2, "second")).unwrap();
        assert!(matches!(state.deliveries[0], Delivery::View(_)));
        assert!(matches!(state.deliveries[1], Delivery::Response { .. }));
        assert!(matches!(state.deliveries[2], Delivery::View(_)));
        assert!(state.push_response(
            0,
            RpcResponse::ok(2, serde_json::json!({"message":"x".repeat(900_000)}))
        ));
        assert!(state.push_response(
            0,
            RpcResponse::ok(3, serde_json::json!({"message":"x".repeat(900_000)}))
        ));
        assert!(!state.push_response(
            0,
            RpcResponse::ok(3, serde_json::json!({"message":"x".repeat(900_000)}))
        ));
        assert!(state.delivery_bytes <= 2 * 1024 * 1024);
    }
    #[test]
    fn disconnected_submissions_become_unknown_without_replay_and_can_be_discarded() {
        let mut state = AppState::new(false);
        state.pending.insert((1, 7));
        state
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .insert(
                (1, 7),
                Pending {
                    event: request(),
                    deadline: Instant::now(),
                    session: 1,
                    unknown: false,
                    bytes: 64,
                },
            );
        let mut tile = tile(state);
        assert!(tile.pump());
        assert!(tile.status_line().unwrap().contains("Outcome unknown"));
        assert_eq!(
            tile.shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .next_deadline(),
            None
        );
        assert!(
            tile.shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending[&(1, 7)]
                .unknown
        );
        tile.command(PLUGIN_APP_REQUESTS);
        tile.handle_input(InputEvent::Key {
            key: Key::Delete,
            modifiers: tg_kit::Modifiers {
                ctrl: true,
                ..tg_kit::Modifiers::NONE
            },
        });
        assert!(
            tile.shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending
                .is_empty()
        );
    }
    #[test]
    fn malformed_acknowledgment_preserves_unknown_snapshot() {
        let mut state = AppState::new(false);
        state.pending.insert((0, 7));
        state
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .insert(
                (0, 7),
                Pending {
                    event: request(),
                    deadline: Instant::now() + Duration::from_secs(30),
                    session: 0,
                    unknown: false,
                    bytes: 64,
                },
            );
        assert!(state.push_response(0, RpcResponse::ok(7, serde_json::Value::Null)));
        let mut tile = tile(state);
        tile.pump();
        assert!(
            tile.shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending[&(0, 7)]
                .unknown
        );
        assert!(
            tile.status_line()
                .unwrap()
                .contains("Malformed acknowledgment")
        );
    }
    fn with_writer(test: impl FnOnce(SessionWriter)) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let supervisor = thegn_svc::plugin::ResidentSupervisor::new(runtime.handle().clone());
        let session = thegn_svc::plugin::ResidentSession::spawn(
            &supervisor,
            "host-app-fixture",
            &["sh".into(), "-c".into(), "cat >/dev/null".into()],
            &BTreeMap::new(),
            None,
            |_| {},
        )
        .unwrap();
        let writer = session.writer();
        test(writer);
        drop(session);
        let report = runtime.block_on(
            supervisor.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(3)),
        );
        assert!(report.is_settled(), "{report:?}");
    }
    #[test]
    fn submission_admission_counts_dirty_drafts_across_app_surfaces() {
        with_writer(|writer| {
            let mut state = AppState::new(false);
            state.session = Some(1);
            state.writer = Some(writer);
            state
                .apply_view(AppView {
                    state_scope: "account-one".into(),
                    revision: 1,
                    status: String::new(),
                    root: Widget::Form {
                        id: "form".into(),
                        instance: "one".into(),
                        title: "Edit".into(),
                        fields: vec![thegn_core::plugin_app::Field {
                            id: "title".into(),
                            label: "Title".into(),
                            value: "clean".into(),
                            ..Default::default()
                        }],
                        submit: "Save".into(),
                        binding: Default::default(),
                    },
                })
                .unwrap();
            state
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .local_usage
                .insert("another-app".into(), (7, 0));
            let mut tile = tile(state);
            tile.pump();
            tile.handle_input(InputEvent::Key {
                key: Key::Enter,
                modifiers: tg_kit::Modifiers {
                    ctrl: true,
                    ..tg_kit::Modifiers::NONE
                },
            });
            assert!(
                tile.shared
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .requests
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .pending
                    .is_empty(),
                "a pending snapshot plus eight dirty drafts cannot exceed the shared record budget"
            );
            assert!(tile.status_line().unwrap().contains("limit"));
        });
    }
    #[test]
    fn epoch_changes_resend_grid_viewport_without_physical_resize() {
        with_writer(|writer| {
            let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
            let output = seen.clone();
            writer
                .observe_outbound(Arc::new(move |frame| {
                    output
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(serde_json::from_slice(frame).unwrap())
                }))
                .unwrap();
            let mut state = AppState::new(false);
            state.session = Some(1);
            state.writer = Some(writer);
            state.epoch = 1;
            state
                .apply_view(AppView {
                    state_scope: "account-one".into(),
                    revision: 1,
                    status: String::new(),
                    root: Widget::Grid {
                        id: "grid".into(),
                        width: 80,
                        height: 24,
                        cells: vec![],
                        cursor: None,
                    },
                })
                .unwrap();
            let mut tile = tile(state);
            tile.pump();
            tile.on_resize(80, 24);
            seen.lock()
                .unwrap_or_else(|error| error.into_inner())
                .clear();
            tile.shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .epoch = 2;
            tile.pump();
            assert!(
                seen.lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .iter()
                    .any(|frame| frame["params"]["payload"]["event"] == "resize"
                        && frame["params"]["payload"]["data"]["width"] == 80)
            );
        });
    }
    #[test]
    fn reload_rebinds_shared_state_without_dropping_unknown_snapshots() {
        let mut original = AppState::new(false);
        original.apply_view(view(1, "old")).unwrap();
        original.pending.insert((1, 7));
        original
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .insert(
                (1, 7),
                Pending {
                    event: request(),
                    deadline: Instant::now() + Duration::from_secs(30),
                    session: 1,
                    unknown: false,
                    bytes: 64,
                },
            );
        let mut tile = tile(original);
        let old_ledger = tile
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .requests
            .clone();
        let mut replacement = AppState::new(false);
        replacement.apply_view(view(2, "new")).unwrap();
        let replacement = Arc::new(Mutex::new(replacement));
        *tile
            .rebind_handle()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(("app".into(), "Renamed".into(), replacement.clone()));
        tile.pump();
        assert!(Arc::ptr_eq(&tile.shared, &replacement));
        assert!(Arc::ptr_eq(
            &old_ledger,
            &replacement
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .requests
        ));
        assert!(
            old_ledger
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending[&(1, 7)]
                .unknown
        );
        assert_eq!(tile.ui.view().unwrap().revision, 2);
        assert_eq!(tile.title(), "Renamed");
    }
    #[test]
    fn discarded_recoveries_prune_all_correlation_keys() {
        let mut state = AppState::new(false);
        for id in 1..=8 {
            state.pending.insert((1, id));
            state
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending
                .insert(
                    (1, id),
                    Pending {
                        event: request(),
                        deadline: Instant::now(),
                        session: 1,
                        unknown: true,
                        bytes: 1,
                    },
                );
        }
        let mut tile = tile(state);
        tile.pump();
        tile.command(PLUGIN_APP_REQUESTS);
        for _ in 0..8 {
            tile.handle_input(InputEvent::Key {
                key: Key::Delete,
                modifiers: tg_kit::Modifiers {
                    ctrl: true,
                    ..tg_kit::Modifiers::NONE
                },
            });
            tile.pump();
        }
        assert!(tile.notified_unknown.is_empty());
        assert!(
            tile.shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending
                .is_empty()
        );
        assert!(
            tile.shared
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .pending
                .is_empty()
        );
    }
}
