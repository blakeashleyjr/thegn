//! THE219 Part B: real CLI processes and Git folding with hermetic fake agents.
#[path = "custody.rs"]
mod custody;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
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
// The credential-free queue-agent hold (THE-419): no sandbox, and autonomous agents never fall back to the host.
const HOLD: &str = "no sandbox could be established for the queued agent in ";

type StartedCommand = (Vec<String>, BTreeMap<String, Option<String>>);

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
    assert!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "lexical tool {name} must be an executable regular file"
    );
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
    started: BTreeMap<usize, StartedCommand>,
    agent_guard: Option<GateReleaseGuard>,
}
struct FixtureOpts {
    gate: String,
    auto_land: bool,
    agent: AgentMode,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentMode {
    HeldAtFloor,
    HostFifo,
}
impl Fixture {
    fn new() -> Self {
        Self::with(FixtureOpts {
            gate: GATE.to_owned(),
            auto_land: false,
            agent: AgentMode::HeldAtFloor,
        })
    }

    fn with(options: FixtureOpts) -> Self {
        let root = Arc::new(tempfile::tempdir().unwrap());
        let gate_command = if options.gate.is_empty() {
            race_gate(root.path())
        } else {
            options.gate
        };
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
        assert_eq!(
            bins,
            ["git", "sh"].map(std::ffi::OsString::from),
            "fixture bin dir must hold exactly the git and sh symlinks, got {bins:?}"
        );
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
            assert!(
                !root.path().join("bin").join(name).exists(),
                "fixture bin dir must not provide {name} on PATH"
            );
        }
        fs::write(root.path().join("gitconfig"), "").unwrap();
        fs::write(root.path().join("empty-rc"), "").unwrap();
        let shell = root.path().join("bin/sh");
        let fixture_shell = root.path().join("fixture-shell");
        fs::write(
            &fixture_shell,
            format!(
                "#!{}\n[ \"$#\" -eq 2 ] && {{ [ \"$1\" = -c ] || [ \"$1\" = -lc ]; }} || exit 97\nexec {} -c \"$2\"\n",
                shell.display(),
                shell.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fixture_shell, fs::Permissions::from_mode(0o700)).unwrap();
        let sentinel = root.path().join("agent-sentinel");
        fs::write(&sentinel, format!("#!{}\n[ \"$#\" -eq 1 ] || exit 91\nprintf 'unexpected-agent\\n' > \"$1\"\nexit 92\n", shell.display())).unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o700)).unwrap();
        let (command, agent_sandbox, agent_timeout_secs, isolation_floor) = match options.agent {
            AgentMode::HeldAtFloor => (
                format!(
                    "{} {}",
                    thegn_core::util::sh_quote(sentinel.to_str().unwrap()),
                    thegn_core::util::sh_quote(root.path().join("agent-ran").to_str().unwrap())
                ),
                true,
                1,
                String::new(),
            ),
            AgentMode::HostFifo => {
                let agent = root.path().join("fake-agent");
                let starts =
                    thegn_core::util::sh_quote(root.path().join("agent-runs").to_str().unwrap());
                let hold =
                    thegn_core::util::sh_quote(root.path().join("agent-hold").to_str().unwrap());
                let open =
                    thegn_core::util::sh_quote(root.path().join("agent-open").to_str().unwrap());
                let script = format!(
                    "#!{}\nprintf 'agent-start %s\\n' \"$$\" >> {starts}; if [ -p {hold} ] && [ ! -e {open} ]; then IFS= read -r _ < {hold}; fi; printf 'agent-end %s\\n' \"$$\" >> {starts}; exit 0\n",
                    shell.display()
                );
                fs::write(&agent, script).unwrap();
                fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
                (
                    thegn_core::util::sh_quote(agent.to_str().unwrap()),
                    false,
                    120,
                    "isolation_floor = \"off\"\n".to_owned(),
                )
            }
        };
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
{isolation_floor}
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
auto_land = {auto_land}
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
agent_timeout_secs = {agent_timeout_secs}
agent_sandbox = {agent_sandbox}
agent_isolation_floor = "guest-kernel"
agent_on_floor_miss = "fail"
"#,
            gate = serde_json::to_string(&gate_command).unwrap(),
            auto_land = options.auto_land,
            command = serde_json::to_string(&command).unwrap(),
            agent_sandbox = agent_sandbox,
            agent_timeout_secs = agent_timeout_secs,
            isolation_floor = isolation_floor
        );
        let errors = thegn_core::config::validate_str(&text);
        assert!(
            errors.is_empty(),
            "private configuration errors: {errors:?}"
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(
            !cfg.automations.enabled && !cfg.daemon.enabled && !cfg.sandbox.enabled,
            "fixture config must disable automations, daemon and sandbox"
        );
        assert!(
            cfg.automations.rules.is_empty() && cfg.env.is_empty(),
            "fixture config must define no automation rules and no env overrides"
        );
        assert!(
            !cfg.sandbox.inject_devshell,
            "fixture config must disable devshell injection"
        );
        assert_eq!(
            cfg.sandbox.warm_direnv.as_str(),
            "off",
            "fixture sandbox.warm_direnv must be off"
        );
        assert_eq!(
            cfg.sandbox.backend.as_str(),
            "none",
            "fixture sandbox.backend must be none"
        );
        assert_eq!(
            cfg.sandbox.limits.cpu.as_deref(),
            Some(""),
            "fixture sandbox cpu limit must be the empty string"
        );
        assert_eq!(
            cfg.sandbox.limits.cpu_total.as_deref(),
            Some("off"),
            "fixture sandbox cpu_total limit must be off"
        );
        assert_eq!(
            cfg.sandbox.limits.memory.as_deref(),
            Some(""),
            "fixture sandbox memory limit must be the empty string"
        );
        assert_eq!(
            cfg.sandbox.limits.memory_total.as_deref(),
            Some("off"),
            "fixture sandbox memory_total limit must be off"
        );
        let mq = &cfg.merge_queue;
        assert!(
            mq.enabled && mq.gate_on,
            "fixture merge queue must be enabled with gate_on set"
        );
        match options.agent {
            AgentMode::HeldAtFloor => assert!(
                mq.agent_sandbox,
                "held-at-floor fixture must set merge_queue.agent_sandbox"
            ),
            AgentMode::HostFifo => assert!(
                !mq.agent_sandbox,
                "host-fifo fixture must clear merge_queue.agent_sandbox"
            ),
        }
        if !options.auto_land && gate_command == GATE {
            assert!(
                !mq.auto_land
                    && !mq.snapshot_dirty
                    && !mq.organize_folders
                    && !mq.sign_commits
                    && !mq.gate_reuse_worktree,
                "default gate fixture must disable auto_land, snapshot_dirty, organize_folders, sign_commits and gate_reuse_worktree"
            );
            assert_eq!(
                mq.gate_command, GATE,
                "default gate fixture must use the GATE command"
            );
        } else {
            assert!(
                !mq.snapshot_dirty
                    && !mq.organize_folders
                    && !mq.sign_commits
                    && !mq.gate_reuse_worktree,
                "custom gate fixture must disable snapshot_dirty, organize_folders, sign_commits and gate_reuse_worktree"
            );
            assert_eq!(
                mq.auto_land, options.auto_land,
                "merge_queue.auto_land must match the fixture option"
            );
            assert_eq!(
                mq.gate_command, gate_command,
                "merge_queue.gate_command must match the fixture gate"
            );
        }
        assert_eq!(
            mq.on_landed,
            OnLanded::Off,
            "merge_queue.on_landed must be off"
        );
        assert_eq!(
            mq.conflict_handoff,
            ConflictHandoff::Agent,
            "merge_queue.conflict_handoff must be agent"
        );
        assert_eq!(
            mq.agent_isolation_floor,
            IsolationFloor::GuestKernel,
            "merge_queue.agent_isolation_floor must be guest-kernel"
        );
        if options.agent == AgentMode::HostFifo {
            assert_eq!(
                mq.agent_timeout_secs, 120,
                "host-fifo fixture must set agent_timeout_secs to 120"
            );
            assert_eq!(
                cfg.sandbox.isolation_floor,
                Some(IsolationFloor::Off),
                "host-fifo fixture must set sandbox isolation_floor to off"
            );
        } else {
            assert_eq!(
                mq.agent_timeout_secs, 1,
                "held-at-floor fixture must set agent_timeout_secs to 1"
            );
            assert_eq!(
                mq.agent_isolation_floor,
                IsolationFloor::GuestKernel,
                "held-at-floor fixture must keep agent_isolation_floor at guest-kernel"
            );
        }
        assert_eq!(
            mq.agent_on_floor_miss,
            OnFloorMiss::Fail,
            "merge_queue.agent_on_floor_miss must be fail"
        );
        assert_eq!(
            mq.agent_max_attempts, 3,
            "merge_queue.agent_max_attempts must be 3"
        );
        assert!(
            mq.regenerate_paths.is_empty() && mq.gate_target_dir.is_empty(),
            "fixture must not regenerate paths or set a gate target dir"
        );
        assert_eq!(
            thegn_core::agent_task::resolve_agent(&cfg, &mq.agent, &mq.agent_command),
            Some(command),
            "resolved agent command must match the fixture command"
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
            started: BTreeMap::new(),
            agent_guard: None,
            root,
        };
        assert!(
            !fixture.db_path().exists(),
            "fixture must start without a private database"
        );
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
            .env("TMPDIR", self.root.path())
            .env("TMP", self.root.path())
            .env("TEMP", self.root.path())
            .env("THEGN_NO_MIGRATE", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.path().join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("SHELL", self.root.path().join("fixture-shell"))
            .env("ENV", self.root.path().join("empty-rc"))
            .env("BASH_ENV", self.root.path().join("empty-rc"));
        cmd
    }
    fn run(&mut self, cmd: Command) -> custody::Output {
        let argv = std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let out = self.run_any(cmd);
        assert!(
            out.status.success(),
            "private argv {argv:?}: {:?}\n{}\n{}",
            out.status,
            out.stdout,
            out.stderr
        );
        out
    }

    fn run_any(&mut self, cmd: Command) -> custody::Output {
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
        assert!(
            !environment.contains_key("HOME") && !environment.contains_key("THEGN_PROFILE"),
            "CLI environment must not inherit HOME or THEGN_PROFILE"
        );
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
        assert_eq!(
            cleanup["leader_reaped"], true,
            "cleanup receipt must show the leader reaped"
        );
        assert_eq!(
            cleanup["retained"], false,
            "cleanup receipt must show the process was not retained"
        );
        assert_eq!(
            cleanup["identity_lost"], false,
            "cleanup receipt must not report a lost process identity"
        );
        fs::write(self.root.path().join(format!("receipts/{}.json", self.serial)), serde_json::to_vec_pretty(&json!({"sequence":self.serial,"argv":argv,"environment":environment,"thegn_binary_sha256":self.binary_hash,"exit_code":out.status.code(),"elapsed_ms":out.elapsed_ms,"stdout":out.stdout,"stderr":out.stderr,"cleanup":cleanup})).unwrap()).unwrap();
        out
    }

    fn start_cli(&mut self, args: &[&str]) -> custody::Running {
        self.serial += 1;
        let mut argv = vec!["--config", self.config.to_str().unwrap()];
        argv.extend_from_slice(args);
        let command = self.command(Path::new(env!("CARGO_BIN_EXE_thegn")), &self.repo, &argv);
        let rendered = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let environment = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|entry| entry.to_string_lossy().into_owned()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert!(
            !environment.contains_key("HOME") && !environment.contains_key("THEGN_PROFILE"),
            "CLI environment must not inherit HOME or THEGN_PROFILE"
        );
        self.started.insert(self.serial, (rendered, environment));
        custody::start(command, Arc::clone(&self.root), self.serial)
    }

    fn finish_cli(&mut self, running: custody::Running) -> custody::Output {
        let sequence = running.sequence();
        let out = running.finish();
        self.record_cli_output(sequence, &out);
        out
    }

    fn kill_cli(&mut self, running: custody::Running) -> custody::Output {
        let sequence = running.sequence();
        let out = running.kill();
        self.record_cli_output(sequence, &out);
        out
    }

    fn record_cli_output(&mut self, sequence: usize, out: &custody::Output) {
        let (argv, environment) = self.started.remove(&sequence).unwrap();
        let cleanup: Value = serde_json::from_slice(
            &fs::read(
                self.root
                    .path()
                    .join(format!("receipts/{sequence}.cleanup.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            cleanup["leader_reaped"].as_bool().unwrap(),
            "cleanup receipt {sequence} must show the leader reaped"
        );
        assert!(
            !cleanup["retained"].as_bool().unwrap(),
            "cleanup receipt {sequence} must show the process was not retained"
        );
        assert!(
            !cleanup["identity_lost"].as_bool().unwrap(),
            "cleanup receipt {sequence} must not report a lost process identity"
        );
        fs::write(
            self.root.path().join(format!("receipts/{sequence}.json")),
            serde_json::to_vec_pretty(&json!({"sequence":sequence,"argv":argv,"environment":environment,"thegn_binary_sha256":self.binary_hash,"exit_code":out.status.code(),"elapsed_ms":out.elapsed_ms,"stdout":out.stdout,"stderr":out.stderr,"cleanup":cleanup})).unwrap(),
        )
        .unwrap();
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
    fn cli_failing(&mut self, args: &[&str]) -> custody::Output {
        let mut argv = vec!["--config", self.config.to_str().unwrap()];
        argv.extend_from_slice(args);
        let command = self.command(Path::new(env!("CARGO_BIN_EXE_thegn")), &self.repo, &argv);
        let out = self.run_any(command);
        assert!(
            !out.status.success(),
            "expected a refusal: {args:?}\n{}\n{}",
            out.stdout,
            out.stderr
        );
        out
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

fn race_gate(root: &Path) -> String {
    let starts = thegn_core::util::sh_quote(root.join("gate-runs").to_str().unwrap());
    let hold = thegn_core::util::sh_quote(root.join("gate-hold").to_str().unwrap());
    let open = thegn_core::util::sh_quote(root.join("gate-open").to_str().unwrap());
    format!(
        "printf 'start %s\\n' \"$$\" >> {starts}; if [ -p {hold} ] && [ ! -e {open} ]; then IFS= read -r _ < {hold}; fi; printf 'end %s\\n' \"$$\" >> {starts}; printf 'private-green-gate\\n'"
    )
}

fn make_gate_fifo(root: &Path, name: &str) {
    let path = std::ffi::CString::new(root.join(name).to_str().unwrap()).unwrap();
    // SAFETY: path is a valid NUL-terminated private fixture path; mode limits
    // the FIFO to the test's owner.
    assert_eq!(
        unsafe { libc::mkfifo(path.as_ptr(), 0o600) },
        0,
        "mkfifo must create gate FIFO {name}"
    );
}

struct GateReleaseGuard {
    root: PathBuf,
    fifo_name: String,
    open_name: String,
    armed: bool,
}

impl GateReleaseGuard {
    fn new(root: &Path, fifo_name: &str, open_name: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            fifo_name: fifo_name.to_owned(),
            open_name: open_name.to_owned(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GateReleaseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let open = self.root.join(&self.open_name);
        if let Err(error) = fs::write(&open, "open\n") {
            tracing::debug!(path = %open.display(), %error, "could not mark race gate open during cleanup");
        }

        let fifo_path = self.root.join(&self.fifo_name);
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo_path)
        {
            Ok(mut fifo) => {
                if let Err(error) = fifo.write_all(b"continue\n") {
                    tracing::debug!(path = %fifo_path.display(), %error, "could not release race gate during cleanup");
                }
            }
            Err(error) => {
                tracing::debug!(path = %fifo_path.display(), %error, "could not open race gate FIFO during cleanup");
            }
        }
    }
}

fn release_gate(root: &Path, fifo_name: &str, open_name: &str) {
    let path = root.join(fifo_name);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut fifo = loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(fifo) => break fifo,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for the blocked gate to open its FIFO reader"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("could not open gate FIFO for release: {error}"),
        }
    };
    fs::write(root.join(open_name), "open\n").unwrap();
    fifo.write_all(b"continue\n").unwrap();
}

fn gate_log(root: &Path, log_name: &str) -> String {
    match fs::read_to_string(root.join(log_name)) {
        Ok(log) => log,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("could not read gate log: {error}"),
    }
}

fn gate_count(root: &Path, log_name: &str, event: &str) -> usize {
    gate_log(root, log_name)
        .lines()
        .filter(|line| line.starts_with(event))
        .count()
}

fn wait_gate_count(root: &Path, log_name: &str, event: &str, expected: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let observed = gate_count(root, log_name, event);
        if observed == expected {
            return;
        }
        assert!(
            observed < expected && std::time::Instant::now() < deadline,
            "timed out waiting for {expected} `{event}` gate records (observed {observed}); log: {}",
            gate_log(root, log_name)
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn one_row_fixture(options: FixtureOpts) -> Fixture {
    one_row_fixture_with_red_gate(options, false)
}

fn one_row_fixture_with_red_gate(options: FixtureOpts, red_gate: bool) -> Fixture {
    let mut fixture = Fixture::with(options);
    let repo = fixture.repo.clone();
    let feature = fixture.held.clone();
    fixture.git(&repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "private-drain"),
        ("user.email", "private@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.fsmonitor", "false"),
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
    ] {
        fixture.git(&repo, &["config", key, value]);
    }
    fixture.git(
        &repo,
        &[
            "config",
            "core.hooksPath",
            fixture.root.path().join("hooks").to_str().unwrap(),
        ],
    );
    fs::write(repo.join("base.txt"), "base\n").unwrap();
    fixture.git(&repo, &["add", "base.txt"]);
    fixture.git(&repo, &["commit", "-qm", "base"]);
    fixture.git(
        &repo,
        &[
            "worktree",
            "add",
            "-qb",
            "feature",
            feature.to_str().unwrap(),
        ],
    );
    fs::write(feature.join("feature.txt"), "feature\n").unwrap();
    if red_gate {
        fs::write(feature.join("red-gate-marker"), "always red\n").unwrap();
        fixture.git(&feature, &["add", "feature.txt", "red-gate-marker"]);
    } else {
        fixture.git(&feature, &["add", "feature.txt"]);
    }
    if red_gate {
        fixture.git(
            &feature,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "feature with red gate marker",
            ],
        );
    } else {
        fixture.git(&feature, &["commit", "-qm", "feature"]);
    }
    fixture.cli(&["merge", "add", feature.to_str().unwrap()]);
    fixture
}

fn agent_row_fixture() -> Fixture {
    let mut fixture = one_row_fixture_with_red_gate(
        FixtureOpts {
            gate: GATE.to_owned(),
            auto_land: false,
            agent: AgentMode::HostFifo,
        },
        true,
    );
    make_gate_fifo(fixture.root.path(), "agent-hold");
    fixture.agent_guard = Some(GateReleaseGuard::new(
        fixture.root.path(),
        "agent-hold",
        "agent-open",
    ));
    fixture
}

#[derive(Debug, PartialEq)]
struct RowState {
    status: String,
    agent_attempts: i64,
    generation: i64,
    phase: String,
    result_oid: Option<String>,
    error_detail: Option<String>,
}

fn row_state(fixture: &Fixture) -> RowState {
    let connection = Connection::open(fixture.db_path()).unwrap();
    connection
        .query_row(
            "SELECT status,agent_attempts,claim_generation,claim_phase,result_oid,error_detail FROM merge_queue",
            [],
            |row| {
                Ok(RowState {
                    status: row.get(0)?,
                    agent_attempts: row.get(1)?,
                    generation: row.get(2)?,
                    phase: row.get(3)?,
                    result_oid: row.get(4)?,
                    error_detail: row.get(5)?,
                })
            },
        )
        .unwrap()
}

fn event_rows(fixture: &Fixture, operation: &str) -> Vec<String> {
    let connection = Connection::open(fixture.db_path()).unwrap();
    let mut statement = connection
        .prepare("SELECT outcome FROM merge_queue_events WHERE operation=?1 ORDER BY id")
        .unwrap();
    statement
        .query_map([operation], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn expire_lease(fixture: &Fixture, worktree: &str) {
    let connection = Connection::open(fixture.db_path()).unwrap();
    connection
        .execute(
            "UPDATE merge_queue SET claim_lease_until_ms=?2 WHERE worktree=?1",
            rusqlite::params![worktree, thegn_core::util::now_ms() - 1],
        )
        .unwrap();
}

#[derive(Clone, Copy)]
enum Case {
    Conflict,
    RedGate,
}
fn assert_summary(doc: &Value, ready: &[&str], deferred: &[&str], agent_blocked: &[&str]) {
    assert_eq!(doc["target"], "main", "drain summary target must be main");
    assert_eq!(
        doc["ready"],
        json!(ready),
        "drain summary ready list must be {ready:?}"
    );
    assert_eq!(
        doc["deferred"],
        json!(deferred),
        "drain summary deferred list must be {deferred:?}"
    );
    assert_eq!(
        doc["agent_blocked"],
        json!(agent_blocked),
        "drain summary agent_blocked list must be {agent_blocked:?}"
    );
    for field in [
        "landed",
        "gate_error",
        "needs_human",
        "warnings",
        "persistence_errors",
        "bookkeeping_failed",
        "stale_checkouts",
    ] {
        assert_eq!(doc[field], json!([]), "{field}: {doc}");
    }
    for (field, count) in [
        ("ready", ready.len()),
        ("deferred", deferred.len()),
        ("agent_blocked", agent_blocked.len()),
        ("landed", 0),
        ("gate_error", 0),
        ("needs_human", 0),
        ("persistence_errors", 0),
        ("bookkeeping_failed", 0),
    ] {
        assert_eq!(
            doc["counts"][field], count,
            "drain summary count {field} must be {count}"
        );
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
    assert_eq!(
        listed.as_array().unwrap().len(),
        2,
        "merge list must report two rows"
    );
    let initial = f.rows();
    assert_eq!(initial.len(), 2, "fixture must seed two queue rows");
    assert!(
        initial
            .iter()
            .all(|r| r.agent_attempts == 0 && r.status == "queued"),
        "both seeded rows must start queued with zero attempts"
    );
    let sql = Connection::open(f.db_path()).unwrap();
    assert_eq!(sql.execute("UPDATE merge_queue SET agent_attempts=1,result_oid='stale',conflict_paths='stale-path',error_detail='stale-diagnostic' WHERE worktree=?1",[held.to_str().unwrap()]).unwrap(),1, "seeding the held row must update exactly one row");
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
        1,
        "moving the clean row enqueue time must update exactly one row"
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
        ["held", "clean"],
        "seeded queue must be held then clean, in that order"
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
    assert_summary(&first, &["clean"], &[], &["held"]);
    let after = f.snapshot();
    assert_eq!(
        after, before,
        "first drain must leave refs, registrations and files unchanged"
    );
    let rows = f.rows();
    assert_eq!(
        rows.len(),
        2,
        "queue must still hold two rows after the first drain"
    );
    let blocked = row(&rows, "held");
    let old = row(&rows_before, "held");
    assert_eq!(
        blocked.status, "agent_blocked",
        "held row must be agent_blocked after the first drain"
    );
    assert_eq!(
        blocked.agent_attempts, 1,
        "held row must have consumed exactly one agent attempt"
    );
    assert!(
        blocked.result_oid.is_none() && blocked.conflict_paths.is_none(),
        "agent_blocked row must have no result oid or conflict paths"
    );
    let detail = blocked.error_detail.as_deref().unwrap_or_default();
    assert!(detail.starts_with(HOLD), "{detail}");
    assert!(detail.contains("never fall back to the host"), "{detail}");
    assert_eq!(
        blocked.queued_at, old.queued_at,
        "agent_blocked row must keep its original enqueue time"
    );
    assert_eq!(
        blocked.worktree, old.worktree,
        "agent_blocked row must keep its original worktree"
    );
    assert_eq!(
        blocked.branch, old.branch,
        "agent_blocked row must keep its original branch"
    );
    assert_eq!(
        blocked.target_branch, old.target_branch,
        "agent_blocked row must keep its original target branch"
    );
    assert_eq!(
        blocked.location, old.location,
        "agent_blocked row must keep its original location"
    );
    assert_eq!(
        row(&rows, "clean").status,
        "ready",
        "clean row must be ready after the first drain"
    );
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
            statuses,
            "audit trail for {path} must be {statuses:?}"
        );
    }
    assert_eq!(
        audit.len(),
        4,
        "audit trail must contain exactly four status transitions"
    );
    assert_eq!(
        audit,
        vec![
            (held.to_str().unwrap().into(), "folding".into()),
            (held.to_str().unwrap().into(), "agent_blocked".into()),
            (clean.to_str().unwrap().into(), "folding".into()),
            (clean.to_str().unwrap().into(), "ready".into())
        ],
        "audit trail must be folding then agent_blocked for held, folding then ready for clean"
    );
    assert!(
        !f.root.path().join("agent-ran").exists(),
        "agent sentinel must not run during the first drain"
    );
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
    assert_ne!(new_tip, old_tip, "repair must move the held branch tip");
    let delta = f.git(&repo, &["diff", "--name-only", &old_tip, &new_tip]);
    let expected = match case {
        Case::Conflict => "conflict.txt\nrepaired.txt",
        Case::RedGate => "red-gate-marker\nrepaired.txt",
    };
    assert_eq!(
        delta, expected,
        "repair must change exactly the expected paths"
    );
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
            .replace(&format!("HEAD {old_tip}"), &format!("HEAD {new_tip}")),
        "repair must change only the HEAD line of worktree registrations"
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
    assert_eq!(
        second_before["files"],
        Value::Object(expected_files),
        "repair must change only the expected files"
    );
    f.record(
        "repair",
        json!({"old_tip":old_tip,"new_tip":new_tip,"changed_paths":delta,"snapshot":second_before}),
    );
    let second = f.json(&["merge", "drain", "--json"]);
    assert_summary(&second, &["held"], &[], &[]);
    let second_after = f.snapshot();
    assert_eq!(
        second_after, second_before,
        "second drain must leave the snapshot unchanged"
    );
    let final_rows = f.rows();
    let ready = row(&final_rows, "held");
    assert_eq!(ready.status, "ready", "recovered held row must be ready");
    assert_eq!(
        ready.agent_attempts, 1,
        "recovered held row must keep its consumed agent attempt"
    );
    assert!(
        ready.conflict_paths.is_none(),
        "recovered held row must have no conflict paths"
    );
    assert_eq!(
        ready.error_detail.as_deref(),
        Some("gated green — awaiting land\nuntrusted: ran on host, not isolated"),
        "recovered held row must report gated green, awaiting land"
    );
    assert_eq!(
        ready.queued_at, blocked.queued_at,
        "recovered held row must keep its original enqueue time"
    );
    assert_eq!(
        ready.worktree, blocked.worktree,
        "recovered held row must keep its original worktree"
    );
    assert_eq!(
        ready.branch, blocked.branch,
        "recovered held row must keep its original branch"
    );
    assert_eq!(
        ready.target_branch, blocked.target_branch,
        "recovered held row must keep its original target branch"
    );
    assert_eq!(
        ready.location, blocked.location,
        "recovered held row must keep its original location"
    );
    assert_eq!(
        row(&final_rows, "clean"),
        row(&rows, "clean"),
        "clean row must be unchanged by the repair drain"
    );
    let oid = ready.result_oid.as_ref().unwrap();
    assert_eq!(
        f.git(&repo, &["cat-file", "-t", oid]),
        "commit",
        "result oid of the held row must name a commit"
    );
    assert_eq!(
        f.audit(),
        vec![
            (held.to_str().unwrap().into(), "folding".into()),
            (held.to_str().unwrap().into(), "ready".into())
        ],
        "held row audit must be folding then ready"
    );
    assert!(
        !f.root.path().join("agent-ran").exists(),
        "agent sentinel must not run during the repair drain"
    );
    let final_list = f.json(&["merge", "list", "--json"]);
    assert_eq!(
        final_list.as_array().unwrap().len(),
        2,
        "merge list must still report two rows"
    );
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

// Both drains are custody-owned CLI groups, while each host fake agent runs in
// the separate group created by agent_run. The feature commit contains
// `red-gate-marker`, so every fold reaches the red gate (GATE). In
// merge_driver.rs, the agent branch consumes the claim attempt and writes
// `agent_running` before runner start (the `agent fixing (n/3)` status write at
// 1316-1318), leaving phase `claimed_no_child`. cmd/merge.rs's live-claim
// `TargetStampAction::SkipHeld` path (961-985) makes the second CLI emit
// "claimed by another drain" without changing status, attempts, or generation.
// Releasing the first agent lets that owner repeat the red gate and agent cycle
// through attempt 3; merge_driver.rs's terminal `agent_runs > 0` arm (1406+)
// writes `needs_human`, then claim finish leaves phase `unclaimed` (1478-1481).
// The guard records one `agent-start`/`agent-end` pair per owner attempt, and
// auto-land is disabled so `refs/heads/main` stays at its captured commit.
#[test]
fn race_agent_two_cli_drains_dispatch_one_agent() {
    let mut f = agent_row_fixture();
    let mut agent_release = f.agent_guard.take().unwrap();
    let repo = f.repo.clone();
    let root = f.root.path().to_path_buf();
    let target_before = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    assert!(owner.pid() > 0, "owner drain must have a live process id");
    wait_gate_count(&root, "agent-runs", "agent-start ", 1);
    let owner_state = row_state(&f);
    assert_eq!(
        owner_state.status, "agent_running",
        "owner must hold the row in agent_running"
    );
    assert_eq!(
        owner_state.agent_attempts, 1,
        "owner must have consumed exactly one agent attempt"
    );
    assert_eq!(
        owner_state.phase, "claimed_no_child",
        "owner must have written claimed_no_child before the agent starts"
    );

    let loser = f.json(&["merge", "drain", "--json"]);
    assert!(
        loser["warnings"].as_array().unwrap().iter().any(|warning| {
            warning.as_str().is_some_and(|warning| {
                warning.contains("feature") && warning.contains("claimed by another drain")
            })
        }),
        "second drain must warn that the feature row is claimed by another drain"
    );
    assert_eq!(
        gate_count(&root, "agent-runs", "agent-start "),
        1,
        "second drain must not start another agent"
    );
    let after_loser = row_state(&f);
    assert_eq!(
        after_loser.agent_attempts, owner_state.agent_attempts,
        "second drain must not consume agent attempts"
    );
    assert_eq!(
        after_loser.generation, owner_state.generation,
        "second drain must not change the claim generation"
    );
    assert_eq!(
        after_loser.status, owner_state.status,
        "second drain must not change the row status"
    );

    release_gate(&root, "agent-hold", "agent-open");
    let winner = f.finish_cli(owner);
    assert!(
        winner.status.success(),
        "owner drain must exit successfully after the agent is released"
    );
    let winner_json: Value = serde_json::from_str(&winner.stdout).unwrap();
    assert_eq!(
        winner_json["needs_human"],
        json!(["feature"]),
        "owner drain must report feature as needs_human"
    );
    let final_row = row_state(&f);
    assert_eq!(
        final_row.status, "needs_human",
        "row must end as needs_human"
    );
    assert_eq!(
        final_row.agent_attempts, 3,
        "row must have exhausted three agent attempts"
    );
    assert_eq!(
        final_row.phase, "unclaimed",
        "needs_human row must be unclaimed"
    );
    assert_eq!(
        gate_count(&root, "agent-runs", "agent-start "),
        3,
        "exactly three agent attempts must start"
    );
    assert_eq!(
        gate_count(&root, "agent-runs", "agent-end "),
        3,
        "every started agent attempt must end"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_before,
        "needs_human run must leave main at its captured commit"
    );
    agent_release.disarm();
    f.close();
}

// The drain CLI is killed in its custody-owned group, but agent_run starts its
// child in a separate owned group, so the blocked fake agent survives. Its
// completed attempt leaves `agent_running`/`claimed_no_child`; status and phase
// are written before runner start (merge_driver.rs:1316-1318). After the test
// expires only the persisted claim lease, merge_driver.rs's
// `recoverable_in_progress_status` admits `folding`/`verifying` only
// (642-651), and the stale `agent_running` branch reports "not claimable"
// (751-800) without claiming or dispatching it. Therefore status, attempts,
// generation and phase remain the crashed values, the start count is still
// one, and the non-auto-land target ref is unchanged.
#[test]
fn race_agent_killed_owner_row_is_not_recovered_after_lease_expiry() {
    let mut f = agent_row_fixture();
    let mut agent_release = f.agent_guard.take().unwrap();
    let repo = f.repo.clone();
    let root = f.root.path().to_path_buf();
    let worktree = f.held.to_string_lossy().into_owned();
    let target_before = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    wait_gate_count(&root, "agent-runs", "agent-start ", 1);
    let killed = f.kill_cli(owner);
    assert_eq!(
        killed.status.signal(),
        Some(libc::SIGKILL),
        "killed owner must die by SIGKILL"
    );
    let crashed = row_state(&f);
    assert_eq!(
        crashed.status, "agent_running",
        "crashed owner row must be agent_running"
    );
    assert_eq!(
        crashed.agent_attempts, 1,
        "crashed owner row must have consumed one agent attempt"
    );
    assert_eq!(
        crashed.phase, "claimed_no_child",
        "crashed owner row must be claimed_no_child"
    );

    release_gate(&root, "agent-hold", "agent-open");
    wait_gate_count(&root, "agent-runs", "agent-end ", 1);
    expire_lease(&f, &worktree);
    let recovered = f.json(&["merge", "drain", "--json"]);
    assert!(
        recovered["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                warning.as_str().is_some_and(|warning| {
                    warning.contains("feature: agent_running row is not claimable")
                })
            }),
        "recovery drain must warn that the agent_running row is not claimable"
    );
    let after = row_state(&f);
    assert_eq!(
        after.status, crashed.status,
        "row status must be unchanged after the recovery refusal"
    );
    assert_eq!(
        after.agent_attempts, crashed.agent_attempts,
        "row agent attempts must be unchanged after the recovery refusal"
    );
    assert_eq!(
        after.generation, crashed.generation,
        "row generation must be unchanged after the recovery refusal"
    );
    assert_eq!(
        after.phase, crashed.phase,
        "row phase must be unchanged after the recovery refusal"
    );
    assert_eq!(
        gate_count(&root, "agent-runs", "agent-start "),
        1,
        "the agent must still start only once"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_before,
        "recovery refusal must leave main at its captured commit"
    );
    agent_release.disarm();
    f.close();
}

// Both CLI processes use custody-owned groups and the same private fixture DB.
// A holds its gate in the gate runner's separate group after RowClaim admission;
// no lease time is simulated, so B observes A's live claim and emits the
// claimed-by-another warning before attempting the gate (merge_driver.rs:761-779).
// Releasing A lets its gate pass and the drain writes `ready` then releases the
// claim (merge_driver.rs:1075-1088, 1478-1481). Thus only one gate start occurs,
// B changes neither attempts nor generation/status, and the target ref remains
// at its captured value because this fixture disables auto-land.
#[test]
fn race_two_cli_drains_have_one_owner() {
    let mut f = one_row_fixture(FixtureOpts {
        gate: String::new(),
        auto_land: false,
        agent: AgentMode::HeldAtFloor,
    });
    let repo = f.repo.clone();
    let before_ref = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    let root = f.root.path().to_path_buf();
    make_gate_fifo(&root, "gate-hold");
    let mut gate_release = GateReleaseGuard::new(&root, "gate-hold", "gate-open");
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    assert!(owner.pid() > 0, "owner drain must have a live process id");
    wait_gate_count(&root, "gate-runs", "start ", 1);
    let owner_claimed = row_state(&f);
    let loser = f.json(&["merge", "drain", "--json"]);
    assert!(
        loser["warnings"].as_array().unwrap().iter().any(|warning| {
            warning.as_str().is_some_and(|warning| {
                warning.contains("feature") && warning.contains("claimed by another drain")
            })
        }),
        "second drain must warn that the feature row is claimed by another drain"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "second drain must not start a second gate"
    );
    let after_loser = row_state(&f);
    assert_eq!(
        after_loser.agent_attempts, owner_claimed.agent_attempts,
        "loser must not consume attempts"
    );
    assert_eq!(
        after_loser.generation, owner_claimed.generation,
        "loser must not claim the row"
    );
    assert_eq!(
        after_loser.status, owner_claimed.status,
        "second drain must not change the row status"
    );
    release_gate(&root, "gate-hold", "gate-open");
    let winner = f.finish_cli(owner);
    assert!(
        winner.status.success(),
        "owner drain must exit successfully after the gate is released"
    );
    let winner_json: Value = serde_json::from_str(&winner.stdout).unwrap();
    assert_eq!(
        winner_json["ready"],
        json!(["feature"]),
        "owner drain must report feature as ready"
    );
    assert_eq!(
        row_state(&f).status,
        "ready",
        "row must be ready after the owner finishes"
    );
    assert_eq!(
        row_state(&f).phase,
        "unclaimed",
        "ready row must be unclaimed"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "exactly one gate must start"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        before_ref,
        "drain must leave main at its captured commit with auto-land disabled"
    );
    gate_release.disarm();
    f.close();
}

// `wt rm` removes the checkout while the drain owns the row, but the
// claim-aware DB cleanup keeps that row until the owner can settle it. The
// target ref must not move while the gate is held. Once released, a successful
// land is valid only if it advanced the target; otherwise the missing checkout
// must produce a held row with a diagnostic rather than a false landed result.
#[test]
fn race_worktree_delete_keeps_the_claimed_row() {
    let mut f = one_row_fixture(FixtureOpts {
        gate: String::new(),
        auto_land: true,
        agent: AgentMode::HeldAtFloor,
    });
    let repo = f.repo.clone();
    let worktree = f.held.clone();
    let worktree_s = worktree.to_string_lossy().into_owned();
    let root = f.root.path().to_path_buf();
    let target_before = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    make_gate_fifo(&root, "gate-hold");
    let mut gate_release = GateReleaseGuard::new(&root, "gate-hold", "gate-open");
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    wait_gate_count(&root, "gate-runs", "start ", 1);
    let held = row_state(&f);
    assert_eq!(
        held.status, "folding",
        "held row must be folding while the gate blocks"
    );
    assert_ne!(
        held.phase, "unclaimed",
        "held row must be claimed while the gate blocks"
    );

    let removed = f.cli(&["wt", "rm", &worktree_s, "--force"]);
    assert!(removed.status.success(), "{}", removed.stderr);
    assert!(!worktree.exists(), "worktree must be removed from disk");
    let after_remove = row_state(&f);
    assert_eq!(
        after_remove.status, held.status,
        "wt rm must not change the row status"
    );
    assert_eq!(
        after_remove.generation, held.generation,
        "wt rm must not change the claim generation"
    );
    assert_eq!(
        after_remove.phase, held.phase,
        "wt rm must not change the claim phase"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_before,
        "wt rm must not move main"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "wt rm must not start a second gate"
    );

    release_gate(&root, "gate-hold", "gate-open");
    let finished = f.finish_cli(owner);
    assert!(finished.status.success(), "{}", finished.stderr);
    wait_gate_count(&root, "gate-runs", "end ", 1);
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "exactly one gate must start"
    );
    let finished_json: Value = serde_json::from_str(&finished.stdout).unwrap();
    let final_row = row_state(&f);
    assert_eq!(
        final_row.phase, "unclaimed",
        "finished row must be unclaimed"
    );
    assert_eq!(
        final_row.generation, held.generation,
        "finished row must keep the claim generation"
    );
    let target_after = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    if final_row.status == "landed" {
        assert_ne!(target_after, target_before, "landed run must advance main");
        assert_eq!(
            final_row.result_oid.as_deref(),
            Some(target_after.as_str()),
            "landed row must record the new main commit"
        );
        assert_eq!(
            finished_json["landed"],
            json!(["feature"]),
            "landed run must report feature as landed"
        );
    } else {
        assert!(
            matches!(
                final_row.status.as_str(),
                "deferred" | "gate_failed" | "gate_error" | "agent_blocked" | "needs_human"
            ),
            "missing checkout must not settle as an unheld row: {final_row:?}"
        );
        assert!(
            final_row
                .error_detail
                .as_deref()
                .is_some_and(|detail| !detail.trim().is_empty()),
            "held row must carry a non-empty diagnostic"
        );
        assert_eq!(
            target_after, target_before,
            "non-landed run must leave main unchanged"
        );
        assert!(
            finished_json["landed"]
                .as_array()
                .is_none_or(|landed| landed.iter().all(|branch| branch != "feature")),
            "non-landed run must not report feature as landed"
        );
    }
    gate_release.disarm();
    f.close();
}

// The sweep only considers landed rows. While the real drain is blocked in
// its gate, move the claimed row into the post-land observation window to
// exercise the cleanup fence: sweep must report merge_claimed and leave both
// the checkout and queue row untouched. The owner then completes and releases
// the claim normally.
#[test]
fn race_merge_sweep_skips_a_claimed_row() {
    let mut f = one_row_fixture(FixtureOpts {
        gate: String::new(),
        auto_land: true,
        agent: AgentMode::HeldAtFloor,
    });
    let config = fs::read_to_string(&f.config).unwrap().replace(
        "on_landed = \"off\"",
        "on_landed = \"expire\"\nmerged_ttl_secs = 1",
    );
    fs::write(&f.config, config).unwrap();
    let repo = f.repo.clone();
    let worktree = f.held.clone();
    let worktree_s = worktree.to_string_lossy().into_owned();
    let root = f.root.path().to_path_buf();
    let target_before = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    let landed_oid = f.git(&repo, &["rev-parse", "feature"]);
    make_gate_fifo(&root, "gate-hold");
    let mut gate_release = GateReleaseGuard::new(&root, "gate-hold", "gate-open");
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    wait_gate_count(&root, "gate-runs", "start ", 1);
    let held = row_state(&f);
    assert_eq!(
        held.status, "folding",
        "held row must be folding while the gate blocks"
    );
    assert_ne!(
        held.phase, "unclaimed",
        "held row must be claimed while the gate blocks"
    );

    let db = Connection::open(f.db_path()).unwrap();
    db.execute(
        "UPDATE merge_queue SET status='landed',result_oid=?1,updated_at=1 WHERE worktree=?2",
        rusqlite::params![landed_oid, worktree_s],
    )
    .unwrap();
    drop(db);
    let claimed_landed = row_state(&f);
    assert_eq!(
        claimed_landed.status, "landed",
        "row must be landed after the simulated land"
    );
    assert_eq!(
        claimed_landed.generation, held.generation,
        "simulated land must keep the claim generation"
    );
    assert_eq!(
        claimed_landed.phase, held.phase,
        "simulated land must keep the claim phase"
    );

    let sweep = f.cli_failing(&["merge", "sweep", "--json"]);
    let sweep_json: Value = serde_json::from_str(&sweep.stdout).unwrap();
    assert_eq!(
        sweep_json["collected"],
        json!([]),
        "sweep must collect nothing while the claim is live"
    );
    assert_eq!(
        sweep_json["cleared_rows"],
        json!([]),
        "sweep must clear no rows while the claim is live"
    );
    assert_eq!(
        sweep_json["kept"].as_array().unwrap().len(),
        1,
        "sweep must keep exactly one row while the claim is live"
    );
    assert_eq!(
        sweep_json["kept"][0]["branch"], "feature",
        "sweep must keep the feature branch"
    );
    assert_eq!(
        sweep_json["kept"][0]["class"], "merge_claimed",
        "sweep must classify the kept row as merge_claimed"
    );
    assert_eq!(
        row_state(&f),
        claimed_landed,
        "sweep must leave the claimed row untouched"
    );
    assert!(
        worktree.exists(),
        "sweep must leave the claimed checkout on disk"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_before,
        "sweep must leave main unchanged"
    );

    release_gate(&root, "gate-hold", "gate-open");
    let finished = f.finish_cli(owner);
    assert!(finished.status.success(), "{}", finished.stderr);
    wait_gate_count(&root, "gate-runs", "end ", 1);
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "exactly one gate must start"
    );
    assert_eq!(
        row_state(&f).phase,
        "unclaimed",
        "owner must release the claim after the gate ends"
    );
    gate_release.disarm();
    f.close();
}

// The owner drain holds its FIFO-blocked gate in a separate process group
// after claiming the row. The rm, retry, and clear commands are fresh custody-
// owned CLI groups; each must observe that live claim without changing the row.
// Releasing the gate lets the original owner write ready and release its claim.
// The lease clock is not simulated, so no recovery path can admit a second
// owner; the single gate start and unchanged target ref prove no duplicate fold.
#[test]
fn race_operator_verbs_are_refused_while_a_drain_holds_the_row() {
    let mut f = one_row_fixture(FixtureOpts {
        gate: String::new(),
        auto_land: false,
        agent: AgentMode::HeldAtFloor,
    });
    let repo = f.repo.clone();
    let root = f.root.path().to_path_buf();
    let target_before = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    make_gate_fifo(&root, "gate-hold");
    let mut gate_release = GateReleaseGuard::new(&root, "gate-hold", "gate-open");
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    wait_gate_count(&root, "gate-runs", "start ", 1);
    let held = row_state(&f);
    assert_ne!(
        held.phase, "unclaimed",
        "held row must be claimed while the gate blocks"
    );
    assert!(
        held.generation >= 1,
        "held row must carry a fencing generation"
    );
    let wt = f.held.to_str().unwrap().to_owned();

    let rm = f.cli_failing(&["merge", "rm", "--worktree", &wt]);
    assert!(
        format!("{}{}", rm.stdout, rm.stderr)
            .contains("is being worked by a drain; remove it after the drain finishes"),
        "rm refusal must name the active drain"
    );
    assert_eq!(
        row_state(&f),
        held,
        "rm refusal must leave the row unchanged"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "rm refusal must not start a second gate"
    );

    let retry = f.cli_failing(&["merge", "retry", "--worktree", &wt]);
    assert!(
        format!("{}{}", retry.stdout, retry.stderr)
            .contains("is being worked by a drain; retry it after the drain finishes"),
        "retry refusal must name the active drain"
    );
    assert_eq!(
        row_state(&f),
        held,
        "retry refusal must leave the row unchanged"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "retry refusal must not start a second gate"
    );

    let clear = f.cli_failing(&["merge", "clear"]);
    assert!(
        format!("{}{}", clear.stdout, clear.stderr).contains("claimed by an active drain"),
        "clear refusal must name the active drain"
    );
    assert_eq!(
        row_state(&f),
        held,
        "clear refusal must leave the row unchanged"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "clear refusal must not start a second gate"
    );

    assert_eq!(
        event_rows(&f, "retry")
            .iter()
            .filter(|outcome| outcome.as_str() == "claimed")
            .count(),
        1,
        "exactly one retry must be recorded as claimed"
    );
    assert_eq!(
        event_rows(&f, "remove")
            .iter()
            .filter(|outcome| outcome.as_str() == "claimed")
            .count(),
        2,
        "rm and clear each record the claimed refusal as a remove event"
    );

    release_gate(&root, "gate-hold", "gate-open");
    let winner = f.finish_cli(owner);
    assert!(
        winner.status.success(),
        "owner drain must exit successfully after the gate is released"
    );
    let winner_json: Value = serde_json::from_str(&winner.stdout).unwrap();
    assert_eq!(
        winner_json["ready"],
        json!(["feature"]),
        "owner drain must report feature as ready"
    );
    let after_drain = row_state(&f);
    assert_eq!(
        after_drain.status, "ready",
        "row must be ready after the owner finishes"
    );
    assert_eq!(
        after_drain.phase, "unclaimed",
        "ready row must be unclaimed"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "exactly one gate must start"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_before,
        "drain must leave main at its captured commit"
    );

    f.cli(&["merge", "rm", "--worktree", &wt]);
    assert!(
        f.rows().is_empty(),
        "rm after the drain must remove the only queue row"
    );
    gate_release.disarm();
    f.close();
}

// The drain process owns the row claim while its gate process blocks on the
// fixture FIFO. `merge land` is a separate custody-owned CLI process; the row
// must still be folding when it performs the ready precheck. The lease clock
// is not simulated, so no recovery path can admit another drain. Releasing the
// FIFO completes the original gate exactly once without moving either ref or
// adding another land audit event.
#[test]
fn race_merge_land_is_refused_while_a_drain_holds_the_row() {
    let mut f = one_row_fixture(FixtureOpts {
        gate: String::new(),
        auto_land: false,
        agent: AgentMode::HeldAtFloor,
    });
    let repo = f.repo.clone();
    let worktree = f.held.clone();
    let root = f.root.path().to_path_buf();
    let target_before = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    let worktree_tip_before = f.git(&worktree, &["rev-parse", "HEAD"]);
    make_gate_fifo(&root, "gate-hold");
    let mut gate_release = GateReleaseGuard::new(&root, "gate-hold", "gate-open");
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    wait_gate_count(&root, "gate-runs", "start ", 1);
    let held = row_state(&f);
    assert_ne!(
        held.phase, "unclaimed",
        "held row must be claimed while the gate blocks"
    );
    assert_eq!(
        held.status, "folding",
        "held row must be folding while the gate blocks"
    );
    assert!(
        held.generation >= 1,
        "held row must carry a fencing generation"
    );

    let wt = worktree.to_str().unwrap().to_owned();
    let land = f.cli_failing(&["merge", "land", "--worktree", &wt]);
    assert!(
        format!("{}{}", land.stdout, land.stderr).contains("merge-queue row is folding, not ready"),
        "land refusal must report that the row is folding"
    );
    assert_eq!(
        row_state(&f),
        held,
        "land refusal must leave the row unchanged"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "land refusal must not start a second gate"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_before,
        "land refusal must leave main unchanged"
    );
    assert_eq!(
        f.git(&worktree, &["rev-parse", "HEAD"]),
        worktree_tip_before,
        "land refusal must leave the worktree HEAD unchanged"
    );
    assert_eq!(
        event_rows(&f, "land"),
        vec!["not_ready".to_string()],
        "land must record one not_ready event"
    );

    release_gate(&root, "gate-hold", "gate-open");
    let winner = f.finish_cli(owner);
    assert!(
        winner.status.success(),
        "owner drain must exit successfully after the gate is released"
    );
    let winner_json: Value = serde_json::from_str(&winner.stdout).unwrap();
    assert_eq!(
        winner_json["ready"],
        json!(["feature"]),
        "owner drain must report feature as ready"
    );
    let after_drain = row_state(&f);
    assert_eq!(
        after_drain.status, "ready",
        "row must be ready after the owner finishes"
    );
    assert_eq!(
        after_drain.phase, "unclaimed",
        "ready row must be unclaimed"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        1,
        "exactly one gate must start"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_before,
        "drain must leave main unchanged"
    );
    assert_eq!(
        f.git(&worktree, &["rev-parse", "HEAD"]),
        worktree_tip_before,
        "drain must leave the worktree HEAD unchanged"
    );
    assert_eq!(
        event_rows(&f, "land"),
        vec!["not_ready".to_string()],
        "land must still record exactly one not_ready event"
    );

    f.cli(&["merge", "rm", "--worktree", &wt]);
    assert!(
        f.rows().is_empty(),
        "rm after the drain must remove the only queue row"
    );
    gate_release.disarm();
    f.close();
}

// The fixture gives each CLI a custody-owned process group, while
// `integrate_gate::Workspace::spawn` puts the gate in its own group. SIGKILL
// therefore kills owner A but leaves its FIFO-blocked gate to write `end`.
// The lease is the only simulated fact (`expire_lease` sets it to now - 1);
// A's claim wrote `folding`, generation, and attempts in the real DB. Recovery
// admits with `queued` (merge_driver.rs:797-825), writes `folding` at
// merge_driver.rs:911, and the Ready arm writes `ready` at merge_driver.rs:1075-1088;
// claim finish releases it at merge_driver.rs:1478-1481. Finish preserves the
// attempt count. Thus gate starts/ends are 2/2 and the killed owner's signal
// status is SIGKILL.
#[test]
fn race_killed_owner_row_is_recovered_after_lease_expiry() {
    let mut f = one_row_fixture(FixtureOpts {
        gate: String::new(),
        auto_land: false,
        agent: AgentMode::HeldAtFloor,
    });
    let root = f.root.path().to_path_buf();
    let worktree = f.held.to_string_lossy().into_owned();
    make_gate_fifo(&root, "gate-hold");
    let mut gate_release = GateReleaseGuard::new(&root, "gate-hold", "gate-open");
    let before = row_state(&f);
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    wait_gate_count(&root, "gate-runs", "start ", 1);
    let killed = f.kill_cli(owner);
    assert_eq!(
        killed.status.signal(),
        Some(libc::SIGKILL),
        "killed owner must die by SIGKILL"
    );
    let crashed = row_state(&f);
    assert_eq!(
        crashed.status, "folding",
        "crashed owner row must be folding"
    );
    assert_ne!(
        crashed.phase, "unclaimed",
        "crashed owner row must be claimed"
    );
    // The gate runs in its own process group and outlives the killed drain.
    release_gate(&root, "gate-hold", "gate-open");
    wait_gate_count(&root, "gate-runs", "end ", 1);
    // Only the lease clock is simulated; claim generation, status, and owner
    // state were written by the killed CLI process itself.
    expire_lease(&f, &worktree);
    fs::write(root.join("gate-open"), "open\n").unwrap();
    let recovered = f.json(&["merge", "drain", "--json"]);
    let after = row_state(&f);
    assert_eq!(after.status, "ready", "recovered row must be ready");
    assert_eq!(after.phase, "unclaimed", "recovered row must be unclaimed");
    assert!(
        after.generation > crashed.generation,
        "recovery must advance the fencing generation"
    );
    assert_eq!(
        after.agent_attempts, before.agent_attempts,
        "recovery preserves agent attempts"
    );
    assert!(
        recovered["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|warning| {
                warning
                    .as_str()
                    .is_none_or(|warning| !warning.contains("not claimable"))
            }),
        "recovery must not report the row as not claimable"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        2,
        "recovery must start the gate a second time"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "end "),
        2,
        "every gate start must end"
    );
    gate_release.disarm();
    f.close();
}

// CLI A owns the first FIFO-blocked gate in its own process group; its lease
// is expired in SQLite while A is still alive. CLI C recovers with a new claim
// generation, folds and passes the now-open gate, then CAS-advances
// `refs/heads/main` and writes `landed` plus the result OID. When A's gate is
// released, `attempt_land` sees C's target commit as an ancestor and returns
// `UpToDate` (integrate.rs:2938-2943); the UpToDate arm writes `landed` and
// `stop_on_write_error!` catches a lost write (merge_driver.rs:1061-1064,
// 877-889). Claim finish's Err(_) arm deduplicates the warning
// (merge_driver.rs:1478-1522). Hence A exits 0, cannot change the landed row/ref,
// and total gate starts/ends are 2/2.
#[test]
fn race_late_former_owner_cannot_overwrite_recovered_row() {
    let mut f = one_row_fixture(FixtureOpts {
        gate: String::new(),
        auto_land: true,
        agent: AgentMode::HeldAtFloor,
    });
    let root = f.root.path().to_path_buf();
    let repo = f.repo.clone();
    let worktree = f.held.to_string_lossy().into_owned();
    make_gate_fifo(&root, "gate-hold");
    let mut gate_release = GateReleaseGuard::new(&root, "gate-hold", "gate-open");
    let target_before = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    let owner = f.start_cli(&["merge", "drain", "--json"]);
    wait_gate_count(&root, "gate-runs", "start ", 1);
    expire_lease(&f, &worktree);
    fs::write(root.join("gate-open"), "open\n").unwrap();
    let recovered = f.json(&["merge", "drain", "--json"]);
    assert_eq!(
        recovered["landed"],
        json!(["feature"]),
        "recovered drain must report feature as landed"
    );
    let after_recovery = row_state(&f);
    assert_eq!(
        after_recovery.status, "landed",
        "recovered row must be landed"
    );
    let result_oid = after_recovery
        .result_oid
        .clone()
        .expect("landed row records result oid");
    assert_ne!(result_oid, "", "landed row must record a result oid");
    assert_ne!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_before,
        "recovered drain advances the target once"
    );
    let target_after_recovery = f.git(&repo, &["rev-parse", "refs/heads/main"]);
    assert_eq!(
        target_after_recovery, result_oid,
        "main must point at the recovered result oid"
    );
    release_gate(&root, "gate-hold", "gate-open");
    let former = f.finish_cli(owner);
    assert_eq!(
        former.status.code(),
        Some(0),
        "late former owner must exit 0"
    );
    let former_json: Value = serde_json::from_str(&former.stdout).unwrap();
    assert!(
        former_json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                warning.as_str()
                    == Some("feature: merge claim lost; another drain recovered the row")
            }),
        "late former owner must warn that its claim was lost"
    );
    let final_row = row_state(&f);
    assert_eq!(
        final_row.status, "landed",
        "late former owner must leave the row landed"
    );
    assert_eq!(
        final_row.phase, "unclaimed",
        "late former owner must leave the row unclaimed"
    );
    assert_eq!(
        final_row.result_oid.as_deref(),
        Some(result_oid.as_str()),
        "late former owner must keep the recovered result oid"
    );
    assert_eq!(
        final_row.generation, after_recovery.generation,
        "late former owner must keep the recovered generation"
    );
    assert_eq!(
        f.git(&repo, &["rev-parse", "refs/heads/main"]),
        target_after_recovery,
        "late former owner must not move target after recovery"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "start "),
        2,
        "exactly two gate starts must occur"
    );
    assert_eq!(
        gate_count(&root, "gate-runs", "end "),
        2,
        "every gate start must end"
    );
    gate_release.disarm();
    f.close();
}
