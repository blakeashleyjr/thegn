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
        request_timeout: Duration,
    ) -> Self {
        Self {
            ui: UiEngine::new(Theme::prism()),
            surface,
            writer,
            inbox,
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
            SessionEvent::Message(message) => {
                if let Some(id) = message.id {
                    push_event(&mut inbox, Incoming::Request { id });
                }
            }
            SessionEvent::Response(response) => {
                push_event(&mut inbox, Incoming::Response(response))
            }
            SessionEvent::Junk(_) => push_event(&mut inbox, Incoming::Junk),
            SessionEvent::Exit { code } => inbox.exited = Some(code),
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
        let intents = self.ui.prepare(area);
        self.apply_intents(intents);
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
        let intents = self.ui.prepare(self.area);
        self.apply_intents(intents);
        self.dirty = true;
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|pending| pending.deadline).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use thegn_core::plugin_app::AppEvent;
    use thegn_svc::plugin::{ResidentSession, ResidentSupervisor};

    #[allow(
        clippy::disallowed_macros,
        reason = "report when the optional Python runtime is unavailable"
    )]
    #[expect(
        clippy::disallowed_methods,
        reason = "probe whether the optional Python runtime is installed"
    )]
    fn python_available() -> bool {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 is not installed");
            false
        } else {
            true
        }
    }

    struct Harness {
        _temp: tempfile::TempDir,
        runtime: tokio::runtime::Runtime,
        supervisor: ResidentSupervisor,
        session: ResidentSession,
        writer: SessionWriter,
        tile: LiveTile,
        updates: mpsc::Receiver<()>,
    }

    impl Harness {
        fn new(mode: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let command = if mode == "request" {
                let script = temp.path().join("request.py");
                std::fs::write(
                    &script,
                    r#"import json, sys
print(json.dumps({"id":7,"method":"host.call","params":{"capability":"sessions.list","params":{}}}), flush=True)
for line in sys.stdin:
    message = json.loads(line)
    if message.get("id") == 7:
        denied = message.get("error", {}).get("code") == "denied"
        app = {"state_scope":"fixture","revision":1,"status":"Synthetic","root":{"kind":"text","id":"text","title":"Fixture","text":"denied" if denied else "allowed"}}
        print(json.dumps({"method":"update","params":{"surface":"fixture.workspace","app":app}}), flush=True)
    if message.get("method") == "deactivate":
        break
"#,
                )
                .unwrap();
                vec!["python3".to_string(), script.display().to_string()]
            } else if mode == "view-then-crash" {
                let script = temp.path().join("view_then_crash.py");
                std::fs::write(
                    &script,
                    r#"import json
app = {"state_scope":"fixture","revision":1,"status":"Synthetic","root":{"kind":"text","id":"text","title":"Fixture","text":"Ready"}}
print(json.dumps({"method":"update","params":{"surface":"fixture.workspace","app":app}}), flush=True)
raise SystemExit(3)
"#,
                )
                .unwrap();
                vec!["python3".to_string(), script.display().to_string()]
            } else {
                let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../examples/plugins/fixtures/fault.py");
                vec![
                    "python3".to_string(),
                    script.display().to_string(),
                    "--mode".to_string(),
                    mode.to_string(),
                ]
            };
            let manifest = format!(
                "id = \"live_tile_test\"\nname = \"Live tile test\"\nversion = \"0.1.0\"\napi = \"0.4.0\"\nmode = \"resident\"\ncapabilities = [\"surface:app\"]\ncommand = {}\n\n[[contributions]]\nid = \"workspace\"\nextension_point = \"AppTab\"\nlabel = \"Live\"\nsurface = \"fixture.workspace\"\n",
                serde_json::to_string(&command).unwrap(),
            );
            std::fs::write(temp.path().join("plugin.toml"), manifest).unwrap();
            let (plugin, surface) = super::super::live::load_plugin(temp.path()).unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let supervisor = ResidentSupervisor::new(runtime.handle().clone());
            let inbox = Arc::new(Mutex::new(Inbox::default()));
            let callback_inbox = inbox.clone();
            let (update_tx, updates) = mpsc::channel();
            let hook: ChangeHook = Arc::new(move || {
                let _ = update_tx.send(());
            });
            let callback_hook = hook.clone();
            let callback_surface = surface.clone();
            let session = ResidentSession::spawn(
                &supervisor,
                "live_tile_test",
                &plugin.spec.command,
                &BTreeMap::new(),
                plugin.effective_cwd().as_deref(),
                move |event| {
                    on_session_event(&callback_inbox, &callback_surface, &callback_hook, event);
                },
            )
            .unwrap();
            let writer = session.writer();
            writer
                .notify_control(
                    PluginCallback::Activate,
                    serde_json::json!({
                        "api": thegn_core::plugin_api::API_VERSION,
                        "session": writer.session_token(),
                        "granted": ["surface:app"],
                        "background": false
                    }),
                )
                .unwrap();
            let tile = LiveTile::new(surface, writer.clone(), inbox, REQUEST_ACK_TIMEOUT);
            Self {
                _temp: temp,
                runtime,
                supervisor,
                session,
                writer,
                tile,
                updates,
            }
        }

        fn wait_for_change(&self) {
            self.updates
                .recv_timeout(Duration::from_secs(3))
                .expect("plugin should signal a session event");
        }

        fn close(self) {
            super::super::live::close_session(
                self.runtime,
                self.supervisor,
                self.session,
                &self.writer,
            )
            .unwrap();
        }
    }

    fn view(revision: u64) -> AppView {
        AppView {
            state_scope: "fixture".into(),
            revision,
            status: "Synthetic".into(),
            root: thegn_core::plugin_app::Widget::Text {
                id: "text".into(),
                title: "Fixture".into(),
                text: format!("revision {revision}"),
            },
        }
    }

    fn update_message(view: AppView) -> thegn_svc::plugin::SessionEvent {
        thegn_svc::plugin::SessionEvent::Message(thegn_core::plugin_api::RpcMessage {
            id: None,
            method: "update".into(),
            params: serde_json::json!({"surface":"fixture.workspace", "app":view}),
        })
    }

    #[test]
    fn live_tile_pump_applies_the_first_view_and_pulses_the_hook_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let hook: ChangeHook = {
            let calls = calls.clone();
            Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
            })
        };
        let inbox = Mutex::new(Inbox::default());
        on_session_event(&inbox, "fixture.workspace", &hook, update_message(view(1)));
        on_session_event(&inbox, "fixture.workspace", &hook, update_message(view(2)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        if !python_available() {
            return;
        }
        let mut harness = Harness::new("normal");
        harness.wait_for_change();
        assert!(harness.tile.pump());
        assert_eq!(harness.tile.ui.view().unwrap().revision, 1);
        harness.close();
    }

    #[test]
    fn live_tile_coalesces_a_view_flood_to_the_newest_revision() {
        let inbox = Mutex::new(Inbox::default());
        let hook: ChangeHook = Arc::new(|| {});
        for revision in 1..=2_000 {
            on_session_event(
                &inbox,
                "fixture.workspace",
                &hook,
                update_message(view(revision)),
            );
        }
        assert_eq!(
            inbox.lock().unwrap().latest_view.as_ref().unwrap().revision,
            2_000
        );
        if !python_available() {
            return;
        }
        let mut harness = Harness::new("flood");
        harness.wait_for_change();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            harness.tile.pump();
            if harness
                .tile
                .ui
                .view()
                .is_some_and(|view| view.revision == 2_000)
            {
                break;
            }
            let _ = harness.updates.recv_timeout(Duration::from_millis(20));
        }
        assert_eq!(harness.tile.ui.view().unwrap().revision, 2_000);
        harness.close();
    }

    #[test]
    fn live_tile_denies_plugin_requests() {
        if !python_available() {
            return;
        }
        let mut harness = Harness::new("request");
        harness.wait_for_change();
        harness.tile.pump();
        for _ in 0..50 {
            if harness.tile.pump()
                && harness
                    .tile
                    .ui
                    .view()
                    .is_some_and(|view| matches!(&view.root, thegn_core::plugin_app::Widget::Text { text, .. } if text == "denied"))
            {
                break;
            }
            let _ = harness.updates.recv_timeout(Duration::from_millis(20));
        }
        assert!(matches!(
            &harness.tile.ui.view().unwrap().root,
            thegn_core::plugin_app::Widget::Text { text, .. } if text == "denied"
        ));
        harness.close();
    }

    #[test]
    fn live_tile_exit_marks_the_status_and_keeps_the_last_view() {
        if !python_available() {
            return;
        }
        let mut harness = Harness::new("view-then-crash");
        harness.wait_for_change();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            harness.tile.pump();
            if harness
                .tile
                .status_line()
                .is_some_and(|status| status.contains("plugin exited"))
            {
                break;
            }
            let _ = harness.updates.recv_timeout(Duration::from_millis(20));
        }
        assert_eq!(harness.tile.ui.view().unwrap().revision, 1);
        assert!(
            harness
                .tile
                .status_line()
                .unwrap()
                .contains("plugin exited")
        );
        harness.close();
    }

    #[test]
    fn live_tile_forwards_input_events_to_the_plugin() {
        if !python_available() {
            return;
        }
        let mut harness = Harness::new("normal");
        harness.wait_for_change();
        harness.tile.pump();
        harness.tile.send(AppEvent::Invoke { id: "run".into() });
        for _ in 0..50 {
            if harness.tile.pump() && harness.tile.pending.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            harness.tile.pending.is_empty(),
            "plugin response should be processed"
        );
        harness.close();
    }

    #[test]
    fn live_tile_reliable_event_times_out_without_an_acknowledgment() {
        if !python_available() {
            return;
        }
        let mut harness = Harness::new("no-read");
        harness.tile.send(AppEvent::Invoke { id: "run".into() });
        assert_eq!(harness.tile.pending.len(), 1);
        assert!(harness.tile.next_deadline().unwrap() >= Instant::now() + Duration::from_secs(9));
        for pending in harness.tile.pending.values_mut() {
            pending.deadline = Instant::now() - Duration::from_millis(1);
        }
        harness.tile.pump();
        assert!(harness.tile.pending.is_empty());
        assert!(
            harness
                .tile
                .ui
                .status_line()
                .unwrap()
                .contains("Outcome unknown")
        );
        harness.close();
    }

    #[test]
    fn live_tile_ctrl_q_exits() {
        if !python_available() {
            return;
        }
        let mut harness = Harness::new("normal");
        assert_eq!(
            harness.tile.handle_input(InputEvent::Key {
                key: Key::Char('q'),
                modifiers: tg_kit::Modifiers {
                    ctrl: true,
                    alt: false,
                    shift: false,
                },
            }),
            InputResult::Exit
        );
        harness.close();
    }

    #[test]
    fn live_tile_queue_is_bounded() {
        let mut inbox = Inbox::default();
        for _ in 0..(MAX_PENDING_EVENTS + 9) {
            push_event(&mut inbox, Incoming::Junk);
        }
        assert_eq!(inbox.events.len(), MAX_PENDING_EVENTS);
        assert_eq!(inbox.dropped, 9);
    }
}
