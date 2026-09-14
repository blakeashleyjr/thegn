use super::*;
use crate::integrate::{gate_tip, run_fold};
use std::time::{Duration, Instant};
use thegn_core::fold::Branch;

#[test]
#[expect(clippy::disallowed_methods)]
fn replacement_refs_never_change_gate_materialization_or_inherited_git_view() {
    fn assert_explicit_no_replace(command: &std::process::Command) {
        // get_envs reports explicit assignments, not the parent's inherited
        // environment. A native parent already setting 1 cannot mask deletion
        // of either production setter.
        let assignment = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("GIT_NO_REPLACE_OBJECTS"))
            .map(|(_, value)| value);
        assert_eq!(assignment, Some(Some(std::ffi::OsStr::new("1"))));
    }
    let f = Fixture::new();
    assert_explicit_no_replace(&gate_git(&f.repo));
    if !f.supported_or_refused() {
        return;
    }
    git(&f.repo, &["replace", &f.first, &f.second]);
    // A full native gate itself exports the corrected environment. Explicitly
    // enable replacements for this private counterexample only, not globally.
    let replaced = util::git_cmd(&f.repo)
        .env_remove("GIT_NO_REPLACE_OBJECTS")
        .args(["show", &format!("{}:value", f.first)])
        .output()
        .unwrap();
    assert!(replaced.status.success());
    assert_eq!(replaced.stdout, b"two\n");
    let replacement = git(
        &f.repo,
        &["rev-parse", &format!("refs/replace/{}", f.first)],
    );
    assert_eq!(replacement, f.second);
    let refs = git(&f.repo, &["show-ref"]);
    let config = std::fs::read(f.repo.join(".git/config")).unwrap();
    for reuse in [true, true, false] {
        let mut gate = f.config.clone();
        gate.gate_reuse_worktree = reuse;
        gate.gate_setup_command = "test \"$GIT_NO_REPLACE_OBJECTS\" = 1 && test \"$(git show \"$THEGN_GATE_OID:value\")\" = one && test \"$(cat value)\" = one && printf setup > setup-proof".into();
        gate.gate_command = "test \"$GIT_NO_REPLACE_OBJECTS\" = 1 && test \"$(git rev-parse HEAD)\" = \"$THEGN_GATE_OID\" && test \"$(git show HEAD:value)\" = one && test \"$(cat value)\" = one && test -f setup-proof".into();
        assert!(matches!(
            gate_tip(&f.repo, &f.first, &gate).unwrap(),
            GateVerdict::Passed
        ));
        assert_eq!(git(&f.repo, &["show-ref"]), refs);
        assert_eq!(std::fs::read(f.repo.join(".git/config")).unwrap(), config);
        assert_eq!(
            git(
                &f.repo,
                &["rev-parse", &format!("refs/replace/{}", f.first)]
            ),
            replacement
        );
    }
    let workspace = f.prepare(&f.first);
    for command in ["printf setup", "printf gate"] {
        assert_explicit_no_replace(&workspace.command_for(command).unwrap());
    }
}

#[test]
fn materialization_record_parser_is_conservative_and_nul_framed() {
    for key in [
        "core.sparsecheckout",
        "core.sparsecheckoutcone",
        "index.sparse",
    ] {
        for value in ["false", "FALSE", "no", "off", "0", ""] {
            admit_config(format!("{key}\n{value}\0").as_bytes()).unwrap();
        }
        for value in ["true", "1", "yes", "on", "unexpected", "-1"] {
            assert!(admit_config(format!("{key}\n{value}\0").as_bytes()).is_err());
        }
        assert!(admit_config(format!("{key}\0").as_bytes()).is_err());
    }
    for suffix in ["clean", "smudge", "process"] {
        admit_config(format!("filter.Private.{suffix}\n\0").as_bytes()).unwrap();
    }
    admit_config(b"user.name\nordinary Unicode \xc3\xa9\0filter.x.required\ntrue\0").unwrap();
    assert!(admit_config(b"core.sparsecheckout\nfalse").is_err());
    admit_index(b"H ordinary\0H path\nwith newline\0").unwrap();
    for record in [
        b"S value\0".as_slice(),
        b"s value\0",
        b"h value\0",
        b"M value\0",
        b"H value",
        b"\0",
    ] {
        assert!(admit_index(record).is_err());
    }
}

fn assert_materialization_refusal_unchanged(f: &Fixture, admin: &Path, reason: &str) {
    let refs = git(&f.repo, &["show-ref"]);
    let paths = [
        admin.join("HEAD"),
        admin.join("index"),
        f.wt().join("value"),
        f.wt().join(".git"),
    ];
    let before: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    let mut config = f.config.clone();
    config.gate_command = "printf ran > gate-must-not-run".into();
    config.gate_setup_command = "printf ran > setup-must-not-run".into();
    let verdict = gate_tip(&f.repo, &f.first, &config).unwrap();
    assert!(matches!(verdict, GateVerdict::Error { log, .. } if log.contains(reason)));
    assert_eq!(git(&f.repo, &["show-ref"]), refs);
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "mutated {}",
            path.display()
        );
    }
    assert!(!f.wt().join("gate-must-not-run").exists());
    assert!(!f.wt().join("setup-must-not-run").exists());
}

#[test]
fn same_oid_stale_skip_worktree_and_assume_unchanged_are_held_without_repair() {
    for flag in ["--skip-worktree", "--assume-unchanged"] {
        let f = Fixture::new();
        if !f.supported_or_refused() {
            return;
        }
        let workspace = f.prepare(&f.first);
        let admin = workspace.checkout.admin.path().to_owned();
        drop(workspace);
        git(&f.wt(), &["update-index", flag, "value"]);
        std::fs::write(f.wt().join("value"), "stale private bytes\n").unwrap();
        if flag == "--skip-worktree" {
            // Actual Git counterexample: force-checkout of the same OID is
            // successful without restoring the hidden tracked file's bytes.
            git(&f.wt(), &["checkout", "--detach", "--force", &f.first]);
            assert_eq!(
                std::fs::read(f.wt().join("value")).unwrap(),
                b"stale private bytes\n"
            );
        }
        assert_materialization_refusal_unchanged(&f, &admin, "Git index state");
    }
}

#[test]
fn sparse_worktree_configuration_is_held_without_index_or_content_mutation() {
    for key in [
        "core.sparseCheckout",
        "core.sparseCheckoutCone",
        "index.sparse",
    ] {
        let f = Fixture::new();
        if !f.supported_or_refused() {
            return;
        }
        let workspace = f.prepare(&f.first);
        let admin = workspace.checkout.admin.path().to_owned();
        drop(workspace);
        git(&f.repo, &["config", "extensions.worktreeConfig", "true"]);
        git(&f.wt(), &["config", "--worktree", key, "true"]);
        let config = std::fs::read(admin.join("config.worktree")).unwrap();
        assert_materialization_refusal_unchanged(&f, &admin, "sparse Git configuration");
        assert_eq!(
            std::fs::read(admin.join("config.worktree")).unwrap(),
            config
        );
    }
}

fn private_script(f: &Fixture, name: &str, contents: &str) -> String {
    let path = f.root.path().join(name);
    std::fs::write(&path, contents).unwrap();
    format!("sh {}", util::sh_quote(path.to_str().unwrap()))
}

fn materialization_directories(f: &Fixture) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(gate_base(&f.repo))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("materialize-")
        })
        .collect();
    paths.sort();
    paths
}

#[test]
fn unused_configured_filters_are_preserved_without_execution() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let marker = f.root.path().join("unused-filter-marker");
    let command = private_script(
        &f,
        "unused-filter.sh",
        &format!(
            "printf unexpected >> {}\nexit 91\n",
            util::sh_quote(marker.to_str().unwrap())
        ),
    );
    for suffix in ["clean", "smudge", "process"] {
        git(
            &f.repo,
            &["config", &format!("filter.unused.{suffix}"), &command],
        );
    }
    let config = std::fs::read(f.repo.join(".git/config")).unwrap();
    assert!(matches!(
        gate_tip(&f.repo, &f.first, &f.config).unwrap(),
        GateVerdict::Passed
    ));
    assert!(!marker.exists());
    assert_eq!(std::fs::read(f.repo.join(".git/config")).unwrap(), config);
    assert!(materialization_directories(&f).is_empty());
}

#[test]
fn used_filter_runs_only_during_materialization_and_preserves_real_index() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let mut workspace = f.prepare(&f.first);
    let marker = f.root.path().join("smudge-marker");
    std::fs::write(&marker, "").unwrap();
    let command = private_script(
        &f,
        "smudge.sh",
        &format!(
            "printf 'smudge\\n' >> {}\nprintf 'SMUDGED:'\ncat\n",
            util::sh_quote(marker.to_str().unwrap())
        ),
    );
    git(&f.repo, &["config", "filter.canary.smudge", &command]);
    git(&f.repo, &["config", "filter.canary.required", "true"]);
    std::fs::write(f.wt().join(".gitattributes"), "value filter=canary\n").unwrap();
    let config = std::fs::read(f.repo.join(".git/config")).unwrap();
    let index = std::fs::read(workspace.checkout.admin.path().join("index")).unwrap();
    let refs = git(&f.repo, &["show-ref"]);
    for attempt in 1..=2 {
        std::fs::write(f.wt().join("value"), "stale private bytes\n").unwrap();
        workspace.verify().unwrap();
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "smudge\n".repeat(attempt - 1)
        );
        workspace = workspace
            .materialize(&AtomicBool::new(false), wait_materialization_child)
            .unwrap();
        assert_eq!(
            std::fs::read(f.wt().join("value")).unwrap(),
            b"SMUDGED:one\n"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "smudge\n".repeat(attempt)
        );
        assert_eq!(
            std::fs::read(workspace.checkout.admin.path().join("index")).unwrap(),
            index
        );
        assert!(materialization_directories(&f).is_empty());
    }
    assert_eq!(git(&f.repo, &["show-ref"]), refs);
    assert_eq!(std::fs::read(f.repo.join(".git/config")).unwrap(), config);
    drop(workspace);
    let mut gate = f.config.clone();
    gate.gate_command = "test \"$(cat value)\" = SMUDGED:one".into();
    assert!(matches!(
        gate_tip(&f.repo, &f.first, &gate).unwrap(),
        GateVerdict::Passed
    ));
}

#[test]
fn required_filter_failure_is_static_infrastructure_and_retains_private_index() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    let admin = workspace.checkout.admin.path().to_owned();
    let index = std::fs::read(admin.join("index")).unwrap();
    std::fs::write(f.wt().join(".gitattributes"), "value filter=canary\n").unwrap();
    let command = private_script(
        &f,
        "failed-smudge.sh",
        "printf 'private-secret-marker' >&2\nexit 42\n",
    );
    git(&f.repo, &["config", "filter.canary.smudge", &command]);
    git(&f.repo, &["config", "filter.canary.required", "true"]);
    let config = std::fs::read(f.repo.join(".git/config")).unwrap();
    let poison = AtomicBool::new(false);
    let error = workspace
        .materialize(&poison, wait_materialization_child)
        .err()
        .unwrap()
        .to_string();
    assert_eq!(
        error,
        "gate materialization command failed; private state retained"
    );
    assert!(!error.contains("private-secret-marker"));
    assert!(
        !poison.load(Ordering::Acquire),
        "a reaped nonzero child is not unknown ownership"
    );
    let held = materialization_directories(&f);
    assert_eq!(held.len(), 1);
    assert!(held[0].join("index").is_file());
    assert_eq!(std::fs::read(admin.join("index")).unwrap(), index);
    assert_eq!(std::fs::read(f.repo.join(".git/config")).unwrap(), config);
    Lock::acquire(&gate_base(&f.repo).join("wt.lock")).unwrap();
}

#[test]
fn unknown_wait_retains_whole_lease_and_poison_refuses_reuse_and_throwaway() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    fn reaped_but_reports_unknown(
        child: &mut std::process::Child,
    ) -> std::io::Result<std::process::ExitStatus> {
        // Fault injection exercises unknown-result ownership without leaving an
        // actually live child/zombie in the test process. Production uses wait.
        wait_materialization_child(child)?;
        Err(std::io::Error::other("injected unknown wait"))
    }
    let workspace = f.prepare(&f.first);
    let poison = AtomicBool::new(false);
    let error = workspace
        .materialize(&poison, reaped_but_reports_unknown)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("wait ownership unknown"));
    assert!(poison.load(Ordering::Acquire));
    let held = materialization_directories(&f);
    assert_eq!(held.len(), 1);
    assert!(
        held[0].join("index").is_file(),
        "unknown state was not recursively removed"
    );
    let lock_error = Lock::acquire(&gate_base(&f.repo).join("wt.lock"))
        .err()
        .unwrap();
    assert_eq!(lock_error.kind(), std::io::ErrorKind::WouldBlock);
    let refs = git(&f.repo, &["show-ref"]);
    for reuse in [true, false] {
        let mut config = f.config.clone();
        config.gate_reuse_worktree = reuse;
        let error = Workspace::prepare_with(
            &f.repo,
            &f.second,
            &config,
            &poison,
            wait_materialization_child,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("wait ownership unknown"));
        assert_eq!(materialization_directories(&f), held);
        assert_eq!(git(&f.repo, &["show-ref"]), refs);
    }
}

#[test]
fn private_index_cleanup_refuses_replacement_and_never_recurses() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    let mut index = FreshIndex::new(&workspace.parent).unwrap();
    std::fs::write(&index.path, "private original").unwrap();
    index.file = Some(Regular::open_existing(&index.path).unwrap());
    let parent = index.parent.path().to_owned();
    std::fs::rename(&index.path, parent.join("preserved-original")).unwrap();
    std::fs::write(&index.path, "replacement").unwrap();
    assert!(index.cleanup().is_err());
    assert_eq!(std::fs::read(parent.join("index")).unwrap(), b"replacement");
    assert_eq!(
        std::fs::read(parent.join("preserved-original")).unwrap(),
        b"private original"
    );
    let mut index = FreshIndex::new(&workspace.parent).unwrap();
    std::fs::write(&index.path, "owned").unwrap();
    index.file = Some(Regular::open_existing(&index.path).unwrap());
    let parent = index.parent.path().to_owned();
    std::fs::write(parent.join("unexpected-note"), "keep me").unwrap();
    assert!(index.cleanup().is_err());
    assert_eq!(
        std::fs::read(parent.join("unexpected-note")).unwrap(),
        b"keep me"
    );
}

#[test]
#[expect(clippy::disallowed_methods)]
fn fresh_indexes_restore_stale_stat_monitor_modes_links_and_missing_files() {
    for mode in ["default-stat", "weak-stat", "fsmonitor"] {
        let f = Fixture::new();
        if !f.supported_or_refused() {
            return;
        }
        let outside = f.root.path().join("outside-sentinel");
        std::fs::write(&outside, "outside unchanged\n").unwrap();
        crate::platform::symlink_file_for_test(&outside, &f.repo.join("link")).unwrap();
        std::fs::write(f.repo.join("executable"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(f.repo.join("deleted"), "restore missing tracked file\n").unwrap();
        git(&f.repo, &["add", "link", "executable", "deleted"]);
        git(&f.repo, &["update-index", "--chmod=+x", "executable"]);
        git(&f.repo, &["commit", "-qm", "materialization entries"]);
        let oid = git(&f.repo, &["rev-parse", "HEAD"]);
        if mode == "weak-stat" {
            git(&f.repo, &["config", "core.trustctime", "false"]);
            git(&f.repo, &["config", "core.checkstat", "minimal"]);
        }
        let mut workspace = Workspace::prepare(&f.repo, &oid, &f.config).unwrap();
        std::fs::create_dir_all(f.repo.join(".git/info")).unwrap();
        std::fs::write(f.repo.join(".git/info/exclude"), "/ignored-cache\n").unwrap();
        std::fs::write(f.wt().join("ignored-cache"), "warm ignored cache\n").unwrap();
        std::fs::write(f.wt().join("untracked-notes"), "keep untracked notes\n").unwrap();
        assert_eq!(
            git(&f.wt(), &["check-ignore", "ignored-cache"]),
            "ignored-cache"
        );
        let value = f.wt().join("value");
        let past = std::time::UNIX_EPOCH + Duration::from_secs(946_684_800);
        std::fs::File::options()
            .write(true)
            .open(&value)
            .unwrap()
            .set_modified(past)
            .unwrap();
        git(&f.wt(), &["update-index", "--refresh"]);
        let monitor_marker = f.root.path().join("monitor-marker");
        std::fs::write(&monitor_marker, "").unwrap();
        if mode == "fsmonitor" {
            let monitor = private_script(
                &f,
                "monitor.sh",
                &format!(
                    "printf 'monitor\\n' >> {}\nprintf 'private-token\\000'\n",
                    util::sh_quote(monitor_marker.to_str().unwrap())
                ),
            );
            git(&f.repo, &["config", "core.fsmonitor", &monitor]);
            git(&f.repo, &["config", "core.fsmonitorHookVersion", "2"]);
            git(&f.wt(), &["update-index", "--fsmonitor"]);
            git(&f.wt(), &["update-index", "--fsmonitor-valid", "value"]);
        }
        let config = std::fs::read(f.repo.join(".git/config")).unwrap();
        let refs = git(&f.repo, &["show-ref"]);
        for _attempt in 0..2 {
            std::fs::write(&value, "bad\n").unwrap(); // same byte length as one\n
            std::fs::File::options()
                .write(true)
                .open(&value)
                .unwrap()
                .set_modified(past)
                .unwrap();
            assert_eq!(std::fs::metadata(&value).unwrap().modified().unwrap(), past);
            std::fs::remove_file(f.wt().join("link")).unwrap();
            crate::platform::symlink_file_for_test(
                &f.root.path().join("unused-target"),
                &f.wt().join("link"),
            )
            .unwrap();
            std::fs::remove_file(f.wt().join("executable")).unwrap();
            std::fs::write(f.wt().join("executable"), "stale nonexecutable\n").unwrap();
            std::fs::remove_file(f.wt().join("deleted")).unwrap();
            let index = std::fs::read(workspace.checkout.admin.path().join("index")).unwrap();
            let marker = std::fs::read(&monitor_marker).unwrap();
            let tags = probe(&f.wt(), &["ls-files", "--cached", "-v", "-z"]).unwrap();
            assert!(
                tags.split(|byte| *byte == 0)
                    .any(|entry| entry == b"H value"),
                "ordinary tag alone does not prove current bytes"
            );
            workspace.verify().unwrap();
            assert_eq!(std::fs::read(&value).unwrap(), b"bad\n");
            assert_eq!(
                std::fs::read(workspace.checkout.admin.path().join("index")).unwrap(),
                index
            );
            workspace = workspace
                .materialize(&AtomicBool::new(false), wait_materialization_child)
                .unwrap();
            assert_eq!(std::fs::read(&value).unwrap(), b"one\n");
            assert_ne!(std::fs::metadata(&value).unwrap().modified().unwrap(), past);
            assert_eq!(std::fs::read_link(f.wt().join("link")).unwrap(), outside);
            assert_eq!(std::fs::read(&outside).unwrap(), b"outside unchanged\n");
            assert_eq!(
                std::fs::read(f.wt().join("executable")).unwrap(),
                b"#!/bin/sh\nexit 0\n"
            );
            assert!(
                std::process::Command::new(f.wt().join("executable"))
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(
                std::fs::read(f.wt().join("deleted")).unwrap(),
                b"restore missing tracked file\n"
            );
            assert_eq!(
                std::fs::read(f.wt().join("ignored-cache")).unwrap(),
                b"warm ignored cache\n"
            );
            assert_eq!(
                std::fs::read(f.wt().join("untracked-notes")).unwrap(),
                b"keep untracked notes\n"
            );
            assert_eq!(
                std::fs::read(workspace.checkout.admin.path().join("index")).unwrap(),
                index
            );
            assert_eq!(
                std::fs::read(&monitor_marker).unwrap(),
                marker,
                "admission/materialization must not invoke fsmonitor"
            );
            assert_eq!(git(&f.repo, &["show-ref"]), refs);
            assert_eq!(std::fs::read(f.repo.join(".git/config")).unwrap(), config);
            assert!(materialization_directories(&f).is_empty());
        }
        drop(workspace);
        std::fs::write(&value, "bad\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&value)
            .unwrap()
            .set_modified(past)
            .unwrap();
        let marker = std::fs::read(&monitor_marker).unwrap();
        let mut gate = f.config.clone();
        gate.gate_command = "test \"$(cat value)\" = one".into();
        assert!(matches!(
            gate_tip(&f.repo, &oid, &gate).unwrap(),
            GateVerdict::Passed
        ));
        assert_eq!(std::fs::read(&monitor_marker).unwrap(), marker);
    }
}

#[test]
fn oversized_configuration_probe_holds_without_echoing_values_or_mutating_checkout() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    let admin = workspace.checkout.admin.path().to_owned();
    drop(workspace);
    let config_path = f.repo.join(".git/config");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str("\n[privatecanary]\nvalue = ");
    config.push_str(&"secret-canary".repeat(200_000));
    config.push('\n');
    std::fs::write(&config_path, &config).unwrap();
    assert_materialization_refusal_unchanged(&f, &admin, "read-only Git probe unavailable");
    let log = f.assert_error(&f.first);
    assert!(!log.contains("secret-canary"));
    assert!(log.len() < 1000);
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), config);
}

struct Fixture {
    _env: crate::testenv::EnvVarGuard,
    root: tempfile::TempDir,
    repo: PathBuf,
    first: String,
    second: String,
    config: MergeQueueConfig,
}

#[expect(clippy::disallowed_methods)]
fn git(path: &Path, args: &[&str]) -> String {
    let output = util::git_cmd(path).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("thegn-gate-test-")
            .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
            .unwrap();
        let state = root.path().join("state");
        let global = root.path().join("gitconfig");
        let template = root.path().join("template");
        std::fs::write(&global, "").unwrap();
        std::fs::create_dir(&template).unwrap();
        let _env = crate::testenv::EnvVarGuard::set(&[
            ("XDG_STATE_HOME", state.to_str().unwrap()),
            (
                "XDG_CONFIG_HOME",
                root.path().join("config").to_str().unwrap(),
            ),
            ("LOCALAPPDATA", state.to_str().unwrap()),
            ("THEGN_PROFILE", ""),
            ("THEGN_DIR", root.path().to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_CONFIG_PARAMETERS", ""),
            ("GIT_TEMPLATE_DIR", template.to_str().unwrap()),
        ]);
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Private Gate"]);
        git(&repo, &["config", "user.email", "private@example.invalid"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(
            &repo,
            &["config", "core.hooksPath", template.to_str().unwrap()],
        );
        std::fs::write(repo.join("value"), "one\n").unwrap();
        git(&repo, &["add", "value"]);
        git(&repo, &["commit", "-qm", "first"]);
        let first = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("value"), "two\n").unwrap();
        git(&repo, &["commit", "-qam", "second"]);
        let second = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["branch", "candidate"]);
        git(&repo, &["reset", "--hard", &first]);
        let config = MergeQueueConfig {
            gate_command: "test \"$(git rev-parse HEAD)\" = \"$THEGN_GATE_OID\"".into(),
            gate_setup_command: String::new(),
            gate_reuse_worktree: true,
            gate_on: true,
            sign_commits: false,
            ..Default::default()
        };
        Self {
            _env,
            root,
            repo,
            first,
            second,
            config,
        }
    }

    fn supported_or_refused(&self) -> bool {
        if thegn_core::sandbox_backend::host_os() != thegn_core::sandbox_backend::HostOs::Windows {
            return true;
        }
        let before = git(&self.repo, &["rev-parse", "HEAD"]);
        assert!(matches!(
            gate_tip(&self.repo, &self.first, &self.config).unwrap(),
            GateVerdict::Error { .. }
        ));
        assert_eq!(git(&self.repo, &["rev-parse", "HEAD"]), before);
        assert!(!gate_base(&self.repo).exists());
        false
    }

    fn prepare(&self, oid: &str) -> Workspace {
        Workspace::prepare(&self.repo, oid, &self.config).unwrap()
    }
    fn wt(&self) -> PathBuf {
        gate_base(&self.repo).join("wt")
    }
    fn assert_error(&self, oid: &str) -> String {
        match gate_tip(&self.repo, oid, &self.config).unwrap() {
            GateVerdict::Error { log, .. } => log,
            other => panic!("expected infrastructure refusal: {other:?}"),
        }
    }
}

#[test]
fn reused_gate_requires_lock_and_preserves_ignored_artifacts() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let held = f.prepare(&f.first);
    std::fs::create_dir_all(f.repo.join(".git/info")).unwrap();
    std::fs::write(f.repo.join(".git/info/exclude"), "/artifact\n").unwrap();
    std::fs::write(f.wt().join("artifact"), "warm cache").unwrap();
    assert_eq!(git(&f.wt(), &["check-ignore", "artifact"]), "artifact");
    let start = Instant::now();
    assert!(f.assert_error(&f.second).contains("already running"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(git(&f.wt(), &["rev-parse", "HEAD"]), f.first);
    drop(held);
    assert!(gate_tip(&f.repo, &f.second, &f.config).unwrap().passed());
    assert_eq!(
        std::fs::read_to_string(f.wt().join("artifact")).unwrap(),
        "warm cache"
    );
}

#[test]
fn concurrent_distinct_oid_gate_refuses_instead_of_rechecking_out_active_tree() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let ready = f.root.path().join("ready");
    let release = f.root.path().join("release");
    let mut config = f.config.clone();
    config.gate_command = format!(
        "printf ready > {}; i=0; while test ! -e {}; do i=$((i+1)); test $i -lt 200 || exit 125; sleep 0.02; done; test \"$(git rev-parse HEAD)\" = \"$THEGN_GATE_OID\"",
        util::sh_quote(ready.to_str().unwrap()),
        util::sh_quote(release.to_str().unwrap())
    );
    let repo = f.repo.clone();
    let oid = f.first.clone();
    struct ReleaseOnDrop(PathBuf);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            if let Err(error) = std::fs::write(&self.0, "release") {
                std::io::Write::write_fmt(
                    &mut std::io::stderr(),
                    format_args!("private gate rendezvous release failed: {error}\n"),
                )
                .unwrap_or(()); // Best-effort diagnostic during cleanup/unwind.
            }
        }
    }
    let (started, contender, released, original) = std::thread::scope(|scope| {
        let worker = scope.spawn(move || gate_tip(&repo, &oid, &config));
        // This drops before the scope joins on every unwind path. The shell
        // rendezvous also has its own finite bound if filesystem release fails.
        let _release_on_drop = ReleaseOnDrop(release.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = ready.exists();
        let contender = started.then(|| gate_tip(&f.repo, &f.second, &f.config));
        let released = std::fs::write(&release, "release");
        let original = worker.join();
        (started, contender, released, original)
    });
    released.unwrap();
    let original = original.unwrap().unwrap();
    assert!(started, "original private gate never started");
    assert!(original.passed(), "{original:?}");
    assert!(matches!(contender, Some(Ok(GateVerdict::Error { .. }))));
    assert_eq!(git(&f.wt(), &["rev-parse", "HEAD"]), f.first);
}

#[test]
fn unknown_directory_and_foreign_repository_are_never_deleted() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    std::fs::create_dir_all(f.wt()).unwrap();
    std::fs::write(f.wt().join("sentinel"), "foreign").unwrap();
    assert!(f.assert_error(&f.first).contains("identity refused"));
    assert_eq!(
        std::fs::read_to_string(f.wt().join("sentinel")).unwrap(),
        "foreign"
    );
    git(&f.wt(), &["init", "-q"]);
    assert!(f.assert_error(&f.first).contains("identity refused"));
    assert_eq!(
        std::fs::read_to_string(f.wt().join("sentinel")).unwrap(),
        "foreign"
    );
    assert!(f.wt().join(".git").is_dir());
}

#[test]
fn stale_registration_is_not_pruned_or_force_recreated() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    let admin = workspace.checkout.admin.path().to_path_buf();
    drop(workspace);
    let displaced = f.root.path().join("displaced");
    std::fs::rename(f.wt(), &displaced).unwrap();
    assert!(f.assert_error(&f.second).contains("creation failed"));
    assert!(admin.is_dir());
    assert_eq!(
        std::fs::read_to_string(displaced.join("value")).unwrap(),
        "one\n"
    );
    assert!(!f.wt().exists());
}

#[test]
fn checkout_failure_preserves_existing_gate_and_index_lock() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    let lock = workspace.checkout.admin.path().join("index.lock");
    std::fs::write(&lock, "private contention").unwrap();
    drop(workspace);
    assert!(f.assert_error(&f.second).contains("checkout failed"));
    assert_eq!(git(&f.wt(), &["rev-parse", "HEAD"]), f.first);
    assert_eq!(std::fs::read_to_string(lock).unwrap(), "private contention");
    assert_eq!(
        std::fs::read_to_string(f.wt().join("value")).unwrap(),
        "one\n"
    );
}

#[test]
fn changed_backlink_or_common_directory_holds_without_checkout() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    let admin = workspace.checkout.admin.path().to_owned();
    drop(workspace);
    let backlink = std::fs::read(admin.join("gitdir")).unwrap();
    std::fs::write(
        admin.join("gitdir"),
        f.root.path().join("foreign/.git").to_str().unwrap(),
    )
    .unwrap();
    assert!(f.assert_error(&f.second).contains("registration"));
    std::fs::write(admin.join("gitdir"), backlink).unwrap();
    std::fs::write(admin.join("commondir"), f.root.path().to_str().unwrap()).unwrap();
    assert!(f.assert_error(&f.second).contains("common directory"));
    assert_eq!(
        std::fs::read_to_string(f.wt().join("value")).unwrap(),
        "one\n"
    );
}

#[test]
fn setup_or_successful_gate_cannot_change_the_commit_under_test() {
    let mut f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    f.config.gate_setup_command = format!("git checkout --detach --force {}", f.second);
    assert!(f.assert_error(&f.first).contains("HEAD"));
    f.config.gate_setup_command.clear();
    f.config.gate_command = format!("git checkout --detach --force {}", f.second);
    assert!(f.assert_error(&f.first).contains("HEAD"));
}

#[test]
fn replaced_lock_during_success_is_infrastructure_not_green() {
    let mut f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let path = gate_base(&f.repo).join("wt.lock");
    f.config.gate_command = format!(
        "mv {} {}; printf replacement > {}",
        util::sh_quote(path.to_str().unwrap()),
        util::sh_quote(f.root.path().join("old-lock").to_str().unwrap()),
        util::sh_quote(path.to_str().unwrap())
    );
    assert!(f.assert_error(&f.first).contains("identity"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "replacement");
}

#[test]
fn throwaway_has_owned_unique_parent_and_exact_scoped_cleanup() {
    let mut f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    f.config.gate_reuse_worktree = false;
    let workspace = f.prepare(&f.first);
    let parent = workspace.temporary_parent.as_ref().unwrap().clone();
    assert!(parent.is_dir());
    assert_eq!(workspace.checkout.worktree.path(), parent.join("wt"));
    workspace.cleanup().unwrap();
    assert!(!parent.exists());
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), f.first);
}

#[test]
fn gate_identity_error_never_advances_or_blames_candidate() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let held = f.prepare(&f.first);
    let report = run_fold(
        &f.config,
        &f.repo,
        vec![Branch {
            name: "candidate".into(),
            tip: f.second.clone(),
        }],
    )
    .unwrap();
    assert!(!report.advanced);
    assert!(report.landed.is_empty());
    assert!(matches!(
        report.gate,
        crate::integrate::GateOutcome::Errored { .. }
    ));
    assert!(!report.deferred.iter().any(|row| row.gate_failed));
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), f.first);
    drop(held);
}

#[test]
fn symlink_gate_path_is_not_followed_or_removed() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    std::fs::create_dir_all(gate_base(&f.repo)).unwrap();
    let foreign = f.root.path().join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("sentinel"), "untouched").unwrap();
    crate::platform::symlink_file_for_test(&foreign, &f.wt()).unwrap();
    assert!(f.assert_error(&f.first).contains("identity refused"));
    assert_eq!(
        std::fs::read_to_string(foreign.join("sentinel")).unwrap(),
        "untouched"
    );
    assert!(
        std::fs::symlink_metadata(f.wt())
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn root_git_mapping_is_revalidated_before_reporting_success() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    // Adding this mapping would retarget subsequent root Git invocations even
    // though the root/.git directory inode itself did not move.
    std::fs::write(
        f.repo.join(".git/commondir"),
        f.root.path().to_str().unwrap(),
    )
    .unwrap();
    assert!(format!("{:#}", workspace.verify().err().unwrap()).contains("association changed"));
    assert!(f.wt().join("value").exists());
}

#[test]
fn setup_failure_keeps_exit_code_and_does_not_run_gate() {
    let mut f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    f.config.gate_setup_command = "printf setup-marker; exit 42".into();
    f.config.gate_command = "printf gate-marker".into();
    match gate_tip(&f.repo, &f.first, &f.config).unwrap() {
        GateVerdict::Error { reason, log } => {
            assert!(reason.contains("exit 42"));
            assert!(log.contains("setup-marker"));
            assert!(!log.contains("gate-marker"));
        }
        other => panic!("expected setup infrastructure error: {other:?}"),
    }
}

#[test]
fn linked_root_gitfile_reassignment_is_not_a_new_repository() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let linked = f.root.path().join("linked-root");
    git(
        &f.repo,
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            &f.first,
        ],
    );
    let root = Repository::capture(&linked).unwrap();
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", f.repo.join(".git").display()),
    )
    .unwrap();
    assert!(format!("{:#}", root.verify().err().unwrap()).contains("gitfile association changed"));
    assert_eq!(
        std::fs::read_to_string(linked.join("value")).unwrap(),
        "one\n"
    );
}

#[test]
fn supplied_parent_grammar_preserves_only_pinned_ancestor_traversal() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    let admin = workspace.checkout.admin.path();
    assert_eq!(
        read_regular(&admin.join("commondir")).unwrap().trim(),
        "../.."
    );
    assert_eq!(resolve_path(admin, "../..\n").unwrap(), f.repo.join(".git"));
    for value in [
        "alias/../target",
        "../alias/../target",
        "./target",
        "a//b",
        "a/./b",
        "/root/../target",
        "/root/alias/../target",
    ] {
        assert!(resolve_path(admin, value).is_err(), "accepted {value}");
    }
    assert!(gitfile_path(f.wt().as_path(), "gitdir: ../relative\n").is_err());
    workspace.verify().unwrap();
}

#[test]
fn forged_symlink_parent_gitfile_refuses_before_any_checkout_mutation() {
    let f = Fixture::new();
    if !f.supported_or_refused() {
        return;
    }
    let workspace = f.prepare(&f.first);
    let original_admin = workspace.checkout.admin.path().to_owned();
    drop(workspace);
    let foreign_repo = f.root.path().join("foreign-repo");
    git(
        f.root.path(),
        &[
            "clone",
            "--no-local",
            "--no-hardlinks",
            f.repo.to_str().unwrap(),
            foreign_repo.to_str().unwrap(),
        ],
    );
    let foreign_wt = f.root.path().join("foreign-checkout/wt");
    std::fs::create_dir(foreign_wt.parent().unwrap()).unwrap();
    git(
        &foreign_repo,
        &[
            "worktree",
            "add",
            "--detach",
            foreign_wt.to_str().unwrap(),
            &f.first,
        ],
    );
    let foreign_admin = PathBuf::from(git(&foreign_wt, &["rev-parse", "--absolute-git-dir"]));
    assert_eq!(original_admin.file_name(), foreign_admin.file_name());
    let pivot = foreign_admin.parent().unwrap().join("pivot");
    std::fs::create_dir(&pivot).unwrap();
    let alias = original_admin.parent().unwrap().join("alias");
    crate::platform::symlink_file_for_test(&pivot, &alias).unwrap();
    let forged = format!(
        "gitdir: {}/../{}\n",
        alias.display(),
        original_admin.file_name().unwrap().to_str().unwrap()
    );
    std::fs::write(f.wt().join(".git"), &forged).unwrap();
    // This is real Git's interpretation of the original supplied string, not
    // our resolver's normalization. The private foreign admin is selected.
    assert_eq!(
        PathBuf::from(git(&f.wt(), &["rev-parse", "--absolute-git-dir"])),
        foreign_admin
    );
    std::fs::write(foreign_wt.join("sentinel"), "foreign unchanged").unwrap();
    let original_refs = git(&f.repo, &["show-ref"]);
    let foreign_refs = git(&foreign_repo, &["show-ref"]);
    let paths = [
        original_admin.join("HEAD"),
        original_admin.join("index"),
        foreign_admin.join("HEAD"),
        foreign_admin.join("index"),
        f.wt().join("value"),
        foreign_wt.join("value"),
        foreign_wt.join("sentinel"),
    ];
    let before: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    assert!(f.assert_error(&f.second).contains("parent traversal"));
    assert_eq!(git(&f.repo, &["show-ref"]), original_refs);
    assert_eq!(git(&foreign_repo, &["show-ref"]), foreign_refs);
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "mutated {}",
            path.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(f.wt().join(".git")).unwrap(),
        forged
    );
}
