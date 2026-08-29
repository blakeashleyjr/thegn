//! The control API's gRPC surface (feature `control-grpc`) — a thin tonic
//! adapter over the same [`ControlApi`] + [`auth`] seams as the HTTP surface.
//!
//! Auth mirrors HTTP: every RPC resolves the caller through one chokepoint
//! (`GrpcControl::authed`) that reads the `authorization` metadata (bearer
//! token) — or grants implicit admin on a `local_admin` listener — and checks
//! `required_scope` BEFORE calling in, so a rejected request performs no
//! action. Event frames are a mechanical `EventFrame` ↔ proto conversion,
//! round-trip tested below.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_util::Stream;
use tonic::{Request, Response, Status};

use thegn_core::control::{Scope, Verb, required_scope};
use thegn_core::control_audit::{AuditOutcome, AuditRecord, is_audited};
use thegn_core::control_wire::{EventFrame, LeaseEventKind, PairingState};
use thegn_core::store::ControlStore;

use super::auth::{self, AuthCtx};
use super::{
    AttachKind, BrowserAction, BrowserCommand, ControlApi, ControlError, ForkSpec, OpenSpec,
    SplitDir, WaitCondition,
};

/// Generated bindings for `thegn.control.v1` (see `proto/…/control.proto`).
#[allow(clippy::all, clippy::pedantic)]
pub mod proto {
    tonic::include_proto!("thegn.control.v1");
}

use proto::control_server::Control;
pub use proto::control_server::ControlServer;

/// The tonic service: the same state the HTTP router carries.
pub struct GrpcControl {
    pub api: Arc<dyn ControlApi>,
    pub store: Arc<Mutex<dyn ControlStore + Send>>,
    /// This listener's peers get implicit admin (unix socket, same uid).
    pub local_admin: bool,
    pub server_label: String,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl From<ControlError> for Status {
    fn from(e: ControlError) -> Status {
        match &e {
            ControlError::NotFound(_) => Status::not_found(e.to_string()),
            ControlError::NoScope { .. } => Status::permission_denied(e.to_string()),
            ControlError::Conflict(_) => Status::aborted(e.to_string()),
            ControlError::Unimplemented(_) => Status::unimplemented(e.to_string()),
            ControlError::Internal(_) => Status::internal(e.to_string()),
        }
    }
}

impl GrpcControl {
    /// Authenticate + enforce the verb's scope — the single gRPC chokepoint.
    /// Every mutating verb (write/git/admin) and every auth/scope rejection
    /// emits one audit record on `thegn::control::audit`, exactly like the HTTP
    /// adapter. The target resource is not threaded per-RPC here (gRPC carries
    /// it in the request body); the record still names the caller, capability
    /// and outcome.
    // The Err IS the RPC's whole response; produced once per request.
    #[allow(clippy::result_large_err)]
    fn authed<T>(&self, req: &Request<T>, verb: Verb) -> Result<AuthCtx, Status> {
        let ctx = if self.local_admin {
            AuthCtx::local_admin()
        } else {
            let Some(token) = req
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::trim)
                .map(str::to_string)
            else {
                grpc_audit("", "", verb, AuditOutcome::Unauthorized);
                return Err(Status::unauthenticated("missing bearer token"));
            };
            let store = self.store.lock().expect("control store lock");
            match auth::verify(&*store, &token, now_ms()) {
                Some(ctx) => ctx,
                None => {
                    drop(store);
                    grpc_audit("", "", verb, AuditOutcome::Unauthorized);
                    return Err(Status::unauthenticated("invalid or revoked token"));
                }
            }
        };
        if let Err(e) = ctx.require(required_scope(verb)) {
            grpc_audit(&ctx.pairing_id, &ctx.label, verb, AuditOutcome::NoScope);
            return Err(Status::from(e));
        }
        if is_audited(verb) {
            grpc_audit(&ctx.pairing_id, &ctx.label, verb, AuditOutcome::Ok);
        }
        Ok(ctx)
    }
}

/// Emit one control audit record (gRPC adapter). `target` is not threaded
/// per-RPC; the record still names caller + capability + outcome. Never a
/// secret — only the public pairing id half.
fn grpc_audit(pairing_id: &str, label: &str, verb: Verb, outcome: AuditOutcome) {
    let rec = AuditRecord::for_verb(pairing_id, label, verb, "", outcome);
    tracing::info!(
        target: "thegn::control::audit",
        pairing_id = rec.pairing_id,
        label = rec.label,
        capability = rec.capability,
        scope = rec.scope.as_str(),
        resource = rec.target,
        outcome = rec.outcome.as_str(),
    );
}

/// Default program for a `split` with no argv: the daemon's login shell.
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn scopes_csv(ctx: &AuthCtx) -> String {
    ctx.scopes.to_csv()
}

/// `EventFrame` → proto `Event` (mechanical; the reverse exists for tests).
pub fn frame_to_proto(frame: &EventFrame) -> proto::Event {
    use proto::event::Kind;
    let kind = match frame {
        EventFrame::Hello(h) => Kind::Hello(proto::Hello {
            proto: h.proto,
            server: h.server.clone(),
            scopes: h
                .scopes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(","),
        }),
        EventFrame::PaneSnapshot {
            session,
            seq,
            cols,
            rows,
            bytes,
        } => Kind::Snapshot(proto::PaneSnapshot {
            session: session.clone(),
            seq: *seq,
            cols: u32::from(*cols),
            rows: u32::from(*rows),
            bytes: bytes.clone(),
        }),
        EventFrame::PaneDelta {
            session,
            seq,
            bytes,
        } => Kind::Delta(proto::PaneDelta {
            session: session.clone(),
            seq: *seq,
            bytes: bytes.clone(),
        }),
        EventFrame::Activity { json } => Kind::Activity(proto::Activity { json: json.clone() }),
        EventFrame::Lease {
            session,
            kind,
            expires_at,
        } => Kind::Lease(proto::LeaseEvent {
            session: session.clone(),
            kind: match kind {
                LeaseEventKind::Opened => "opened",
                LeaseEventKind::Refreshed => "refreshed",
                LeaseEventKind::Released => "released",
                LeaseEventKind::Reaped => "reaped",
            }
            .to_string(),
            expires_at: *expires_at,
        }),
        EventFrame::Pairing {
            pairing_id,
            label,
            scope,
            state,
        } => Kind::Pairing(proto::PairingEvent {
            pairing_id: pairing_id.clone(),
            label: label.clone(),
            scopes: scope.clone(),
            state: match state {
                PairingState::Requested => "requested",
                PairingState::Approved => "approved",
                PairingState::Revoked => "revoked",
            }
            .to_string(),
        }),
        EventFrame::Sessions => Kind::Sessions(proto::Empty {}),
        EventFrame::SessionExit { session, code } => Kind::Exit(proto::SessionExit {
            session: session.clone(),
            code: *code,
        }),
    };
    proto::Event {
        seq: match frame {
            EventFrame::PaneSnapshot { seq, .. } | EventFrame::PaneDelta { seq, .. } => *seq,
            _ => 0,
        },
        kind: Some(kind),
    }
}

fn info_to_proto(i: &super::SessionInfo) -> proto::SessionInfo {
    proto::SessionInfo {
        id: i.id.clone(),
        worktree: i.worktree.clone().unwrap_or_default(),
        program: i.program.clone(),
        cwd: i.cwd.clone().unwrap_or_default(),
        rows: u32::from(i.rows),
        cols: u32::from(i.cols),
        created_at_ms: i.created_at_ms,
        attached_clients: i.attached_clients,
        lease_expires_at: i.lease_expires_at,
        error_active: i.error_active,
        forked_from: i.forked_from.clone(),
    }
}

type EventStream = Pin<Box<dyn Stream<Item = Result<proto::Event, Status>> + Send>>;

#[tonic::async_trait]
impl Control for GrpcControl {
    async fn list_sessions(
        &self,
        req: Request<proto::ListSessionsRequest>,
    ) -> Result<Response<proto::ListSessionsReply>, Status> {
        self.authed(&req, Verb::ListSessions)?;
        let sessions = self.api.list_sessions().await.map_err(Status::from)?;
        Ok(Response::new(proto::ListSessionsReply {
            sessions: sessions.iter().map(info_to_proto).collect(),
        }))
    }

    type AttachStream = EventStream;

    async fn attach(
        &self,
        req: Request<proto::AttachRequest>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        self.authed(&req, Verb::Attach)?;
        let r = req.into_inner();
        let kind = if r.observer {
            AttachKind::Observer
        } else {
            AttachKind::Interactive
        };
        let reply = self
            .api
            .attach(
                &r.client_id,
                &r.session,
                kind,
                r.rows.min(u16::MAX as u32) as u16,
                r.cols.min(u16::MAX as u32) as u16,
                // The proto has no history flag yet; gRPC attaches are always
                // fresh clients, so the full-context snapshot is correct.
                true,
            )
            .await
            .map_err(Status::from)?;
        let snapshot = frame_to_proto(&reply.snapshot);
        let mut frames = reply.frames;
        let stream = async_stream(move |tx| async move {
            let _ = tx.send(Ok(snapshot)).await; // best-effort: client may be gone
            while let Some(f) = frames.recv().await {
                if tx.send(Ok(frame_to_proto(&f))).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(stream))
    }

    async fn detach(
        &self,
        req: Request<proto::DetachRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.authed(&req, Verb::Detach)?;
        let r = req.into_inner();
        self.api
            .detach(&r.client_id, &r.session)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::Empty {}))
    }

    async fn open_session(
        &self,
        req: Request<proto::OpenSessionRequest>,
    ) -> Result<Response<proto::SessionInfo>, Status> {
        self.authed(&req, Verb::OpenSession)?;
        let r = req.into_inner();
        let spec = OpenSpec {
            argv: r.argv,
            cwd: (!r.cwd.is_empty()).then_some(r.cwd),
            env: r.env.into_iter().map(|e| (e.key, e.value)).collect(),
            rows: r.rows.min(u16::MAX as u32) as u16,
            cols: r.cols.min(u16::MAX as u32) as u16,
            worktree: (!r.worktree.is_empty()).then_some(r.worktree),
            // `OpenSessionRequest` has no agent/adopt fields, so a gRPC caller
            // gets the raw-argv path and the daemon caps it (see the
            // `sessions.open` SURFACE_GAPS entry).
            ..Default::default()
        };
        let info = self.api.open(spec).await.map_err(Status::from)?;
        Ok(Response::new(info_to_proto(&info)))
    }

    async fn fork_session(
        &self,
        req: Request<proto::ForkSessionRequest>,
    ) -> Result<Response<proto::SessionInfo>, Status> {
        self.authed(&req, Verb::ForkSession)?;
        let r = req.into_inner();
        let info = self
            .api
            .fork(ForkSpec {
                session: r.session,
                harness: (!r.harness.is_empty()).then_some(r.harness),
                agent: (!r.agent.is_empty()).then_some(r.agent),
                cwd: (!r.cwd.is_empty()).then_some(r.cwd),
                worktree: (!r.worktree.is_empty()).then_some(r.worktree),
                scrollback: r.scrollback,
                adopt: r.adopt,
                tab: r.tab,
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(info_to_proto(&info)))
    }

    async fn send_input(
        &self,
        req: Request<proto::SendInputRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.authed(&req, Verb::SendInput)?;
        let r = req.into_inner();
        self.api
            .send_input(&r.session, r.bytes)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::Empty {}))
    }

    async fn resize(
        &self,
        req: Request<proto::ResizeRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.authed(&req, Verb::Resize)?;
        let r = req.into_inner();
        self.api
            .resize(
                &r.session,
                r.rows.min(u16::MAX as u32) as u16,
                r.cols.min(u16::MAX as u32) as u16,
            )
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::Empty {}))
    }

    async fn snapshot(
        &self,
        req: Request<proto::SnapshotRequest>,
    ) -> Result<Response<proto::Event>, Status> {
        self.authed(&req, Verb::Snapshot)?;
        let r = req.into_inner();
        let frame = self.api.snapshot(&r.session).await.map_err(Status::from)?;
        Ok(Response::new(frame_to_proto(&frame)))
    }

    async fn kill_session(
        &self,
        req: Request<proto::KillSessionRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.authed(&req, Verb::KillSession)?;
        let r = req.into_inner();
        self.api.kill(&r.session).await.map_err(Status::from)?;
        Ok(Response::new(proto::Empty {}))
    }

    async fn open_worktree(
        &self,
        req: Request<proto::OpenWorktreeRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.authed(&req, Verb::OpenWorktree)?;
        let r = req.into_inner();
        self.api
            .open_worktree(&r.repo, (!r.branch.is_empty()).then_some(r.branch.as_str()))
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::Empty {}))
    }

    async fn list_worktrees(
        &self,
        req: Request<proto::ListWorktreesRequest>,
    ) -> Result<Response<proto::ListWorktreesReply>, Status> {
        self.authed(&req, Verb::ListWorktrees)?;
        let worktrees = self.api.list_worktrees().await.map_err(Status::from)?;
        Ok(Response::new(proto::ListWorktreesReply {
            worktrees: worktrees
                .into_iter()
                .map(|w| proto::WorktreeInfo {
                    path: w.path,
                    branch: w.branch,
                    repo_root: w.repo_root,
                    location: w.location,
                    created_at: w.created_at,
                })
                .collect(),
        }))
    }

    async fn wait(
        &self,
        req: Request<proto::WaitRequest>,
    ) -> Result<Response<proto::WaitReply>, Status> {
        self.authed(&req, Verb::Wait)?;
        let r = req.into_inner();
        let cond = match r.condition_kind.as_str() {
            "idle" => WaitCondition::Idle,
            "blocked" => WaitCondition::Blocked,
            "done" => WaitCondition::Done,
            "output_matches" => WaitCondition::OutputMatches { regex: r.regex },
            // Default (and the explicit "exited"): wait for the PTY to exit.
            _ => WaitCondition::Exited,
        };
        let outcome = self
            .api
            .wait(&r.session, cond, r.timeout_ms)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::WaitReply {
            matched: outcome.matched,
            condition: outcome.condition,
            exit_code: outcome.exit_code,
        }))
    }

    async fn split(
        &self,
        req: Request<proto::SplitRequest>,
    ) -> Result<Response<proto::SessionInfo>, Status> {
        self.authed(&req, Verb::Split)?;
        let r = req.into_inner();
        let dir = if r.dir == "down" {
            SplitDir::Down
        } else {
            SplitDir::Right
        };
        let argv = if r.argv.is_empty() {
            vec![default_shell()]
        } else {
            r.argv
        };
        let spec = OpenSpec {
            argv,
            cwd: (!r.cwd.is_empty()).then_some(r.cwd),
            env: r.env.into_iter().map(|e| (e.key, e.value)).collect(),
            rows: r.rows.min(u16::MAX as u32) as u16,
            cols: r.cols.min(u16::MAX as u32) as u16,
            ..Default::default()
        };
        let info = self
            .api
            .split(&r.session, dir, spec)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(info_to_proto(&info)))
    }

    async fn drive_browser(
        &self,
        req: Request<proto::DriveBrowserRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.authed(&req, Verb::DriveBrowser)?;
        let r = req.into_inner();
        let action = match r.action {
            Some(proto::drive_browser_request::Action::NavigateUrl(url)) => {
                BrowserAction::Navigate { url }
            }
            Some(proto::drive_browser_request::Action::Back(_)) => BrowserAction::Back,
            _ => BrowserAction::Reload,
        };
        self.api
            .drive_browser(BrowserCommand {
                session: (!r.session.is_empty()).then_some(r.session),
                action,
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::Empty {}))
    }

    async fn git_status(
        &self,
        req: Request<proto::GitStatusRequest>,
    ) -> Result<Response<proto::GitStatusReply>, Status> {
        self.authed(&req, Verb::GitStatus)?;
        let r = req.into_inner();
        let files = self
            .api
            .git_status(&r.worktree)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::GitStatusReply {
            files: files
                .into_iter()
                .map(|f| proto::GitFileStatus {
                    path: f.path,
                    code: f.code,
                })
                .collect(),
        }))
    }

    async fn git_stage(
        &self,
        req: Request<proto::GitStageRequest>,
    ) -> Result<Response<proto::Empty>, Status> {
        self.authed(&req, Verb::GitStage)?;
        let r = req.into_inner();
        self.api
            .git_stage(&r.worktree, &r.paths)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::Empty {}))
    }

    async fn git_commit(
        &self,
        req: Request<proto::GitCommitRequest>,
    ) -> Result<Response<proto::GitCommitReply>, Status> {
        self.authed(&req, Verb::GitCommit)?;
        let r = req.into_inner();
        let commit = self
            .api
            .git_commit(&r.worktree, &r.message)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::GitCommitReply { commit }))
    }

    async fn merge_list(
        &self,
        req: Request<proto::MergeListRequest>,
    ) -> Result<Response<proto::MergeListReply>, Status> {
        self.authed(&req, Verb::MergeList)?;
        let r = req.into_inner();
        let rows = self
            .api
            .merge_list(&r.worktree)
            .await
            .map_err(Status::from)?;
        let rows_json =
            serde_json::to_string(&rows).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(proto::MergeListReply { rows_json }))
    }

    async fn merge_add(
        &self,
        req: Request<proto::MergeAddRequest>,
    ) -> Result<Response<proto::MergeAddReply>, Status> {
        self.authed(&req, Verb::MergeAdd)?;
        let r = req.into_inner();
        let message = self
            .api
            .merge_add(&r.worktree)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::MergeAddReply { message }))
    }

    async fn merge_clear(
        &self,
        req: Request<proto::MergeClearRequest>,
    ) -> Result<Response<proto::MergeClearReply>, Status> {
        self.authed(&req, Verb::MergeClear)?;
        let r = req.into_inner();
        let cleared = self
            .api
            .merge_clear(&r.worktree)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::MergeClearReply {
            cleared: cleared as u64,
        }))
    }

    async fn calendar_events(
        &self,
        req: Request<proto::CalendarEventsRequest>,
    ) -> Result<Response<proto::CalendarEventsReply>, Status> {
        self.authed(&req, Verb::CalendarEvents)?;
        let r = req.into_inner();
        let events = self
            .api
            .calendar_events(&r.from, &r.to)
            .await
            .map_err(Status::from)?;
        let events_json =
            serde_json::to_string(&events).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(proto::CalendarEventsReply { events_json }))
    }

    async fn calendar_clocks(
        &self,
        req: Request<proto::CalendarClocksRequest>,
    ) -> Result<Response<proto::CalendarClocksReply>, Status> {
        self.authed(&req, Verb::CalendarClocks)?;
        let clocks = self.api.calendar_clocks().await.map_err(Status::from)?;
        let clocks_json =
            serde_json::to_string(&clocks).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(proto::CalendarClocksReply { clocks_json }))
    }

    async fn calendar_ingest(
        &self,
        req: Request<proto::CalendarIngestRequest>,
    ) -> Result<Response<proto::CalendarIngestReply>, Status> {
        self.authed(&req, Verb::CalendarIngest)?;
        let r = req.into_inner();
        let events: Vec<thegn_core::calendar::CalEvent> = serde_json::from_str(&r.events_json)
            .map_err(|e| Status::invalid_argument(format!("events_json: {e}")))?;
        let stored = self
            .api
            .calendar_ingest(&r.account, events)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::CalendarIngestReply {
            stored: stored as u64,
        }))
    }

    type EventsStream = EventStream;

    async fn events(
        &self,
        req: Request<proto::EventsRequest>,
    ) -> Result<Response<Self::EventsStream>, Status> {
        let ctx = self.authed(&req, Verb::Events)?;
        let hello = frame_to_proto(&EventFrame::Hello(thegn_core::control_wire::Hello {
            proto: thegn_core::control_wire::PROTO_VERSION,
            server: self.server_label.clone(),
            scopes: [Scope::Read, Scope::Write, Scope::Git, Scope::Admin]
                .into_iter()
                .filter(|s| ctx.scopes.contains(*s))
                .collect(),
        }));
        let mut rx = self.api.subscribe();
        let stream = async_stream(move |tx| async move {
            let _ = tx.send(Ok(hello)).await; // best-effort: client may be gone
            loop {
                match rx.recv().await {
                    Ok(frame) => {
                        if tx.send(Ok(frame_to_proto(&frame))).await.is_err() {
                            return;
                        }
                    }
                    // A lagged monitor skips events; pane bytes ride Attach.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(Response::new(stream))
    }

    async fn lease_status(
        &self,
        req: Request<proto::LeaseStatusRequest>,
    ) -> Result<Response<proto::LeaseStatusReply>, Status> {
        self.authed(&req, Verb::LeaseStatus)?;
        let rows = self.api.lease_status().await.map_err(Status::from)?;
        Ok(Response::new(proto::LeaseStatusReply {
            leases: rows
                .into_iter()
                .map(|l| proto::Lease {
                    lease_id: l.lease_id,
                    session: l.session_id,
                    kind: l.kind,
                    client: l.client_id.unwrap_or_default(),
                    expires_at: l.expires_at,
                })
                .collect(),
        }))
    }

    async fn pr_status(
        &self,
        req: Request<proto::PrStatusRequest>,
    ) -> Result<Response<proto::PrStatusReply>, Status> {
        self.authed(&req, Verb::PrStatus)?;
        let rows = self.api.pr_status().await.map_err(Status::from)?;
        Ok(Response::new(proto::PrStatusReply {
            prs: rows
                .into_iter()
                .map(|r| proto::PrStatusRow {
                    worktree: r.worktree,
                    branch: r.branch,
                    number: r.number,
                    title: r.title,
                    state: r.state,
                    url: r.url,
                    is_draft: r.is_draft,
                    fetched_at: r.fetched_at,
                })
                .collect(),
        }))
    }

    async fn notify_push(
        &self,
        req: Request<proto::NotifyPushRequest>,
    ) -> Result<Response<proto::NotifyPushReply>, Status> {
        self.authed(&req, Verb::NotifyPush)?;
        let r = req.into_inner();
        let id = self
            .api
            .notify_push(super::PushedNote {
                title: r.title,
                body: r.body,
                urgency: (!r.urgency.is_empty()).then_some(r.urgency),
                source: (!r.source.is_empty()).then_some(r.source),
            })
            .await
            .map_err(Status::from)?;
        Ok(Response::new(proto::NotifyPushReply { id }))
    }

    async fn me(&self, req: Request<proto::MeRequest>) -> Result<Response<proto::MeReply>, Status> {
        let ctx = self.authed(&req, Verb::Me)?;
        Ok(Response::new(proto::MeReply {
            pairing_id: ctx.pairing_id.clone(),
            label: ctx.label.clone(),
            scopes: scopes_csv(&ctx),
        }))
    }
}

/// Bridge a producer closure onto a boxed tonic response stream.
fn async_stream<F, Fut>(f: F) -> EventStream
where
    F: FnOnce(tokio::sync::mpsc::Sender<Result<proto::Event, Status>>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(f(tx));
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// Every host capability the gRPC mirror implements, by catalog id. One entry
/// per `Control` service method above; the coverage test pins it against
/// `CATALOG` so a verb added to HTTP but not mirrored here must be excused
/// in `SURFACE_GAPS` (and a mirrored one must have its excuse removed).
pub const GRPC_CAPS: &[&str] = &[
    "sessions.list",
    "sessions.attach",
    "sessions.detach",
    "sessions.open",
    "sessions.fork",
    "sessions.input",
    "sessions.resize",
    "sessions.snapshot",
    "sessions.kill",
    "sessions.wait",
    "sessions.split",
    "worktrees.list",
    "worktrees.open",
    "browser.drive",
    "git.status",
    "git.stage",
    "git.commit",
    "merge.list",
    "merge.add",
    "merge.clear",
    "calendar.events",
    "calendar.clocks",
    "calendar.ingest",
    "events.subscribe",
    "leases.list",
    "me",
    "pr.status",
    "notify.push",
];

#[cfg(test)]
mod tests {
    #[test]
    fn grpc_methods_cover_catalog() {
        let problems = thegn_core::capability::coverage_problems(
            thegn_core::capability::Surface::Grpc,
            super::GRPC_CAPS,
        );
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    use super::*;
    use thegn_core::control_wire::Hello;

    /// proto `Event` → `EventFrame`, for the round-trip test (lossy on
    /// unknown strings by construction — the wire enums are ours).
    fn proto_to_frame(e: &proto::Event) -> EventFrame {
        use proto::event::Kind;
        match e.kind.as_ref().expect("kind") {
            Kind::Hello(h) => EventFrame::Hello(Hello {
                proto: h.proto,
                server: h.server.clone(),
                scopes: h
                    .scopes
                    .split(',')
                    .filter_map(|s| match s {
                        "read" => Some(Scope::Read),
                        "write" => Some(Scope::Write),
                        "git" => Some(Scope::Git),
                        "exec" => Some(Scope::Exec),
                        "admin" => Some(Scope::Admin),
                        _ => None,
                    })
                    .collect(),
            }),
            Kind::Snapshot(s) => EventFrame::PaneSnapshot {
                session: s.session.clone(),
                seq: s.seq,
                cols: s.cols as u16,
                rows: s.rows as u16,
                bytes: s.bytes.clone(),
            },
            Kind::Delta(d) => EventFrame::PaneDelta {
                session: d.session.clone(),
                seq: d.seq,
                bytes: d.bytes.clone(),
            },
            Kind::Activity(a) => EventFrame::Activity {
                json: a.json.clone(),
            },
            Kind::Lease(l) => EventFrame::Lease {
                session: l.session.clone(),
                kind: match l.kind.as_str() {
                    "opened" => LeaseEventKind::Opened,
                    "refreshed" => LeaseEventKind::Refreshed,
                    "released" => LeaseEventKind::Released,
                    _ => LeaseEventKind::Reaped,
                },
                expires_at: l.expires_at,
            },
            Kind::Pairing(p) => EventFrame::Pairing {
                pairing_id: p.pairing_id.clone(),
                label: p.label.clone(),
                scope: p.scopes.clone(),
                state: match p.state.as_str() {
                    "requested" => PairingState::Requested,
                    "approved" => PairingState::Approved,
                    _ => PairingState::Revoked,
                },
            },
            Kind::Sessions(_) => EventFrame::Sessions,
            Kind::Exit(x) => EventFrame::SessionExit {
                session: x.session.clone(),
                code: x.code,
            },
        }
    }

    #[test]
    fn every_frame_round_trips_through_proto() {
        let frames = vec![
            EventFrame::Hello(Hello {
                proto: 1,
                server: "h thegn 0.1".into(),
                scopes: vec![Scope::Read, Scope::Git],
            }),
            EventFrame::PaneSnapshot {
                session: "s".into(),
                seq: 9,
                cols: 80,
                rows: 24,
                bytes: b"\x1b[2J".to_vec(),
            },
            EventFrame::PaneDelta {
                session: "s".into(),
                seq: 10,
                bytes: vec![0, 255, 3],
            },
            EventFrame::Activity {
                json: r#"{"k":1}"#.into(),
            },
            EventFrame::Lease {
                session: "s".into(),
                kind: LeaseEventKind::Reaped,
                expires_at: Some(5),
            },
            EventFrame::Pairing {
                pairing_id: "p".into(),
                label: "phone".into(),
                scope: "read".into(),
                state: PairingState::Requested,
            },
            EventFrame::Sessions,
            EventFrame::SessionExit {
                session: "s".into(),
                code: Some(1),
            },
            EventFrame::SessionExit {
                session: "s".into(),
                code: None,
            },
        ];
        for f in frames {
            assert_eq!(proto_to_frame(&frame_to_proto(&f)), f, "{f:?}");
        }
    }
}
