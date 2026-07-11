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
use thegn_core::control_wire::{EventFrame, LeaseEventKind, PairingState};
use thegn_core::store::ControlStore;

use super::auth::{self, AuthCtx};
use super::{AttachKind, BrowserAction, BrowserCommand, ControlApi, ControlError, OpenSpec};

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
    // The Err IS the RPC's whole response; produced once per request.
    #[allow(clippy::result_large_err)]
    fn authed<T>(&self, req: &Request<T>, verb: Verb) -> Result<AuthCtx, Status> {
        let ctx = if self.local_admin {
            AuthCtx::local_admin()
        } else {
            let token = req
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::trim)
                .ok_or_else(|| Status::unauthenticated("missing bearer token"))?
                .to_string();
            let store = self.store.lock().expect("control store lock");
            auth::verify(&*store, &token, now_ms())
                .ok_or_else(|| Status::unauthenticated("invalid or revoked token"))?
        };
        ctx.require(required_scope(verb)).map_err(Status::from)?;
        Ok(ctx)
    }
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
            )
            .await
            .map_err(Status::from)?;
        let snapshot = frame_to_proto(&reply.snapshot);
        let mut frames = reply.frames;
        let stream = async_stream(move |tx| async move {
            let _ = tx.send(Ok(snapshot)).await;
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
        };
        let info = self.api.open(spec).await.map_err(Status::from)?;
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
            let _ = tx.send(Ok(hello)).await;
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

#[cfg(test)]
mod tests {
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
