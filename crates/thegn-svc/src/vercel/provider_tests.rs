//! Provider-level tests over an in-memory control plane: the spend-safety
//! ledger, destroy, wake rules, the env allowlist and the inventory page cap.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::*;
use crate::provider::ExecSpec;

/// One scripted control plane. Every call is recorded as `"<op> <arg>"`.
#[derive(Default)]
pub(super) struct FakeApi {
    pub(super) calls: Mutex<Vec<String>>,
    /// The create outcome (`Err(text)` renders as an anyhow error).
    pub(super) create: Mutex<Option<std::result::Result<SandboxView, String>>>,
    /// `get` answers, consumed front to back; the last one repeats.
    pub(super) gets: Mutex<VecDeque<Option<SandboxView>>>,
    /// The resume (`get … resume=true`) answer.
    pub(super) resume: Mutex<Option<SandboxView>>,
    /// List pages; when exhausted, `endless` keeps returning a fresh cursor.
    pub(super) pages: Mutex<VecDeque<(Vec<SandboxView>, Option<String>)>>,
    pub(super) endless: bool,
    pub(super) delete_fails: bool,
    pub(super) snapshots: Vec<String>,
    /// Every create and run body sent, in order.
    pub(super) bodies: Mutex<Vec<serde_json::Value>>,
    /// Name-keyed answers for `get` and (when no page is scripted) `list`.
    pub(super) by_name: Mutex<BTreeMap<String, SandboxView>>,
    /// Set by the conformance suite: the ledger file whose existence `create`
    /// records, and what it saw.
    pub(super) ledger_probe: Mutex<Option<std::path::PathBuf>>,
    pub(super) ledger_seen: Mutex<Option<bool>>,
}

impl FakeApi {
    pub(super) fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    pub(super) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub(super) fn count(&self, op: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.split(' ').next() == Some(op))
            .count()
    }
}

impl Api for FakeApi {
    fn create<'a>(&'a self, body: &'a serde_json::Value) -> BoxFuture<'a, Result<SandboxView>> {
        Box::pin(async move {
            self.record("create".into());
            if let Some(path) = self.ledger_probe.lock().unwrap().clone() {
                *self.ledger_seen.lock().unwrap() = Some(path.exists());
            }
            self.bodies.lock().unwrap().push(body.clone());
            match self.create.lock().unwrap().clone() {
                Some(Ok(view)) => {
                    if self.ledger_probe.lock().unwrap().is_some() {
                        self.by_name
                            .lock()
                            .unwrap()
                            .insert(view.name.clone(), view.clone());
                    }
                    Ok(view)
                }
                Some(Err(text)) => Err(anyhow!(text)),
                None => Err(anyhow!("fake: no create scripted")),
            }
        })
    }

    fn get<'a>(
        &'a self,
        name: &'a str,
        resume: bool,
    ) -> BoxFuture<'a, Result<Option<SandboxView>>> {
        Box::pin(async move {
            if resume {
                self.record(format!("resume {name}"));
                return Ok(self.resume.lock().unwrap().clone());
            }
            self.record(format!("get {name}"));
            if let Some(view) = self.by_name.lock().unwrap().get(name).cloned() {
                return Ok(Some(view));
            }
            let mut gets = self.gets.lock().unwrap();
            let view = if gets.len() > 1 {
                gets.pop_front().flatten()
            } else {
                gets.front().cloned().flatten()
            };
            Ok(view)
        })
    }

    fn list_page<'a>(
        &'a self,
        cursor: Option<&'a str>,
        _name_prefix: &'a str,
    ) -> BoxFuture<'a, Result<(Vec<SandboxView>, Option<String>)>> {
        Box::pin(async move {
            self.record(format!("list {}", cursor.unwrap_or("-")));
            if let Some(page) = self.pages.lock().unwrap().pop_front() {
                return Ok(page);
            }
            if !self.endless && !self.by_name.lock().unwrap().is_empty() {
                let rows = self.by_name.lock().unwrap().values().cloned().collect();
                return Ok((rows, None));
            }
            if self.endless {
                let n = self.count("list");
                return Ok((Vec::new(), Some(format!("c{n}"))));
            }
            Ok((Vec::new(), None))
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.record(format!("delete {name}"));
            if self.delete_fails {
                Err(anyhow!("fake: delete failed (503 Service Unavailable)"))
            } else {
                // A deleted sandbox leaves the name-keyed listing, as on the
                // real control plane (only the conformance suite fills it).
                self.by_name.lock().unwrap().remove(name);
                Ok(())
            }
        })
    }

    fn stop<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.record(format!("stop {session}"));
            Ok(())
        })
    }

    fn extend<'a>(
        &'a self,
        session: &'a str,
        ms: u64,
    ) -> BoxFuture<'a, Result<client::ExtendOutcome>> {
        Box::pin(async move {
            self.record(format!("extend {session} {ms}"));
            Ok(client::ExtendOutcome::Extended)
        })
    }

    fn set_network_policy<'a>(
        &'a self,
        name: &'a str,
        _body: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.record(format!("policy {name}"));
            Ok(())
        })
    }

    fn run_wait<'a>(
        &'a self,
        session: &'a str,
        body: &'a serde_json::Value,
        _out_cap: usize,
    ) -> BoxFuture<'a, Result<(i32, Vec<u8>)>> {
        Box::pin(async move {
            self.record(format!("run {session}"));
            self.bodies.lock().unwrap().push(body.clone());
            Ok((0, b"ok".to_vec()))
        })
    }

    fn snapshots_for<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            self.record(format!("snapshots {name}"));
            Ok(self.snapshots.clone())
        })
    }

    fn delete_snapshot<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.record(format!("delete_snapshot {id}"));
            Ok(())
        })
    }
    fn usage<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<SessionUsage>>> {
        Box::pin(async move {
            self.record(format!("usage {name}"));
            Ok(None)
        })
    }
    fn snapshot_usage<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Vec<SnapshotUsage>>> {
        Box::pin(async move {
            self.record(format!("snapshot_usage {name}"));
            Ok(vec![])
        })
    }
}

pub(super) const NAME: &str = "tg-vercel-test";

pub(super) fn spec(max_instances: u32) -> VercelSpec {
    VercelSpec {
        api_base: String::new(),
        token: VercelToken::new("vcp_test_token".into()),
        credential_ref: "VERCEL_TOKEN".into(),
        scope: Scope {
            team_id: "team_test".into(),
            project_id: "prj_test".into(),
        },
        name: NAME.into(),
        vcpus: 2,
        image: None,
        region: None,
        max_instances,
        max_lifetime_secs: 0,
        interactive_host_suffixes: Vec::new(),
        model_credentials: thegn_core::config::ModelCredentials::None,
    }
}

pub(super) fn provider(fake: &Arc<FakeApi>, ledger: &std::path::Path, cap: u32) -> VercelProvider {
    let api: Arc<dyn Api> = fake.clone();
    VercelProvider::with_client_for_test(spec(cap), api, ledger.to_path_buf())
        .expect("test provider admits its spec")
}

pub(super) fn view(name: &str, status: SessionStatus, session: Option<&str>) -> SandboxView {
    SandboxView {
        name: name.into(),
        status,
        session_id: session.map(str::to_string),
        vcpus: Some(2),
        region: Some("iad1".into()),
        created_at_ms: Some(1_700_000_000_000),
        tags: BTreeMap::new(),
        cwd: None,
    }
}

/// A listed sandbox carrying this controller's ownership tag.
pub(super) fn tagged(name: &str, controller: &str) -> SandboxView {
    let mut tags = policy::tags_for(controller, "owner", "host");
    if tags.is_empty() {
        tags.insert("thegn-controller".into(), controller.into());
    }
    SandboxView {
        tags,
        ..view(name, SessionStatus::Running, Some("sess_x"))
    }
}

fn ledger_record(name: &str, state: &str) -> registry::VpsRecord {
    registry::VpsRecord {
        name: name.into(),
        provider: LEDGER_PROVIDER.into(),
        state: state.into(),
        instance_id: String::new(),
        ip: String::new(),
        created_at: thegn_core::util::now(),
        owner: String::new(),
        account: String::new(),
    }
}

#[tokio::test]
async fn create_at_cap_is_refused_before_any_http_call() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    registry::write_at(ledger.path(), &ledger_record("tg-vercel-a", "ready")).unwrap();
    registry::write_at(ledger.path(), &ledger_record("tg-vercel-b", "ready")).unwrap();
    let fake = Arc::new(FakeApi::default());
    let error = provider(&fake, ledger.path(), 2)
        .create()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("max_instances = 2"), "{error:#}");
    assert!(
        fake.calls().is_empty(),
        "no request at cap: {:?}",
        fake.calls()
    );
    assert!(registry::read_at(ledger.path(), NAME).is_none());
}

#[tokio::test]
async fn remote_tagged_sandboxes_count_against_the_hard_cap() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let controller = crate::vps::controller::controller_id().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.pages.lock().unwrap().push_back((
        vec![
            tagged("tg-other-host-a", &controller),
            tagged("tg-other-host-b", &controller),
            // Another controller's sandbox is not ours and does not count.
            tagged("tg-foreign", "some-other-controller"),
        ],
        None,
    ));
    let error = provider(&fake, ledger.path(), 2)
        .create()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("max_instances = 2"), "{error:#}");
    assert_eq!(fake.count("create"), 0, "{:?}", fake.calls());
    assert!(registry::read_at(ledger.path(), NAME).is_none());
}

#[tokio::test]
async fn create_4xx_clears_the_intent() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    *fake.create.lock().unwrap() = Some(Err(
        "vercel POST /v2/sandboxes failed (400 Bad Request)".into()
    ));
    let error = provider(&fake, ledger.path(), 2)
        .create()
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("400"), "{error:#}");
    assert_eq!(fake.count("create"), 1);
    assert_eq!(fake.count("delete"), 0, "a refused create made nothing");
    assert!(
        registry::read_at(ledger.path(), NAME).is_none(),
        "a definite 4xx must clear the intent"
    );
}

#[tokio::test]
async fn failure_after_create_with_failed_delete_keeps_the_record() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let controller = crate::vps::controller::controller_id().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi {
        delete_fails: true,
        ..FakeApi::default()
    });
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(tagged(NAME, &controller)));
    *fake.create.lock().unwrap() = Some(Err("vercel: connection reset by peer".into()));
    provider(&fake, ledger.path(), 2)
        .create()
        .await
        .unwrap_err();
    assert_eq!(
        fake.count("delete"),
        1,
        "cleanup DELETE fires: {:?}",
        fake.calls()
    );
    let kept = registry::read_at(ledger.path(), NAME).expect("intent kept for reconciliation");
    assert_eq!(kept.state, "creating");
    assert_eq!(kept.provider, "vercel");
}

#[tokio::test]
async fn failure_after_create_with_landed_delete_clears_the_record() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let controller = crate::vps::controller::controller_id().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(tagged(NAME, &controller)));
    *fake.create.lock().unwrap() = Some(Err("vercel: request timed out".into()));
    provider(&fake, ledger.path(), 2)
        .create()
        .await
        .unwrap_err();
    assert_eq!(fake.count("delete"), 1);
    assert!(registry::read_at(ledger.path(), NAME).is_none());
}

#[tokio::test]
async fn cleanup_never_deletes_an_untagged_sandbox() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    *fake.create.lock().unwrap() = Some(Err("vercel: request timed out".into()));
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(tagged(NAME, "other-controller")));
    provider(&fake, ledger.path(), 2)
        .create()
        .await
        .unwrap_err();
    assert_eq!(fake.count("delete"), 0, "{:?}", fake.calls());
    assert_eq!(fake.count("delete_snapshot"), 0, "{:?}", fake.calls());
    assert!(registry::read_at(ledger.path(), NAME).is_some());
}

#[tokio::test]
async fn create_conflict_keeps_the_record() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    *fake.create.lock().unwrap() = Some(Err("vercel create failed (409 Conflict)".into()));
    provider(&fake, ledger.path(), 2)
        .create()
        .await
        .unwrap_err();
    // A 409 means the name exists already: it is never deleted, and the
    // intent stays for reconciliation.
    assert_eq!(fake.count("delete"), 0, "{:?}", fake.calls());
    assert!(registry::read_at(ledger.path(), NAME).is_some());
}

#[tokio::test]
async fn create_records_ready_and_returns_the_bridge_handle() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    *fake.create.lock().unwrap() = Some(Ok(view(NAME, SessionStatus::Running, Some("sess_1"))));
    let handle = provider(&fake, ledger.path(), 2).create().await.unwrap();
    assert_eq!(handle.id, NAME);
    let ExecKind::Command(argv) = handle.exec else {
        panic!("vercel rides the self-bridge");
    };
    assert_eq!(&argv[1..], ["vercel-exec", NAME, "--"]);
    let record = registry::read_at(ledger.path(), NAME).expect("ready record");
    assert_eq!(record.state, "ready");
    assert_eq!(record.instance_id, NAME);
    assert_eq!(record.created_at, 1_700_000_000);
    assert!(record.account.starts_with("acct1-"));
}

#[tokio::test]
async fn destroy_deletes_the_snapshots() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let controller = crate::vps::controller::controller_id().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    registry::write_at(ledger.path(), &ledger_record(NAME, "ready")).unwrap();
    let fake = Arc::new(FakeApi {
        snapshots: vec!["snap_1".into(), "snap_2".into()],
        ..FakeApi::default()
    });
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(tagged(NAME, &controller)));
    provider(&fake, ledger.path(), 2)
        .destroy(NAME)
        .await
        .unwrap();
    assert_eq!(
        fake.calls(),
        [
            format!("get {NAME}"),
            format!("delete {NAME}"),
            format!("snapshots {NAME}"),
            "delete_snapshot snap_1".to_string(),
            "delete_snapshot snap_2".to_string(),
        ]
    );
    assert!(registry::read_at(ledger.path(), NAME).is_none());
}

#[tokio::test]
async fn wake_never_on_a_stopped_sandbox_errors_without_resuming() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(view(NAME, SessionStatus::Stopped, None)));
    let p = provider(&fake, ledger.path(), 2);
    let error = p.session_for(NAME, Wake::Never).await.unwrap_err();
    assert_eq!(error.to_string(), STOPPED_TEXT);
    let error = p
        .run_exec(NAME, &["true".to_string()], None, &[], Wake::Never)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), STOPPED_TEXT);
    assert_eq!(fake.count("resume"), 0, "{:?}", fake.calls());
    assert_eq!(fake.count("run"), 0);
}

#[tokio::test]
async fn wake_allow_resumes_a_stopped_sandbox() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(view(NAME, SessionStatus::Stopped, None)));
    *fake.resume.lock().unwrap() = Some(view(NAME, SessionStatus::Running, Some("sess_2")));
    let p = provider(&fake, ledger.path(), 2);
    assert_eq!(p.session_for(NAME, Wake::Allow).await.unwrap(), "sess_2");
    assert_eq!(fake.count("resume"), 1);
}

#[tokio::test]
async fn interactive_open_never_resumes_past_the_lifetime_ceiling() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();

    // (a) Created long ago: refused, never resumed.
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(view(NAME, SessionStatus::Stopped, None)));
    let p = provider(&fake, ledger.path(), 2);
    let error = p.interactive_session(NAME).await.unwrap_err();
    assert!(format!("{error:#}").contains("lifetime"), "{error:#}");
    assert_eq!(fake.count("resume"), 0, "{:?}", fake.calls());

    // (b) Created a minute ago: resumed as usual.
    let fake = Arc::new(FakeApi::default());
    fake.gets.lock().unwrap().push_back(Some(SandboxView {
        created_at_ms: Some(now_ms() - 60_000),
        ..view(NAME, SessionStatus::Stopped, None)
    }));
    *fake.resume.lock().unwrap() = Some(view(NAME, SessionStatus::Running, Some("sess_2")));
    let p = provider(&fake, ledger.path(), 2);
    assert_eq!(p.interactive_session(NAME).await.unwrap(), "sess_2");
    assert_eq!(fake.count("resume"), 1);
}

/// The `vercel-exec` login upload resumes through `resume_interactive`: past
/// the lifetime ceiling it refuses before any resume (no compute is bought).
#[tokio::test]
async fn login_upload_resume_never_resumes_past_the_lifetime_ceiling() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(view(NAME, SessionStatus::Stopped, None)));
    *fake.resume.lock().unwrap() = Some(view(NAME, SessionStatus::Running, Some("sess_9")));
    let p = provider(&fake, ledger.path(), 2);
    let error = p.resume_interactive(NAME).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("max_lifetime_secs"),
        "{error:#}"
    );
    assert_eq!(fake.count("resume"), 0, "{:?}", fake.calls());

    // Inside the ceiling it resumes as `resume` would.
    let fake = Arc::new(FakeApi::default());
    fake.gets.lock().unwrap().push_back(Some(SandboxView {
        created_at_ms: Some(now_ms() - 60_000),
        ..view(NAME, SessionStatus::Stopped, None)
    }));
    *fake.resume.lock().unwrap() = Some(view(NAME, SessionStatus::Running, Some("sess_9")));
    let p = provider(&fake, ledger.path(), 2);
    p.resume_interactive(NAME).await.unwrap();
    assert_eq!(fake.count("resume"), 1);
}

#[tokio::test]
async fn stop_session_never_resumes() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(view(NAME, SessionStatus::Running, Some("sess_3"))));
    provider(&fake, ledger.path(), 2)
        .stop_session(NAME)
        .await
        .unwrap();
    assert_eq!(fake.calls(), [format!("get {NAME}"), "stop sess_3".into()]);
}

#[test]
fn env_allowlist_drops_credentials() {
    // Input data for the filter, not an environment write: named through a
    // const so the state-env ratchet's tuple pattern does not misread it.
    const HOME_VAR: &str = "HOME";
    let env: Vec<(String, String)> = [
        ("VERCEL_TOKEN", "v"),
        ("ANTHROPIC_API_KEY", "a"),
        ("GITHUB_TOKEN", "g"),
        ("THEGN_API_TOKEN", "t"),
        ("THEGN_SIGNING_KEY", "k"),
        ("THEGN_CLIENT_SECRET", "s"),
        (HOME_VAR, "/root"),
        ("TERM", "xterm-256color"),
        ("LANG", "C.UTF-8"),
        ("LC_ALL", "C.UTF-8"),
        ("COLORTERM", "truecolor"),
        ("THEGN_WORKTREE", "wt"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let kept: Vec<String> = env_allowlisted(&env, "VERCEL_TOKEN")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        kept,
        ["TERM", "LANG", "LC_ALL", "COLORTERM", "THEGN_WORKTREE"]
    );
}

#[tokio::test]
async fn inventory_page_cap_gives_partial() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi {
        endless: true,
        ..FakeApi::default()
    });
    let p = provider(&fake, ledger.path(), 2);
    match p.sandbox_inventory().await.unwrap() {
        InventoryReport::Partial { .. } => {}
        InventoryReport::Complete(_) => panic!("a page-cap hit must never be Complete"),
    }
    assert_eq!(fake.count("list"), MAX_LIST_PAGES);
    assert!(
        p.list().await.is_err(),
        "list() refuses a partial inventory"
    );
}

#[tokio::test]
async fn inventory_lists_only_our_tagged_sandboxes() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let controller = crate::vps::controller::controller_id().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.pages.lock().unwrap().push_back((
        vec![tagged("tg-ours", &controller), tagged("tg-theirs", "other")],
        Some("next".into()),
    ));
    fake.pages
        .lock()
        .unwrap()
        .push_back((vec![tagged("tg-ours-2", &controller)], None));
    let p = provider(&fake, ledger.path(), 2);
    let InventoryReport::Complete(rows) = p.sandbox_inventory().await.unwrap() else {
        panic!("two pages and no cursor is complete");
    };
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["tg-ours", "tg-ours-2"]);
    assert_eq!(rows[0].state.as_deref(), Some("running"));
    assert_eq!(rows[0].size.as_deref(), Some("2vcpu"));
}

#[tokio::test]
async fn block_only_egress_is_refused_and_allow_lists_are_cached() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    let p = provider(&fake, ledger.path(), 2);
    assert!(
        p.set_network_policy(NAME, &[], &["evil.example".into()])
            .await
            .is_err()
    );
    assert_eq!(fake.count("policy"), 0);
    assert!(
        p.set_network_policy(NAME, &["*.github.com".into()], &["gist.github.com".into()])
            .await
            .is_err()
    );
    assert_eq!(fake.count("policy"), 0);
    p.set_network_policy(NAME, &["github.com".into()], &[])
        .await
        .unwrap();
    let rules = p.get_network_policy(NAME).await.unwrap();
    assert!(
        rules
            .iter()
            .any(|r| r.domain == "github.com" && r.action == "allow")
    );
}

#[test]
fn create_rejection_classification() {
    let definite = |t: &str| definite_create_rejection(&anyhow!(t.to_string()));
    assert!(definite("create failed (400 Bad Request)"));
    assert!(definite("create failed (HTTP 422)"));
    assert!(!definite("create failed (409 Conflict)"));
    assert!(!definite("create failed (429 Too Many Requests)"));
    assert!(!definite("create failed (503 Service Unavailable)"));
    assert!(!definite("connection reset by peer"));
}

/// A templated env sandbox name (`{repo}-{worktree}-{hash}`): no `tg-` prefix.
const TEMPLATED: &str = "thegn-feature-x-a1b2c3";

fn credential_env() -> Vec<(String, String)> {
    [
        ("GITHUB_TOKEN", "g"),
        ("ANTHROPIC_API_KEY", "a"),
        ("AWS_SECRET_ACCESS_KEY", "s"),
        ("THEGN_SESSION_SECRET", "t"),
        ("THEGN_ROUTE_AUTH", "r"),
        ("VERCEL_TOKEN", "v"),
        ("THEGN_VERCEL_REF", "ref-value"),
        ("TERM", "xterm-256color"),
        ("THEGN_WORKTREE", "wt"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn env_keys(body: &serde_json::Value) -> Vec<String> {
    body.get("env")
        .and_then(|e| e.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn every_sandbox_env_path_drops_credentials_and_the_token_ref() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(view(NAME, SessionStatus::Running, Some("sess_e"))));
    let mut custom = spec(2);
    // A token ref that the THEGN_* allowlist alone would pass.
    custom.credential_ref = "env:THEGN_VERCEL_REF".into();
    let api: Arc<dyn Api> = fake.clone();
    let p = VercelProvider::with_client_for_test(custom, api, ledger.path().to_path_buf())
        .expect("test provider admits its spec");
    let env = credential_env();
    let want = ["TERM", "THEGN_WORKTREE"];

    // The PTY start frame.
    let spec = ExecSpec {
        argv: vec!["sh".into()],
        tty: true,
        cols: 80,
        rows: 24,
        env: env.clone(),
        cwd: None,
    };
    let frame: serde_json::Value =
        serde_json::from_str(&p.start_frame(&spec).expect("start frame")).expect("json frame");
    assert_eq!(env_keys(&frame), want, "pty start frame");

    // The stream-mode (detached) command body.
    let detached = p
        .command_body(&spec.argv, None, &spec.env, false)
        .expect("detached body");
    assert_eq!(env_keys(&detached), want, "detached command");

    // The one-shot command (capture reads and provisioning steps).
    p.run_exec(NAME, &["true".to_string()], None, &env, Wake::Allow)
        .await
        .expect("run");
    let sent = fake
        .bodies
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("run body");
    assert_eq!(env_keys(&sent), want, "one-shot command");
    for body in [&frame, &detached, &sent] {
        let text = body.to_string();
        for leaked in [
            "ref-value",
            "\"v\"",
            "GITHUB_TOKEN",
            "API_KEY",
            "SECRET",
            "AUTH",
        ] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
    }
}

#[tokio::test]
async fn created_tags_prove_ownership_to_destroy_and_the_reaper_check() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let controller = crate::vps::controller::controller_id().unwrap();
    let owner = "wt-owner-digest";
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    let mut templated = spec(2);
    templated.name = TEMPLATED.into();
    let api: Arc<dyn Api> = fake.clone();
    let p = VercelProvider::with_client_for_test(templated, api, ledger.path().to_path_buf())
        .expect("test provider admits its spec")
        .with_owner(owner.into());
    *fake.create.lock().unwrap() = Some(Ok(view(TEMPLATED, SessionStatus::Running, Some("s1"))));
    p.create().await.expect("create");

    // What Vercel stores is what the create body sent; read it back the way
    // the client parses a sandbox.
    let body = fake.bodies.lock().unwrap()[0].clone();
    let echoed = api::parse_sandbox(&serde_json::json!({
        "name": TEMPLATED,
        "status": "running",
        "currentSessionId": "s1",
        "createdAt": 1_700_000_000_000_i64,
        "tags": body["tags"].clone(),
    }))
    .expect("sandbox parses");

    // The reaper's check: the sanitized controller tag it derives, plus the
    // ledger owner the create recorded.
    let record = registry::read_at(ledger.path(), TEMPLATED).expect("ready record");
    let reaper_tag = policy::tags_for(&controller, "", "")
        .remove(policy::CONTROLLER_TAG)
        .unwrap_or_default();
    assert!(policy::tags_prove_ours(
        &echoed.tags,
        &reaper_tag,
        &record.owner
    ));
    assert!(!policy::tags_prove_ours(
        &echoed.tags,
        "someone-else",
        &record.owner
    ));

    // Inventory lists it (no name prefix), and destroy deletes it.
    fake.pages
        .lock()
        .unwrap()
        .push_back((vec![echoed.clone()], None));
    let InventoryReport::Complete(rows) = p.sandbox_inventory().await.unwrap() else {
        panic!("one page and no cursor is complete");
    };
    assert_eq!(
        rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        [TEMPLATED]
    );
    fake.gets.lock().unwrap().push_back(Some(echoed));
    p.destroy(TEMPLATED).await.expect("destroy recognises ours");
    assert_eq!(fake.count("delete"), 1, "{:?}", fake.calls());
    assert!(registry::read_at(ledger.path(), TEMPLATED).is_none());
}

#[tokio::test]
async fn destroy_refuses_a_sandbox_tagged_for_another_controller() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(tagged(NAME, "another-controller")));
    let error = provider(&fake, ledger.path(), 2)
        .destroy(NAME)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("refusing to delete"),
        "{error:#}"
    );
    assert_eq!(fake.count("delete"), 0);
}

#[test]
fn env_allowlist_passes_only_tmpfs_login_paths() {
    let env: Vec<(String, String)> = [
        (
            "CLAUDE_CONFIG_DIR",
            "/dev/shm/thegn-login-0123456789abcdef/claude",
        ),
        ("CODEX_HOME", "/dev/shm/thegn-login-0123456789abcdef/codex"),
        (
            "ANTHROPIC_API_KEY",
            "/dev/shm/thegn-login-0123456789abcdef/k",
        ),
        ("VERCEL_TOKEN", "/dev/shm/thegn-login-0123456789abcdef/t"),
        ("TERM", "xterm-256color"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let kept: Vec<String> = env_allowlisted(&env, "VERCEL_TOKEN")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(kept, ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "TERM"]);

    for value in [
        "/root/.claude",
        "/dev/shm/other/claude",
        "/dev/shm/thegn-login-0123456789abcdef/../../root/.claude",
    ] {
        let env = vec![("CLAUDE_CONFIG_DIR".to_string(), value.to_string())];
        assert!(env_allowlisted(&env, "VERCEL_TOKEN").is_empty(), "{value}");
    }
    // The token ref name is dropped even when shaped like a login key.
    let env = vec![(
        "CODEX_HOME".to_string(),
        "/dev/shm/thegn-login-0123456789abcdef/codex".to_string(),
    )];
    assert!(env_allowlisted(&env, "env:CODEX_HOME").is_empty());
}

#[tokio::test]
async fn usage_report_never_resumes_a_stopped_sandbox() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    registry::write_at(ledger.path(), &ledger_record(NAME, "ready")).unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.gets
        .lock()
        .unwrap()
        .push_back(Some(view(NAME, SessionStatus::Stopped, None)));
    let (rows, complete) = provider(&fake, ledger.path(), 2)
        .usage_report()
        .await
        .expect("usage report");
    assert!(complete, "{:?}", fake.calls());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].name, NAME);
    assert_eq!(rows[0].status, "stopped");
    assert_eq!(fake.count("resume"), 0, "{:?}", fake.calls());
    assert_eq!(fake.count("get"), 1, "{:?}", fake.calls());
    assert_eq!(fake.count("usage"), 1, "{:?}", fake.calls());
    assert_eq!(fake.count("snapshot_usage"), 1, "{:?}", fake.calls());
    for op in ["create", "delete", "stop", "extend", "run", "policy"] {
        assert_eq!(fake.count(op), 0, "{op}: {:?}", fake.calls());
    }
}

#[tokio::test]
async fn usage_report_lists_tagged_sandboxes_without_a_get() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let controller = crate::vps::controller::controller_id().unwrap();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi::default());
    fake.pages
        .lock()
        .unwrap()
        .push_back((vec![tagged("tg-vercel-listed", &controller)], None));
    let (rows, complete) = provider(&fake, ledger.path(), 2)
        .usage_report()
        .await
        .expect("usage report");
    assert!(complete);
    assert_eq!(
        rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["tg-vercel-listed"]
    );
    assert_eq!(rows[0].status, "running");
    assert_eq!(fake.count("get"), 0, "{:?}", fake.calls());
    assert_eq!(fake.count("resume"), 0, "{:?}", fake.calls());
}

#[tokio::test]
async fn usage_report_is_incomplete_on_a_partial_listing() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = Arc::new(FakeApi {
        endless: true,
        ..FakeApi::default()
    });
    let (_rows, complete) = provider(&fake, ledger.path(), 2)
        .usage_report()
        .await
        .expect("usage report");
    assert!(!complete, "a page-cap listing is a gap");
    assert_eq!(fake.count("resume"), 0);
}
