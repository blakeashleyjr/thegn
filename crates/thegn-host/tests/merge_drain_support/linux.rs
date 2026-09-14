//! THE219 Part B: real CLI processes and Git folding, never a real agent.
#[path = "custody.rs"]
mod custody;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thegn_core::config::{Config, ConflictHandoff, IsolationFloor, OnFloorMiss, OnLanded};
use thegn_core::db::{Db, MergeQueueRow};
use thegn_core::store::WorktreeAuxStore;

const GATE: &str = "if [ -f red-gate-marker ]; then printf 'private-red-gate\\n'; exit 1; fi; printf 'private-green-gate\\n'";
const HOLD: &str = "sandbox could not be established for the queue task (infrastructure failure); the branch is not at fault";

fn lexical_tool(name: &str) -> PathBuf {
    let found = PathBuf::from(
        thegn_core::util::which_path(name)
            .unwrap_or_else(|| panic!("required private fixture tool missing: {name}")),
    );
    let path = if found.is_absolute() {
        found
    } else {
        std::env::current_dir().unwrap().join(found)
    };
    let target = path.canonicalize().unwrap();
    let metadata = target.metadata().unwrap();
    assert!(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    // Keep the lexical basename: Nix sh/sleep-style multicall entrypoints must
    // not be invoked using the canonical package binary's different argv[0].
    path
}

fn digest(path: &Path) -> String {
    let mut input = File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buf).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buf[..count]);
    }
    format!("{:x}", hash.finalize())
}

struct Fixture {
    root: Arc<tempfile::TempDir>,
    repo: PathBuf,
    held: PathBuf,
    clean: PathBuf,
    config: PathBuf,
    serial: usize,
    binary_hash: String,
}
impl Fixture {
    fn new() -> Self {
        let root = Arc::new(tempfile::tempdir().unwrap());
        for name in [
            "repo", "bin", "hooks", "state", "config", "cache", "runtime", "tmp", "receipts",
        ] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        for name in ["git", "sh"] {
            symlink(lexical_tool(name), root.path().join("bin").join(name)).unwrap();
        }
        let mut bins = fs::read_dir(root.path().join("bin"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>();
        bins.sort();
        assert_eq!(bins, ["git", "sh"].map(std::ffi::OsString::from));
        for name in [
            "nice",
            "ionice",
            "systemd-run",
            "podman",
            "docker",
            "ssh",
            "direnv",
            "nix",
            "claude",
            "codex",
        ] {
            assert!(!root.path().join("bin").join(name).exists());
        }
        fs::write(root.path().join("gitconfig"), "").unwrap();
        fs::write(root.path().join("empty-rc"), "").unwrap();
        let shell = root.path().join("bin/sh");
        let sentinel = root.path().join("agent-sentinel");
        fs::write(&sentinel, format!("#!{}\n[ \"$#\" -eq 1 ] || exit 91\nprintf 'unexpected-agent\\n' > \"$1\"\nexit 92\n", shell.display())).unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o700)).unwrap();
        let command = format!(
            "{} {}",
            thegn_core::util::sh_quote(sentinel.to_str().unwrap()),
            thegn_core::util::sh_quote(root.path().join("agent-ran").to_str().unwrap())
        );
        // JSON basic strings are valid TOML basic strings for these fixed ASCII
        // fixture paths/commands. No path is interpolated into shell source.
        let text = format!(
            r#"
[automations]
enabled = false
[daemon]
enabled = false
[sandbox]
enabled = false
backend = "none"
warm_direnv = "off"
inject_devshell = false
[sandbox.limits]
cpu = ""
memory = ""
cpu_total = "off"
memory_total = "off"
[merge_queue]
enabled = true
target_branch = "main"
auto_land = false
snapshot_dirty = false
organize_folders = false
on_landed = "off"
sign_commits = false
regenerate_paths = []
gate_reuse_worktree = false
gate_target_dir = ""
gate_on = true
gate_command = {gate}
gate_setup_command = ""
remote_mode = "route_to_host"
conflict_handoff = "agent"
agent_command = {command}
agent_max_attempts = 3
agent_timeout_secs = 1
agent_sandbox = true
agent_isolation_floor = "guest-kernel"
agent_on_floor_miss = "fail"
"#,
            gate = serde_json::to_string(GATE).unwrap(),
            command = serde_json::to_string(&command).unwrap()
        );
        let errors = thegn_core::config::validate_str(&text);
        assert!(
            errors.is_empty(),
            "private configuration errors: {errors:?}"
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(!cfg.automations.enabled && !cfg.daemon.enabled && !cfg.sandbox.enabled);
        assert!(cfg.automations.rules.is_empty() && cfg.env.is_empty());
        assert!(!cfg.sandbox.inject_devshell);
        assert_eq!(cfg.sandbox.warm_direnv.as_str(), "off");
        assert_eq!(cfg.sandbox.backend.as_str(), "none");
        assert_eq!(cfg.sandbox.limits.cpu.as_deref(), Some(""));
        assert_eq!(cfg.sandbox.limits.cpu_total.as_deref(), Some("off"));
        assert_eq!(cfg.sandbox.limits.memory.as_deref(), Some(""));
        assert_eq!(cfg.sandbox.limits.memory_total.as_deref(), Some("off"));
        let mq = &cfg.merge_queue;
        assert!(mq.enabled && mq.gate_on && mq.agent_sandbox);
        assert!(
            !mq.auto_land
                && !mq.snapshot_dirty
                && !mq.organize_folders
                && !mq.sign_commits
                && !mq.gate_reuse_worktree
        );
        assert_eq!(mq.on_landed, OnLanded::Off);
        assert_eq!(mq.conflict_handoff, ConflictHandoff::Agent);
        assert_eq!(mq.agent_isolation_floor, IsolationFloor::GuestKernel);
        assert_eq!(mq.agent_on_floor_miss, OnFloorMiss::Fail);
        assert_eq!(mq.agent_max_attempts, 3);
        assert_eq!(mq.gate_command, GATE);
        assert!(mq.regenerate_paths.is_empty() && mq.gate_target_dir.is_empty());
        assert_eq!(
            thegn_core::agent_task::resolve_agent(&cfg, &mq.agent, &mq.agent_command),
            Some(command)
        );
        let config = root.path().join("fixture.toml");
        fs::write(&config, text).unwrap();
        let fixture = Self {
            repo: root.path().join("repo"),
            held: root.path().join("held"),
            clean: root.path().join("clean"),
            config,
            serial: 0,
            binary_hash: digest(Path::new(env!("CARGO_BIN_EXE_thegn"))),
            root,
        };
        assert!(!fixture.db_path().exists());
        fixture.record("manifest", json!({
            "kind":"THE219-private-actual-merge-drain", "native_platform":"linux",
            "binary":env!("CARGO_BIN_EXE_thegn"), "binary_sha256":fixture.binary_hash,
            "config":fs::read_to_string(&fixture.config).unwrap(),
            "git":{"path":fixture.root.path().join("bin/git"),"sha256":digest(&fixture.root.path().join("bin/git"))},
            "sh":{"path":fixture.root.path().join("bin/sh"),"sha256":digest(&fixture.root.path().join("bin/sh"))},
            "agent_sentinel":fs::read_to_string(fixture.root.path().join("agent-sentinel")).unwrap(),
            "scope":"No THE608 acceptance; Part A separately proves recovered floor admission"
        }));
        fixture
    }

    fn command(&self, executable: &Path, cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(executable);
        cmd.env_clear()
            .current_dir(cwd)
            .args(args)
            .env("PATH", self.root.path().join("bin"))
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.path().join("runtime"))
            .env("THEGN_DIR", self.root.path().join("app"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("TMP", self.root.path().join("tmp"))
            .env("TEMP", self.root.path().join("tmp"))
            .env("THEGN_NO_MIGRATE", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("SHELL", self.root.path().join("bin/sh"))
            .env("ENV", self.root.path().join("empty-rc"))
            .env("BASH_ENV", self.root.path().join("empty-rc"));
        cmd
    }
    fn run(&mut self, cmd: Command) -> custody::Output {
        self.serial += 1;
        let argv = std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let environment = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|s| s.to_string_lossy().into_owned()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert!(!environment.contains_key("HOME") && !environment.contains_key("THEGN_PROFILE"));
        let out = custody::run(cmd, Arc::clone(&self.root), self.serial);
        let cleanup: Value = serde_json::from_slice(
            &fs::read(
                self.root
                    .path()
                    .join(format!("receipts/{}.cleanup.json", self.serial)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(cleanup["leader_reaped"], true);
        assert_eq!(cleanup["retained"], false);
        assert_eq!(cleanup["identity_lost"], false);
        fs::write(self.root.path().join(format!("receipts/{}.json", self.serial)), serde_json::to_vec_pretty(&json!({"sequence":self.serial,"argv":argv,"environment":environment,"thegn_binary_sha256":self.binary_hash,"exit_code":out.status.code(),"elapsed_ms":out.elapsed_ms,"stdout":out.stdout,"stderr":out.stderr,"cleanup":cleanup})).unwrap()).unwrap();
        assert!(
            out.status.success(),
            "private argv {argv:?}: {:?}\n{}\n{}",
            out.status,
            out.stdout,
            out.stderr
        );
        out
    }
    fn git(&mut self, cwd: &Path, args: &[&str]) -> String {
        let command = self.command(&self.root.path().join("bin/git"), cwd, args);
        self.run(command).stdout.trim_end_matches('\n').to_owned()
    }
    fn cli(&mut self, args: &[&str]) -> custody::Output {
        let mut argv = vec!["--config", self.config.to_str().unwrap()];
        argv.extend_from_slice(args);
        let command = self.command(Path::new(env!("CARGO_BIN_EXE_thegn")), &self.repo, &argv);
        self.run(command)
    }
    fn json(&mut self, args: &[&str]) -> Value {
        serde_json::from_str(&self.cli(args).stdout).expect("exactly one CLI JSON document")
    }
    fn db_path(&self) -> PathBuf {
        self.root.path().join("state/thegn/thegn.db")
    }
    fn rows(&self) -> Vec<MergeQueueRow> {
        Db::open_at(&self.db_path())
            .unwrap()
            .list_merge_queue()
            .unwrap()
    }
    fn audit(&self) -> Vec<(String, String)> {
        let db = Connection::open(self.db_path()).unwrap();
        let mut statement = db
            .prepare("SELECT worktree,new_status FROM fixture_status_audit ORDER BY seq")
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }
    fn snapshot(&mut self) -> Value {
        let repo = self.repo.clone();
        let refs = self.git(
            &repo,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        let registrations = self.git(&repo, &["worktree", "list", "--porcelain"]);
        let mut files = BTreeMap::new();
        for dir in [&self.repo, &self.held, &self.clean] {
            let mut entries = fs::read_dir(dir)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                if entry.file_name() == ".git" {
                    continue;
                }
                assert!(
                    entry.file_type().unwrap().is_file(),
                    "fixture has only committed flat files"
                );
                files.insert(
                    entry.path().to_str().unwrap().to_owned(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
        json!({"refs":refs,"registrations":registrations,"files":files})
    }
    fn record(&self, name: &str, value: Value) {
        fs::write(
            self.root
                .path()
                .join("receipts")
                .join(format!("{name}.json")),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }
    fn close(self) {
        let mut entries = fs::read_dir(self.root.path().join("receipts"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        let mut receipts = BTreeMap::new();
        for entry in entries {
            if entry.path().extension().is_some_and(|e| e == "json") {
                let bytes = fs::read(entry.path()).unwrap();
                receipts.insert(
                    entry.file_name().to_string_lossy().into_owned(),
                    serde_json::from_slice::<Value>(&bytes).unwrap(),
                );
            }
        }
        let serialized =
            serde_json::to_string(&json!({"receipts":receipts,"private_root_removed":true}))
                .unwrap();
        assert!(
            serialized.len() <= 8 * 1024 * 1024,
            "bounded fixture evidence export"
        );
        Arc::try_unwrap(self.root)
            .expect("held child retains private evidence")
            .close()
            .expect("strict private drain fixture cleanup");
        // Emit success only after strict removal; root retains this test log.
        writeln!(
            std::io::stdout().lock(),
            "THE219_DRAIN_RECEIPT {serialized}"
        )
        .unwrap();
    }
}

#[derive(Clone, Copy)]
enum Case {
    Conflict,
    RedGate,
}
fn assert_summary(doc: &Value, ready: &[&str], deferred: &[&str]) {
    assert_eq!(doc["target"], "main");
    assert_eq!(doc["ready"], json!(ready));
    assert_eq!(doc["deferred"], json!(deferred));
    for field in [
        "landed",
        "gate_error",
        "needs_human",
        "warnings",
        "stale_checkouts",
    ] {
        assert_eq!(doc[field], json!([]), "{field}: {doc}");
    }
    for (field, count) in [
        ("ready", ready.len()),
        ("deferred", deferred.len()),
        ("landed", 0),
        ("gate_error", 0),
        ("needs_human", 0),
    ] {
        assert_eq!(doc["counts"][field], count);
    }
}
fn row<'a>(rows: &'a [MergeQueueRow], branch: &str) -> &'a MergeQueueRow {
    rows.iter().find(|r| r.branch == branch).unwrap()
}

fn exercise(case: Case) {
    let mut f = Fixture::new();
    let repo = f.repo.clone();
    let held = f.held.clone();
    let clean = f.clean.clone();
    f.git(&repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "private-drain"),
        ("user.email", "private@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.fsmonitor", "false"),
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
    ] {
        f.git(&repo, &["config", key, value]);
    }
    let hooks = f.root.path().join("hooks");
    f.git(
        &repo,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    fs::write(repo.join("conflict.txt"), "base\n").unwrap();
    f.git(&repo, &["add", "conflict.txt"]);
    f.git(&repo, &["commit", "-qm", "base"]);
    f.git(
        &repo,
        &["worktree", "add", "-qb", "held", held.to_str().unwrap()],
    );
    match case {
        Case::Conflict => {
            fs::write(held.join("conflict.txt"), "held edit\n").unwrap();
            f.git(&held, &["add", "conflict.txt"]);
            f.git(&held, &["commit", "-qm", "held conflict"]);
            fs::write(repo.join("conflict.txt"), "main edit\n").unwrap();
            f.git(&repo, &["add", "conflict.txt"]);
            f.git(&repo, &["commit", "-qm", "main conflict"]);
        }
        Case::RedGate => {
            fs::write(held.join("red-gate-marker"), "red\n").unwrap();
            f.git(&held, &["add", "red-gate-marker"]);
            f.git(&held, &["commit", "-qm", "red gate"]);
        }
    }
    f.git(
        &repo,
        &["worktree", "add", "-qb", "clean", clean.to_str().unwrap()],
    );
    fs::write(clean.join("clean.txt"), "clean payload\n").unwrap();
    f.git(&clean, &["add", "clean.txt"]);
    f.git(&clean, &["commit", "-qm", "clean change"]);
    assert!(
        !f.db_path().exists(),
        "only real CLI bootstrap initializes private database"
    );
    f.cli(&[
        "merge",
        "add",
        held.to_str().unwrap(),
        clean.to_str().unwrap(),
    ]);
    let listed = f.json(&["merge", "list", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 2);
    let initial = f.rows();
    assert_eq!(initial.len(), 2);
    assert!(
        initial
            .iter()
            .all(|r| r.agent_attempts == 0 && r.status == "queued")
    );
    let sql = Connection::open(f.db_path()).unwrap();
    assert_eq!(sql.execute("UPDATE merge_queue SET agent_attempts=1,result_oid='stale',conflict_paths='stale-path',error_detail='stale-diagnostic' WHERE worktree=?1",[held.to_str().unwrap()]).unwrap(),1);
    // Freeze explicit oldest-first ordering. Equal second-resolution enqueue
    // timestamps must not let clean execute before the held branch and mask a
    // regression that aborts the entire drain on its infrastructure hold.
    let first_time = row(&initial, "held").queued_at;
    assert_eq!(
        sql.execute(
            "UPDATE merge_queue SET queued_at=?2 WHERE worktree=?1",
            rusqlite::params![clean.to_str().unwrap(), first_time.checked_add(1).unwrap()]
        )
        .unwrap(),
        1
    );
    sql.execute_batch("CREATE TABLE fixture_status_audit(seq INTEGER PRIMARY KEY,worktree TEXT NOT NULL,old_status TEXT NOT NULL,new_status TEXT NOT NULL,result_oid TEXT,conflict_paths TEXT,error_detail TEXT,attempts INTEGER); CREATE TRIGGER fixture_every_status AFTER UPDATE OF status ON merge_queue BEGIN INSERT INTO fixture_status_audit(worktree,old_status,new_status,result_oid,conflict_paths,error_detail,attempts) VALUES(NEW.worktree,OLD.status,NEW.status,NEW.result_oid,NEW.conflict_paths,NEW.error_detail,NEW.agent_attempts); END;").unwrap();
    drop(sql);
    let before = f.snapshot();
    let rows_before = f.rows();
    assert_eq!(
        rows_before
            .iter()
            .map(|r| r.branch.as_str())
            .collect::<Vec<_>>(),
        ["held", "clean"]
    );
    f.record(
        "setup",
        json!({"initial_rows":initial,"seeded_rows":rows_before}),
    );
    f.record(
        "phase1-before",
        json!({"snapshot":before,"rows":rows_before}),
    );
    let first = f.json(&["merge", "drain", "--json"]);
    assert_summary(&first, &["clean"], &["held"]);
    let after = f.snapshot();
    assert_eq!(after, before);
    let rows = f.rows();
    assert_eq!(rows.len(), 2);
    let blocked = row(&rows, "held");
    let old = row(&rows_before, "held");
    assert_eq!(blocked.status, "agent_blocked");
    assert_eq!(blocked.agent_attempts, 1);
    assert!(blocked.result_oid.is_none() && blocked.conflict_paths.is_none());
    assert_eq!(blocked.error_detail.as_deref(), Some(HOLD));
    assert_eq!(blocked.queued_at, old.queued_at);
    assert_eq!(blocked.worktree, old.worktree);
    assert_eq!(blocked.branch, old.branch);
    assert_eq!(blocked.target_branch, old.target_branch);
    assert_eq!(blocked.location, old.location);
    assert_eq!(row(&rows, "clean").status, "ready");
    let audit = f.audit();
    for (path, statuses) in [
        (held.to_str().unwrap(), vec!["folding", "agent_blocked"]),
        (clean.to_str().unwrap(), vec!["folding", "ready"]),
    ] {
        assert_eq!(
            audit
                .iter()
                .filter(|(p, _)| p == path)
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>(),
            statuses
        );
    }
    assert_eq!(audit.len(), 4);
    assert_eq!(
        audit,
        vec![
            (held.to_str().unwrap().into(), "folding".into()),
            (held.to_str().unwrap().into(), "agent_blocked".into()),
            (clean.to_str().unwrap().into(), "folding".into()),
            (clean.to_str().unwrap().into(), "ready".into())
        ]
    );
    assert!(!f.root.path().join("agent-ran").exists());
    f.record(
        "phase1-after",
        json!({"summary":first,"snapshot":after,"rows":rows,"audit":audit}),
    );

    // Intentional repair is a separate phase, never attributed to drain. The
    // later CLI proves re-enumeration/recovery; Part A proves floor admission.
    let old_tip = f.git(&repo, &["rev-parse", "held"]);
    match case {
        Case::Conflict => {
            fs::write(
                held.join("conflict.txt"),
                fs::read(repo.join("conflict.txt")).unwrap(),
            )
            .unwrap();
            fs::write(held.join("repaired.txt"), "desired repair\n").unwrap();
            f.git(&held, &["add", "conflict.txt", "repaired.txt"]);
        }
        Case::RedGate => {
            fs::remove_file(held.join("red-gate-marker")).unwrap();
            fs::write(held.join("repaired.txt"), "desired repair\n").unwrap();
            f.git(&held, &["add", "-A"]);
        }
    }
    f.git(&held, &["commit", "-qm", "private intentional repair"]);
    let new_tip = f.git(&repo, &["rev-parse", "held"]);
    assert_ne!(new_tip, old_tip);
    let delta = f.git(&repo, &["diff", "--name-only", &old_tip, &new_tip]);
    let expected = match case {
        Case::Conflict => "conflict.txt\nrepaired.txt",
        Case::RedGate => "red-gate-marker\nrepaired.txt",
    };
    assert_eq!(delta, expected);
    assert_eq!(
        f.rows(),
        rows,
        "repair must not reset queue or attempt count"
    );
    let sql = Connection::open(f.db_path()).unwrap();
    sql.execute("DELETE FROM fixture_status_audit", []).unwrap();
    drop(sql);
    let second_before = f.snapshot();
    let expected_refs = after["refs"].as_str().unwrap().replace(
        &format!("refs/heads/held {old_tip}"),
        &format!("refs/heads/held {new_tip}"),
    );
    assert_eq!(
        second_before["refs"], expected_refs,
        "repair may advance only held branch"
    );
    // Worktree porcelain includes branch HEAD: this is the one intentional
    // registration-byte change, not permission to add/remove any registration.
    assert_eq!(
        second_before["registrations"],
        after["registrations"]
            .as_str()
            .unwrap()
            .replace(&format!("HEAD {old_tip}"), &format!("HEAD {new_tip}"))
    );
    let mut expected_files = after["files"].as_object().unwrap().clone();
    match case {
        Case::Conflict => {
            expected_files.insert(
                held.join("conflict.txt").to_str().unwrap().into(),
                json!(fs::read(repo.join("conflict.txt")).unwrap()),
            );
        }
        Case::RedGate => {
            expected_files.remove(held.join("red-gate-marker").to_str().unwrap());
        }
    }
    expected_files.insert(
        held.join("repaired.txt").to_str().unwrap().into(),
        json!(b"desired repair\n".to_vec()),
    );
    assert_eq!(second_before["files"], Value::Object(expected_files));
    f.record(
        "repair",
        json!({"old_tip":old_tip,"new_tip":new_tip,"changed_paths":delta,"snapshot":second_before}),
    );
    let second = f.json(&["merge", "drain", "--json"]);
    assert_summary(&second, &["held"], &[]);
    let second_after = f.snapshot();
    assert_eq!(second_after, second_before);
    let final_rows = f.rows();
    let ready = row(&final_rows, "held");
    assert_eq!(ready.status, "ready");
    assert_eq!(ready.agent_attempts, 1);
    assert!(ready.conflict_paths.is_none());
    assert_eq!(
        ready.error_detail.as_deref(),
        Some("gated green — awaiting land")
    );
    assert_eq!(ready.queued_at, blocked.queued_at);
    assert_eq!(ready.worktree, blocked.worktree);
    assert_eq!(ready.branch, blocked.branch);
    assert_eq!(ready.target_branch, blocked.target_branch);
    assert_eq!(ready.location, blocked.location);
    assert_eq!(row(&final_rows, "clean"), row(&rows, "clean"));
    let oid = ready.result_oid.as_ref().unwrap();
    assert_eq!(f.git(&repo, &["cat-file", "-t", oid]), "commit");
    assert_eq!(
        f.audit(),
        vec![
            (held.to_str().unwrap().into(), "folding".into()),
            (held.to_str().unwrap().into(), "ready".into())
        ]
    );
    assert!(!f.root.path().join("agent-ran").exists());
    let final_list = f.json(&["merge", "list", "--json"]);
    assert_eq!(final_list.as_array().unwrap().len(), 2);
    f.record("phase2-after",json!({"summary":second,"snapshot":second_after,"rows":final_rows,"audit":f.audit(),"list":final_list}));
    f.close();
}

#[test]
fn real_cli_conflict_hold_continues_then_plain_drain_recovers_same_row() {
    exercise(Case::Conflict);
}

#[test]
fn real_cli_red_gate_hold_continues_then_plain_drain_recovers_same_row() {
    exercise(Case::RedGate);
}
