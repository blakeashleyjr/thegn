//! Interactive adapter for a resident plugin preview.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tg_kit::ratatui::{buffer::Buffer, layout::Rect};
use tg_kit::{AppTile, ChangeHook, InputEvent, InputResult, Key, Theme};
use thegn_core::plugin_api::{PluginCallback, RpcError, RpcErrorCode, RpcResponse};
use thegn_core::plugin_app::{AppEvent, AppEventParams, AppEventResult, AppView};
use thegn_plugin_ui::UiEngine;
use thegn_svc::plugin::{SessionEvent, SessionWriter};

pub(super) const MAX_PENDING_EVENTS: usize = 256;
pub(super) const REQUEST_ACK_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const MAX_OUTSTANDING_REQUESTS: usize = thegn_svc::plugin::session::MAX_QUEUED_FRAMES;

#[derive(Default)]
pub(super) struct Inbox {
    latest_view: Option<AppView>,
    rejected_views: u64,
    events: VecDeque<Incoming>,
    dropped: u64,
    exited: Option<Option<i32>>,
    signalled: bool,
}

enum Incoming {
    Response(RpcResponse),
    Request { id: u64 },
    Junk,
}

struct Pending {
    event: AppEventParams,
    deadline: Instant,
}

pub(super) struct LiveTile {
    ui: UiEngine,
    surface: String,
    writer: SessionWriter,
    inbox: Arc<Mutex<Inbox>>,
    hook: ChangeHook,
    dirty: bool,
    area: Rect,
    pending: BTreeMap<u64, Pending>,
    status_note: Option<String>,
    request_timeout: Duration,
}

impl LiveTile {
    pub(super) fn new(
        surface: String,
        writer: SessionWriter,
        inbox: Arc<Mutex<Inbox>>,
        hook: ChangeHook,
        request_timeout: Duration,
    ) -> Self {
        Self {
            ui: UiEngine::new(Theme::prism()),
            surface,
            writer,
            inbox,
            hook,
            dirty: true,
            area: Rect::default(),
            pending: BTreeMap::new(),
            status_note: None,
            request_timeout,
        }
    }

    fn send(&mut self, event: AppEvent) {
        let params = AppEventParams {
            surface: self.surface.clone(),
            session: self.writer.session_token().to_owned(),
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
            self.dirty = true;
            return;
        }
        let wire = serde_json::json!({"kind":"App","payload":params});
        if params.event.reliable() {
            if self.pending.len() >= MAX_OUTSTANDING_REQUESTS {
                self.ui.delivery_failed("too many unacknowledged events");
                self.dirty = true;
                return;
            }
            match self.writer.request(PluginCallback::OnEvent, wire) {
                Ok(id) => {
                    if let AppEvent::Submit(submission) = &params.event {
                        self.ui
                            .mark_submitted(&submission.widget, &submission.instance);
                    }
                    self.pending.insert(
                        id,
                        Pending {
                            event: params,
                            deadline: Instant::now() + self.request_timeout,
                        },
                    );
                }
                Err(error) => {
                    if let AppEvent::Submit(submission) = &params.event {
                        self.ui.submission_failed(
                            &params.state_scope,
                            &submission.widget,
                            &submission.instance,
                            &error.to_string(),
                        );
                    } else {
                        self.ui
                            .delivery_failed(format!("Input was not delivered: {error}"));
                    }
                }
            }
        } else {
            let result = match &params.event {
                AppEvent::Change(change) => self.writer.notify_coalesced(
                    PluginCallback::OnEvent,
                    &format!(
                        "{}:change:{}:{}",
                        self.surface, params.state_scope, change.widget
                    ),
                    wire,
                ),
                AppEvent::Select(selection) => self.writer.notify_coalesced(
                    PluginCallback::OnEvent,
                    &format!("{}:select:{}", self.surface, selection.widget),
                    wire,
                ),
                AppEvent::Resize { widget, .. } => self.writer.notify_coalesced(
                    PluginCallback::OnEvent,
                    &format!("{}:resize:{widget}", self.surface),
                    wire,
                ),
                AppEvent::Visibility { .. } | AppEvent::Focus { .. } | AppEvent::Closed => {
                    self.writer.notify_control(PluginCallback::OnEvent, wire)
                }
                _ => self.writer.notify(PluginCallback::OnEvent, wire),
            };
            if let Err(error) = result {
                self.ui
                    .delivery_failed(format!("Input was not delivered: {error}"));
            }
        }
        self.dirty = true;
    }

    fn apply_intents(&mut self, intents: Vec<AppEvent>) {
        for event in intents {
            self.send(event);
        }
    }

    fn handle_response(&mut self, response: RpcResponse) {
        let Some(pending) = self.pending.remove(&response.id) else {
            return;
        };
        if let Some(error) = response.error {
            self.ui.delivery_failed(error.message);
        } else if let AppEvent::Submit(submission) = &pending.event.event {
            let result = response
                .result
                .and_then(|value| serde_json::from_value::<AppEventResult>(value).ok());
            if let Some(result) = result {
                self.ui.acknowledge(
                    &pending.event.state_scope,
                    &submission.widget,
                    &submission.instance,
                    submission.draft_version,
                    &submission.values,
                    &result,
                    true,
                );
            } else {
                self.ui
                    .delivery_failed("Malformed acknowledgment; outcome unknown");
            }
        }
        self.dirty = true;
    }
}

/// Queue one session event and pulse the harness only on the first unseen change.
pub(super) fn on_session_event(
    inbox: &Mutex<Inbox>,
    surface: &str,
    hook: &ChangeHook,
    event: SessionEvent,
) {
    let should_signal = {
        let mut inbox = inbox.lock().unwrap_or_else(|error| error.into_inner());
        match event {
            SessionEvent::Message(message)
                if message.method == "update"
                    && message
                        .params
                        .get("surface")
                        .and_then(serde_json::Value::as_str)
                        == Some(surface) =>
            {
                let view = message
                    .params
                    .get("app")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<AppView>(value).ok());
                match view {
                    Some(view) if view.validate().is_ok() => inbox.latest_view = Some(view),
                    _ => inbox.rejected_views = inbox.rejected_views.saturating_add(1),
                }
            }
            SessionEvent::Message(message) if let Some(id) = message.id => {
                push_event(&mut inbox, Incoming::Request { id });
            }
            SessionEvent::Response(response) => {
                push_event(&mut inbox, Incoming::Response(response))
            }
            SessionEvent::Junk(_) => push_event(&mut inbox, Incoming::Junk),
            SessionEvent::Exit { code } => inbox.exited = Some(code),
            SessionEvent::Message(_) => {}
        }
        let changed = inbox.latest_view.is_some()
            || !inbox.events.is_empty()
            || inbox.rejected_views > 0
            || inbox.exited.is_some();
        let signal = changed && !inbox.signalled;
        if signal {
            inbox.signalled = true;
        }
        signal
    };
    if should_signal {
        hook();
    }
}

fn push_event(inbox: &mut Inbox, event: Incoming) {
    if inbox.events.len() >= MAX_PENDING_EVENTS {
        inbox.dropped = inbox.dropped.saturating_add(1);
    } else {
        inbox.events.push_back(event);
    }
}

impl AppTile for LiveTile {
    fn id(&self) -> &str {
        "plugin-preview"
    }

    fn title(&self) -> String {
        "Plugin preview (live)".into()
    }

    fn pump(&mut self) -> bool {
        let (latest_view, events, rejected, dropped, exited) = {
            let mut inbox = self.inbox.lock().unwrap_or_else(|error| error.into_inner());
            inbox.signalled = false;
            (
                inbox.latest_view.take(),
                std::mem::take(&mut inbox.events),
                std::mem::take(&mut inbox.rejected_views),
                std::mem::take(&mut inbox.dropped),
                inbox.exited.take(),
            )
        };
        let mut changed = false;
        if let Some(view) = latest_view {
            if let Err(error) = self.ui.replace_view(view) {
                self.ui.delivery_failed(error);
            }
            changed = true;
        }
        for event in events {
            match event {
                Incoming::Response(response) => self.handle_response(response),
                Incoming::Request { id } => {
                    let response = RpcResponse::err(
                        id,
                        RpcError::new(RpcErrorCode::Denied, "plugin preview grants no host calls"),
                    );
                    if let Err(error) = self.writer.respond(&response) {
                        tracing::debug!(target: "thegn::plugin::preview", %error, "denial not delivered");
                    }
                    changed = true;
                }
                Incoming::Junk => changed = true,
            }
        }
        if rejected > 0 {
            self.status_note = Some(format!("{rejected} invalid plugin view(s) rejected"));
            changed = true;
        }
        if dropped > 0 {
            self.status_note = Some(format!("{dropped} events dropped"));
            changed = true;
        }
        if let Some(code) = exited {
            self.status_note = Some(format!("plugin exited (code {code:?}); Ctrl+Q closes"));
            changed = true;
        }
        let now = Instant::now();
        let expired = self
            .pending
            .iter()
            .filter_map(|(id, pending)| (pending.deadline <= now).then_some(*id))
            .collect::<Vec<_>>();
        for id in expired {
            if let Some(pending) = self.pending.remove(&id) {
                if let AppEvent::Submit(submission) = &pending.event.event {
                    self.ui.submission_failed(
                        &pending.event.state_scope,
                        &submission.widget,
                        &submission.instance,
                        "Outcome unknown: no acknowledgment within 10 s",
                    );
                } else {
                    self.ui
                        .delivery_failed("Outcome unknown: no acknowledgment within 10 s");
                }
                changed = true;
            }
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    fn wants_redraw(&self) -> bool {
        self.dirty
    }

    fn handle_input(&mut self, event: InputEvent) -> InputResult {
        if matches!(&event, InputEvent::Key { key: Key::Char('q'), modifiers } if modifiers.ctrl) {
            return InputResult::Exit;
        }
        let outcome = self.ui.input(event);
        self.apply_intents(outcome.intents);
        self.dirty = true;
        outcome.result
    }

    fn render(&mut self, area: Rect, buffer: &mut Buffer) {
        self.area = area;
        self.apply_intents(self.ui.prepare(area));
        self.ui.paint(buffer);
        self.dirty = false;
    }

    fn cursor(&self) -> Option<(u16, u16)> {
        self.ui.cursor()
    }

    fn status_line(&self) -> Option<String> {
        self.status_note
            .clone()
            .or_else(|| {
                self.ui
                    .view()
                    .is_none()
                    .then(|| "waiting for the plugin's first view".into())
            })
            .or_else(|| self.ui.status_line())
    }

    fn on_resize(&mut self, width: u16, height: u16) {
        self.area = Rect::new(0, 0, width, height);
        self.apply_intents(self.ui.prepare(self.area));
        self.dirty = true;
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|pending| pending.deadline).min()
    }
}
