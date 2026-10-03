//! THE-215: sealed tiers get a *confidential* `$HOME` — a private tmpfs holding
//! only a reviewed read-only allowlist — not the ambient `$HOME` mounted
//! read-only (which protects integrity but leaves every credential readable).
//!
//! Everything here uses a **synthetic** `$HOME` in a tempdir with canary
//! sentinels; no real credential file is ever read.

use std::collections::HashMap;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use thegn_core::config::{FileAccess, Network, SandboxProfile};
use thegn_core::placement::Placement;
use thegn_core::sandbox::{Backend, Mount, SandboxLimits, SandboxSpec, enter_argv};
use thegn_core::sandbox_floor::home_gate;
use thegn_core::sandbox_mounts::{HomeView, home_view, planned_home_view, sealed_home_allowlist};

const CANARY: &str = "THE215-CANARY-DO-NOT-LEAK";

/// A synthetic `$HOME`: credential canaries + benign dotfiles.
fn fixture() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let h = td.path();
    for d in [
        ".ssh",
        ".aws",
        ".gnupg",
        ".secrets",
        ".config/gh",
        ".local/state/thegn",
    ] {
        std::fs::create_dir_all(h.join(d)).unwrap();
    }
    for f in [
        ".ssh/id_test",
        ".aws/credentials",
        ".gnupg/key",
        ".secrets/token",
        ".config/gh/hosts.yml",
        ".local/state/thegn/state.db",
    ] {
        std::fs::write(h.join(f), CANARY).unwrap();
    }
    std::fs::write(h.join(".gitconfig"), "[user]\n\tname = Synthetic\n").unwrap();
    std::fs::write(h.join(".zshrc"), "# benign\n").unwrap();
    td
}

fn spec(backend: Backend, home: &Path, mounts: Vec<Mount>, seal: bool) -> SandboxSpec {
    SandboxSpec {
        backend,
        placement: Placement::Local,
        image: None,
        worktree: home.join("wt"),
        mounts,
        env: vec![],
        env_overrides: HashMap::new(),
        env_block: vec![],
        network: Network::None,
        network_allow: vec![],
        network_block: vec![],
        read_only_root: true,
        no_new_privileges: true,
        pids_limit: None,
        drop_capabilities: vec![],
        add_capabilities: vec![],
        file_access: FileAccess::WorktreePlusCaches,
        ports: vec![],
        gpu: None,
        limits: SandboxLimits::default(),
        volumes: vec![],
        compose: None,
        build: None,
        init_script: None,
        devenv: false,
        devenv_path: None,
        name: "thegn-test".into(),
        vpn: None,
        oci_host: None,
        oci_runtime: None,
        daemon_persistent: false,
        seal_home: seal.then(|| home.to_string_lossy().into_owned()),
    }
}

fn rw(p: &Path) -> Mount {
    let s = p.to_string_lossy().into_owned();
    Mount {
        host: s.clone(),
        dest: s,
        ro: false,
        cache: false,
    }
}

fn dests(m: &[Mount]) -> Vec<&str> {
    m.iter().map(|m| m.dest.as_str()).collect()
}

// ── allowlist: reviewed entries only, symlinks cannot redirect into secrets ──

#[test]
fn allowlist_binds_benign_dotfiles_and_nothing_secret() {
    let td = fixture();
    let al = sealed_home_allowlist(td.path());
    let d = dests(&al);
    assert!(d.contains(&td.path().join(".gitconfig").to_str().unwrap()));
    assert!(d.contains(&td.path().join(".zshrc").to_str().unwrap()));
    assert!(al.iter().all(|m| m.ro), "allowlist is read-only");
    for secret in [
        ".ssh",
        ".aws",
        ".gnupg",
        ".secrets",
        ".config/gh",
        ".local/state",
    ] {
        let p = td.path().join(secret);
        assert!(
            !al.iter()
                .any(|m| Path::new(&m.dest).starts_with(&p) || Path::new(&m.host).starts_with(&p)),
            "{secret} leaked into the allowlist: {al:?}"
        );
    }
}

#[test]
fn symlinked_entry_into_a_secret_dir_is_rejected() {
    let td = fixture();
    let h = td.path();
    // ~/.zshenv -> ~/.ssh/id_test, ~/.bashrc -> ~/.aws/credentials
    symlink(h.join(".ssh/id_test"), h.join(".zshenv")).unwrap();
    symlink(h.join(".aws/credentials"), h.join(".bashrc")).unwrap();
    let al = sealed_home_allowlist(h);
    let d = dests(&al);
    assert!(!d.contains(&h.join(".zshenv").to_str().unwrap()));
    assert!(!d.contains(&h.join(".bashrc").to_str().unwrap()));
}

#[test]
fn symlinked_dir_that_is_an_ancestor_of_a_secret_is_rejected() {
    let td = fixture();
    let h = td.path();
    // ~/.config/git -> ~/.config (which contains ~/.config/gh)
    if let Err(e) = std::fs::remove_dir_all(h.join(".config/git")) {
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
    }
    symlink(h.join(".config"), h.join(".config/git")).unwrap();
    let al = sealed_home_allowlist(h);
    assert!(
        !dests(&al).contains(&h.join(".config/git").to_str().unwrap()),
        "{al:?}"
    );
}

#[test]
fn symlink_outside_approved_roots_is_rejected_but_home_dotfile_repo_is_not() {
    let td = fixture();
    let h = td.path();
    let elsewhere = tempfile::tempdir().unwrap(); // not $HOME, not /nix/store
    std::fs::write(elsewhere.path().join("rc"), CANARY).unwrap();
    symlink(elsewhere.path().join("rc"), h.join(".zprofile")).unwrap();
    // A stow-style dotfiles repo inside $HOME is fine; mounted from its target.
    std::fs::create_dir_all(h.join("dotfiles")).unwrap();
    std::fs::write(h.join("dotfiles/inputrc"), "set bell-style none\n").unwrap();
    symlink(h.join("dotfiles/inputrc"), h.join(".inputrc")).unwrap();
    let al = sealed_home_allowlist(h);
    assert!(!dests(&al).contains(&h.join(".zprofile").to_str().unwrap()));
    let inputrc = al
        .iter()
        .find(|m| m.dest == h.join(".inputrc").to_str().unwrap())
        .expect("in-home dotfile target is allowed");
    assert!(inputrc.host.ends_with("dotfiles/inputrc"));
}

// ── backend argv ─────────────────────────────────────────────────────────────

#[test]
fn bwrap_sealed_hides_home_behind_a_tmpfs_before_any_bind() {
    let td = fixture();
    let h = td.path();
    let mut mounts = sealed_home_allowlist(h);
    mounts.push(rw(&h.join("wt")));
    let argv = enter_argv(&spec(Backend::Bwrap, h, mounts, true), "true").unwrap();
    let home = h.to_str().unwrap();
    let tmpfs = argv
        .windows(2)
        .position(|w| w[0] == "--tmpfs" && w[1] == home)
        .expect("sealed bwrap must mount a private tmpfs $HOME");
    let first_home_bind = argv
        .iter()
        .enumerate()
        .position(|(i, a)| {
            i > 0 && matches!(argv[i - 1].as_str(), "--ro-bind" | "--bind") && a.starts_with(home)
        })
        .expect("allowlist/worktree binds");
    assert!(tmpfs < first_home_bind, "tmpfs must precede home binds");
    let j = argv.join(" ");
    assert!(!j.contains(&format!("--ro-bind {home} {home}")), "{j}");
    assert!(!j.contains(&format!("--bind {home} {home}")), "{j}");
    assert!(!j.contains(".ssh") && !j.contains(".aws"), "{j}");
}

#[test]
fn bwrap_hardened_is_unchanged_no_tmpfs_home() {
    let td = fixture();
    let h = td.path();
    let mut ro_home = rw(h);
    ro_home.ro = true;
    let argv = enter_argv(
        &spec(Backend::Bwrap, h, vec![ro_home, rw(&h.join("wt"))], false),
        "true",
    )
    .unwrap();
    let j = argv.join(" ");
    assert!(!j.contains(&format!("--tmpfs {}", h.display())), "{j}");
    assert!(
        j.contains(&format!("--ro-bind {0} {0}", h.display())),
        "{j}"
    );
}

#[test]
fn systemd_sealed_with_a_home_outside_protecthome_roots_is_refused() {
    // Argv shape is covered in-crate (`sandbox_tests`, needs a /home path); here
    // the synthetic tempdir HOME is outside /home, /root and /run/user, where
    // `ProtectHome=tmpfs` does not reach: enter_argv must refuse.
    let td = fixture();
    let h = td.path();
    let mut mounts = sealed_home_allowlist(h);
    mounts.push(rw(&h.join("wt")));
    let err = enter_argv(&spec(Backend::Systemd, h, mounts, true), "true").unwrap_err();
    assert!(err.to_string().contains("sealed"), "{err}");
}

#[test]
fn systemd_hardened_keeps_readonly_home() {
    let td = fixture();
    let j = enter_argv(&spec(Backend::Systemd, td.path(), vec![], false), "true")
        .unwrap()
        .join(" ");
    assert!(j.contains("ProtectHome=read-only"), "{j}");
}

// ── HomeView / floor gate ────────────────────────────────────────────────────

#[test]
fn home_view_and_gate_per_backend() {
    let td = fixture();
    let h = td.path();
    let mut mounts = sealed_home_allowlist(h);
    mounts.push(rw(&h.join("wt")));

    // Hidden: bwrap, OCI (no mount covers $HOME). systemd only hides a `$HOME`
    // under /home, /root or /run/user (see the dedicated test below); this
    // tempdir home is outside them, so it fails closed.
    let sd = spec(Backend::Systemd, h, mounts.clone(), true);
    assert_eq!(home_view(&sd, h), HomeView::ReadOnly);
    assert!(home_gate(&sd).is_some());
    for b in [Backend::Bwrap, Backend::Podman, Backend::Docker] {
        let s = spec(b, h, mounts.clone(), true);
        assert_eq!(home_view(&s, h), HomeView::Hidden, "{b:?}");
        assert_eq!(home_gate(&s), None, "{b:?}");
    }
    // Apple container: no host mounts at all => hidden by construction.
    let apple = spec(Backend::Apple, h, vec![], true);
    assert_eq!(home_gate(&apple), None);

    // Backends with no filesystem boundary cannot hide $HOME: refused.
    for b in [
        Backend::None,
        Backend::WinJobObject,
        Backend::WinAppContainer,
    ] {
        let s = spec(b, h, vec![], true);
        let msg = home_gate(&s).unwrap_or_else(|| panic!("{b:?} must fail the home gate"));
        assert!(msg.contains("writable"), "{msg}");
    }
    // `file_access = all` dev-binds `/`: refused rather than silently exposed.
    let mut all = spec(Backend::Bwrap, h, mounts.clone(), true);
    all.file_access = FileAccess::All;
    assert!(home_gate(&all).is_some());
    // A `[sandbox] mounts` entry that covers $HOME re-exposes it: refused.
    let mut ro_home = rw(h);
    ro_home.ro = true;
    let mut exposed = mounts.clone();
    exposed.insert(0, ro_home);
    let s = spec(Backend::Podman, h, exposed, true);
    assert_eq!(home_view(&s, h), HomeView::ReadOnly);
    assert!(home_gate(&s).is_some(), "a mount covering $HOME is refused");
    // Non-sealed profiles are not gated.
    assert_eq!(home_gate(&spec(Backend::None, h, vec![], false)), None);
}

#[test]
fn planned_view_separates_confidentiality_from_integrity() {
    use SandboxProfile::*;
    assert_eq!(planned_home_view(Sealed, Backend::Bwrap), HomeView::Hidden);
    assert_eq!(
        planned_home_view(SealedTunnel, Backend::Podman),
        HomeView::Hidden
    );
    assert_eq!(
        planned_home_view(Sealed, Backend::Systemd),
        HomeView::Hidden
    );
    assert_eq!(
        planned_home_view(Hardened, Backend::Bwrap),
        HomeView::ReadOnly
    );
    assert_eq!(planned_home_view(Open, Backend::Bwrap), HomeView::Writable);
    assert_eq!(planned_home_view(Sealed, Backend::None), HomeView::Writable);
}

// ── real bwrap against the synthetic HOME ────────────────────────────────────

/// Run `script` inside a real sealed bwrap over the fixture, returning
/// (success, stdout). `None` when bwrap/userns is unavailable here.
fn run_sealed(h: &Path, script: &str) -> Option<(bool, String)> {
    if !thegn_core::util::have("bwrap") {
        return None;
    }
    let mut mounts = sealed_home_allowlist(h);
    mounts.push(rw(&h.join("wt")));
    // bwrap hardcodes the system substrate (/nix/store, /usr, /etc, ...).
    let s = spec(Backend::Bwrap, h, mounts, true);
    let argv = enter_argv(&s, script).unwrap();
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env("HOME", h)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    // userns denied (CI container) => not a product failure; skip.
    if !out.status.success()
        && String::from_utf8_lossy(&out.stderr).contains("Operation not permitted")
    {
        return None;
    }
    Some((out.status.success(), stdout))
}

#[test]
fn real_bwrap_sealed_cannot_read_canaries_but_worktree_and_git_work() {
    let td = fixture();
    let h: PathBuf = td.path().to_path_buf();
    std::fs::create_dir_all(h.join("wt")).unwrap();
    // Attempt every canary, copy attempts into the writable worktree, and prove
    // the curated substrate (allowlisted gitconfig, writable worktree, git) works.
    let script = format!(
        "for f in .ssh/id_test .aws/credentials .gnupg/key .secrets/token \
         .config/gh/hosts.yml .local/state/thegn/state.db; do \
           cat {h}/$f 2>/dev/null; cp {h}/$f {h}/wt/stolen 2>/dev/null; \
         done; ls -A {h} 2>/dev/null; echo ---; cat {h}/.gitconfig; \
         echo ok > {h}/wt/out && cat {h}/wt/out; \
         git -C {h}/wt init -q && git -C {h}/wt -c user.name=t -c user.email=t@t \
           -c commit.gpgsign=false commit -q --allow-empty -m t && echo gitok",
        h = h.display()
    );
    let Some((_, out)) = run_sealed(&h, &script) else {
        // skipped: no usable bwrap here
        return;
    };
    assert!(!out.contains(CANARY), "canary leaked: {out}");
    assert!(!h.join("wt/stolen").exists(), "canary copied into worktree");
    let (listing, rest) = out
        .split_once("---")
        .unwrap_or_else(|| panic!("script ran: {out}"));
    for hidden in [".ssh", ".aws", ".gnupg", ".secrets", ".local"] {
        assert!(
            !listing.lines().any(|l| l == hidden),
            "{hidden} visible: {listing}"
        );
    }
    assert!(
        rest.contains("Synthetic"),
        "allowlisted .gitconfig readable: {rest}"
    );
    assert!(
        rest.contains("ok") && rest.contains("gitok"),
        "worktree+git: {rest}"
    );
}

// ── review fixes: mounts deny filter, doctor listing, gate edge cases ────────

#[test]
fn default_config_mounts_never_bring_gnupg_into_a_sealed_argv() {
    if !thegn_core::util::have("bwrap") {
        // skipped: no usable bwrap here
        return;
    }
    let td = fixture();
    let h = td.path().canonicalize().unwrap();
    std::fs::create_dir_all(h.join("wt")).unwrap();
    // SAFETY: nextest runs each test in its own process; nothing else reads HOME.
    unsafe { std::env::set_var("HOME", &h) };
    let mut cfg = thegn_core::config::SandboxConfig {
        enabled: true,
        backend: thegn_core::config::SandboxBackend::Bwrap,
        ..Default::default()
    };
    assert!(
        cfg.mounts.iter().any(|m| m.contains(".gnupg")),
        "default mounts should still carry ~/.gnupg (the case under test)"
    );
    cfg.mounts.push("~/.ssh:ro".into());
    cfg.mounts
        .push(format!("{}:/ssh-alias:ro", h.join(".ssh").display()));
    let loc = thegn_core::remote::GitLoc::Local(h.join("wt"));
    let Some(spec) = thegn_core::sandbox::resolve_placed(
        &cfg,
        &loc,
        "t",
        SandboxProfile::Sealed,
        Placement::Local,
    ) else {
        // skipped: no usable bwrap here
        return;
    };
    assert_eq!(spec.seal_home.as_deref(), h.to_str());
    let joined = enter_argv(&spec, "true").unwrap().join(" ");
    for secret in [".gnupg", ".ssh", "ssh-alias"] {
        assert!(
            !joined.contains(secret),
            "{secret} in sealed argv: {joined}"
        );
    }
    assert!(
        spec.mounts.iter().all(|m| !m.host.contains(".gnupg")),
        "{:?}",
        spec.mounts
    );
    // Doctor lists what was dropped.
    let dropped = thegn_core::sandbox_mounts::sealed_dropped_cfg_mounts(&cfg.mounts, &h);
    assert!(dropped.iter().any(|p| p.ends_with(".gnupg")), "{dropped:?}");
    assert!(dropped.iter().any(|p| p.ends_with(".ssh")), "{dropped:?}");
}

#[test]
fn mount_deny_follows_symlinks_on_host_and_dest() {
    use thegn_core::sandbox_mounts::sealed_denies_mount;
    let td = fixture();
    let h = td.path();
    symlink(h.join(".gnupg"), h.join("innocent")).unwrap();
    let m = |host: &Path, dest: &str| Mount {
        host: host.to_string_lossy().into_owned(),
        dest: dest.into(),
        ro: false,
        cache: false,
    };
    assert!(sealed_denies_mount(&m(&h.join("innocent"), "/x"), h));
    assert!(sealed_denies_mount(
        &m(&h.join("wt"), &h.join(".ssh/new").to_string_lossy()),
        h
    ));
    assert!(
        sealed_denies_mount(&m(h, &h.to_string_lossy()), h),
        "$HOME is an ancestor"
    );
    assert!(!sealed_denies_mount(
        &m(
            &h.join(".gitconfig"),
            &h.join(".gitconfig").to_string_lossy()
        ),
        h
    ));
}

#[test]
fn allowlist_never_binds_git_or_zsh_dirs_wholesale() {
    let td = fixture();
    let h = td.path();
    for d in [".config/git", ".config/zsh"] {
        std::fs::create_dir_all(h.join(d)).unwrap();
    }
    std::fs::write(h.join(".config/git/config"), "[user]\n").unwrap();
    std::fs::write(h.join(".config/git/credentials"), CANARY).unwrap();
    std::fs::write(h.join(".config/zsh/.zshrc"), "#\n").unwrap();
    std::fs::write(h.join(".config/zsh/.zsh_history"), CANARY).unwrap();
    let al = sealed_home_allowlist(h);
    let d = dests(&al);
    assert!(d.contains(&h.join(".config/git/config").to_str().unwrap()));
    assert!(d.contains(&h.join(".config/zsh/.zshrc").to_str().unwrap()));
    for m in &al {
        for bad in [".config/git", ".config/zsh"] {
            assert_ne!(Path::new(&m.dest), h.join(bad), "{bad} bound wholesale");
        }
        assert!(!m.dest.contains("credentials") && !m.dest.contains("zsh_history"));
    }
}

#[test]
fn systemd_home_view_sees_covering_mounts_and_unprotected_homes() {
    let td = fixture();
    let h = td.path(); // /tmp/..: NOT under /home, /root or /run/user
    let s = spec(Backend::Systemd, h, vec![], true);
    assert_ne!(
        home_view(&s, h),
        HomeView::Hidden,
        "ProtectHome=tmpfs misses this $HOME"
    );
    assert!(home_gate(&s).is_some());
    let mut ro_home = rw(h);
    ro_home.ro = true;
    let hp = Path::new("/home/synthetic-user");
    let mut s2 = spec(Backend::Systemd, hp, vec![ro_home.clone()], true);
    s2.seal_home = Some(hp.to_string_lossy().into_owned());
    let covering = Mount {
        host: "/home".into(),
        dest: "/home".into(),
        ro: true,
        cache: false,
    };
    s2.mounts = vec![covering];
    assert_eq!(home_view(&s2, hp), HomeView::ReadOnly);
    s2.mounts = vec![];
    assert_eq!(
        home_view(&s2, hp),
        HomeView::Hidden,
        "/home is covered by ProtectHome=tmpfs"
    );
}

#[test]
fn systemd_sealed_refuses_whitespace_or_colon_in_bind_paths() {
    let td = fixture();
    let h = td.path();
    for bad in [
        "with space",
        "with:colon",
        "with\"quote",
        "with'quote",
        "with\\slash",
    ] {
        let p = h.join(bad);
        let s = spec(Backend::Systemd, h, vec![rw(&p)], true);
        let msg = home_gate(&s).unwrap_or_else(|| panic!("{bad} must be refused"));
        assert!(msg.contains("systemd"), "{msg}");
    }
}

#[test]
fn unusable_home_and_wsl_fail_the_sealed_gate() {
    let td = fixture();
    let h = td.path();
    let mut s = spec(Backend::Bwrap, h, vec![], true);
    s.seal_home = Some(String::new());
    assert!(home_gate(&s).unwrap().contains("resolvable"));
    s.seal_home = Some("relative/home".into());
    assert!(home_gate(&s).is_some());
    s.seal_home = Some("/nonexistent/synthetic-home".into());
    assert!(home_gate(&s).is_some());
    let wsl = spec(Backend::Wsl, h, vec![], true);
    assert!(home_gate(&wsl).is_some(), "WSL cannot hide $HOME");
    assert_eq!(
        planned_home_view(SandboxProfile::Sealed, Backend::Wsl),
        HomeView::ReadOnly
    );
}

#[test]
fn bwrap_omits_an_allowlist_mount_retargeted_into_a_secret_after_resolution() {
    let td = fixture();
    let h = td.path();
    let rc = h.join(".zshrc");
    let mut mounts = sealed_home_allowlist(h);
    mounts.push(rw(&h.join("wt")));
    // Retarget the symlinked dotfile AFTER the allowlist was resolved.
    std::fs::remove_file(&rc).unwrap();
    symlink(h.join(".ssh/id_test"), &rc).unwrap();
    let j = enter_argv(&spec(Backend::Bwrap, h, mounts, true), "true")
        .unwrap()
        .join(" ");
    assert!(!j.contains(".ssh"), "{j}");
}

// ── round 3: the chokepoint, B2, and the minor fixes ─────────────────────────

fn bwrap_argv(h: &Path, mounts: Vec<Mount>) -> Result<String, thegn_core::sandbox::EnterError> {
    enter_argv(&spec(Backend::Bwrap, h, mounts, true), "true").map(|a| a.join(" "))
}

#[test]
fn identity_and_agent_mounts_added_after_resolution_never_reach_a_sealed_argv() {
    let td = fixture();
    let h = td.path();
    std::fs::create_dir_all(h.join("wt")).unwrap();
    let mut mounts = sealed_home_allowlist(h);
    mounts.push(rw(&h.join("wt")));
    // What `bundle::fold_identity`, `provider_home_mounts` and the credential
    // mounts append AFTER the resolver: ssh key, gh dir, gnupg home, ambient
    // agent homes.
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    std::fs::create_dir_all(h.join(".codex")).unwrap();
    for p in [".ssh/id_test", ".config/gh", ".gnupg", ".claude", ".codex"] {
        mounts.push(rw(&h.join(p)));
    }
    let j = bwrap_argv(h, mounts).unwrap();
    for secret in [".ssh", ".config/gh", ".gnupg", ".claude", ".codex"] {
        assert!(!j.contains(secret), "{secret} reached the sealed argv: {j}");
    }
    assert!(
        j.contains(&format!("{}/wt", h.display())),
        "worktree kept: {j}"
    );
}

#[test]
fn only_the_managed_account_dir_is_exempt_from_the_deny_list() {
    let td = fixture();
    let h = td.path().canonicalize().unwrap();
    let state = h.join("xdg-state");
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("XDG_STATE_HOME", &state) };
    let managed = state.join("thegn/accounts/claude/work");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::create_dir_all(state.join("thegn/other")).unwrap();
    std::fs::create_dir_all(h.join(".claude")).unwrap();
    let mounts = vec![
        rw(&managed),
        rw(&state.join("thegn/other")),
        rw(&h.join(".claude")),
    ];
    let j = bwrap_argv(&h, mounts).unwrap();
    assert!(
        j.contains(managed.to_str().unwrap()),
        "managed dir kept: {j}"
    );
    assert!(!j.contains("thegn/other"), "{j}");
    assert!(!j.contains(&format!("{}/.claude", h.display())), "{j}");
}

#[test]
fn a_worktree_that_is_home_itself_or_covers_secrets_is_refused() {
    let td = fixture();
    let h = td.path();
    let err = bwrap_argv(h, vec![rw(h)]).unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
    let err = bwrap_argv(h, vec![rw(&h.join(".config"))]).unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
}

#[test]
fn oci_mount_paths_with_unsafe_syntax_are_refused() {
    let td = fixture();
    let h = td.path();
    for bad in ["a:b", "a b", "a\"b", "a'b", "a\\b"] {
        let mut s = spec(Backend::Podman, h, vec![rw(&h.join(bad))], false);
        s.seal_home = None;
        let err = enter_argv(&s, "true").unwrap_err();
        assert!(err.to_string().contains("-v"), "{bad}: {err}");
    }
}

#[test]
fn allowlist_binds_files_only_and_new_credential_names_are_denied() {
    let td = fixture();
    let h = td.path();
    std::fs::create_dir_all(h.join(".inputrc")).unwrap(); // a directory, not a file
    std::fs::create_dir_all(h.join(".terminfo")).unwrap();
    std::fs::write(h.join(".npmrc"), CANARY).unwrap();
    std::fs::create_dir_all(h.join(".cargo")).unwrap();
    std::fs::write(h.join(".cargo/credentials.toml"), CANARY).unwrap();
    symlink(h.join(".npmrc"), h.join(".zprofile")).unwrap();
    symlink(h.join(".cargo/credentials.toml"), h.join(".zlogin")).unwrap();
    let al = sealed_home_allowlist(h);
    let d = dests(&al);
    for not in [".inputrc", ".terminfo", ".zprofile", ".zlogin"] {
        assert!(!d.contains(&h.join(not).to_str().unwrap()), "{not}: {d:?}");
    }
    assert!(al.iter().all(|m| Path::new(&m.host).is_file()), "{al:?}");
}

#[test]
fn systemd_sealed_without_read_only_root_fails_closed() {
    let td = fixture();
    let h = Path::new("/home/synthetic-user");
    let mut s = spec(Backend::Systemd, td.path(), vec![], true);
    s.seal_home = Some(h.to_string_lossy().into_owned());
    s.read_only_root = false;
    assert_eq!(home_view(&s, h), HomeView::Writable);
}

#[test]
fn sealed_systemd_allowlist_survives_auto_caches_off() {
    if !thegn_core::util::have("systemd-run") {
        return;
    }
    let td = fixture();
    let h = td.path().canonicalize().unwrap();
    std::fs::create_dir_all(h.join("wt")).unwrap();
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::set_var("HOME", &h) };
    let cfg = thegn_core::config::SandboxConfig {
        enabled: true,
        backend: thegn_core::config::SandboxBackend::Systemd,
        auto_caches: false,
        ..Default::default()
    };
    let loc = thegn_core::remote::GitLoc::Local(h.join("wt"));
    let Some(spec) = thegn_core::sandbox::resolve_placed(
        &cfg,
        &loc,
        "t",
        SandboxProfile::Sealed,
        Placement::Local,
    ) else {
        return;
    };
    assert!(
        spec.mounts
            .iter()
            .any(|m| m.dest == h.join(".zshrc").to_str().unwrap()),
        "{:?}",
        spec.mounts
    );
}

#[test]
fn sealed_launch_refuses_compose_backed_specs() {
    let td = fixture();
    let h = td.path();
    let mut s = spec(Backend::Podman, h, vec![], true);
    s.compose = Some("docker-compose.yml".to_string());
    let err = enter_argv(&s, "true").unwrap_err();
    assert!(err.to_string().contains("compose volumes"), "{err}");
    // Non-sealed compose is unaffected by this gate.
    s.seal_home = None;
    assert!(enter_argv(&s, "true").is_ok());
}

#[test]
fn a_legitimate_user_ro_mount_under_home_survives_the_sealed_filter() {
    let td = fixture();
    let h = td.path();
    let data = tempfile::tempdir().unwrap(); // outside $HOME and the approved roots
    let mut m = rw(&h.join("data"));
    m.host = data.path().to_string_lossy().into_owned();
    m.ro = true;
    let j = bwrap_argv(h, vec![m]).unwrap();
    assert!(j.contains(data.path().to_str().unwrap()), "{j}");
}
