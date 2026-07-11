//! SSH credential plumbing for sandboxes.
//!
//! Two problems keep ssh-based git from working inside a sandbox on
//! NixOS/home-manager hosts, and both are solved here on the (unsandboxed)
//! host side:
//!
//! 1. **Config ownership.** ssh refuses a config file — or any file it
//!    `Include`s — unless it is owned by the invoking user or root. Under
//!    unprivileged bwrap the whole nix store (where home-manager keeps
//!    `~/.ssh/config` and its includes) is owned by `nobody` (the
//!    user-namespace overflow uid), so ssh rejects it with "Bad owner or
//!    permissions". [`prepare_ssh_config`] flattens the resolved config —
//!    inlining every include, even agenix-backed ones under `/run/agenix` —
//!    into a single user-owned `0600` file under the state dir; the caller
//!    points sandboxed git at it via `GIT_SSH_COMMAND='ssh -F <file>'`. (We
//!    cannot bind it over `~/.ssh/config`: when `$HOME` is rw-bound and that
//!    path is a symlink, bwrap dereferences the symlink onto the read-only
//!    store and fails with "Can't create file".)
//!
//! 2. **Dangling identity keys.** Secret managers (agenix, sops-nix) keep
//!    private keys on a tmpfs outside `$HOME` — e.g. `~/.ssh/id_rsa` is a
//!    symlink to `/run/agenix/<name>` — and we deliberately do NOT bind those
//!    secret trees wholesale into the sandbox. [`identity_mounts`] enumerates
//!    the identity files a sandboxed ssh would actually use, resolves each
//!    symlink chain, and returns a read-only [`Mount`] for just the referenced
//!    key files at their symlink-target paths, so the `$HOME`-mounted symlinks
//!    resolve inside the sandbox without exposing any other secret.

use crate::sandbox::Mount;
use std::path::{Path, PathBuf};

/// Materialize a flattened, user-owned copy of `~/.ssh/config` under the
/// state dir. Returns its path (which is also its in-sandbox path, since it
/// lives under the rw-bound `$HOME`), or `None` if there is no usable
/// `~/.ssh/config`.
pub fn prepare_ssh_config() -> Option<String> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    let state = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&home).join(".local/state"));
    prepare_ssh_config_at(Path::new(&home), &state)
}

/// Testable core of [`prepare_ssh_config`]: explicit home and state dirs.
pub fn prepare_ssh_config_at(home: &Path, state: &Path) -> Option<String> {
    let flattened = flattened_config(home)?;
    let dir = state.join("thegn/sandbox");
    std::fs::create_dir_all(&dir).ok()?;
    let out = dir.join("ssh_config");
    std::fs::write(&out, flattened).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600));
    }
    Some(out.to_string_lossy().into_owned())
}

/// Read `<home>/.ssh/config` and inline its `Include` directives.
fn flattened_config(home: &Path) -> Option<String> {
    let ssh_dir = home.join(".ssh");
    let content = std::fs::read_to_string(ssh_dir.join("config")).ok()?;
    let home_s = home.to_string_lossy().into_owned();
    Some(flatten_ssh_config(
        &content,
        &|tok| read_ssh_include(&ssh_dir, &home_s, tok),
        0,
    ))
}

/// Read-only mounts for identity keys whose symlink chains leave the paths
/// already reachable inside the sandbox (`covered_dests`: every mount dest
/// plus any backend-hardcoded binds; `"/"` covers everything).
pub fn identity_mounts(covered_dests: &[String]) -> Vec<Mount> {
    match std::env::var("HOME").ok().filter(|h| !h.is_empty()) {
        Some(home) => identity_mounts_at(Path::new(&home), covered_dests),
        None => Vec::new(),
    }
}

/// Testable core of [`identity_mounts`]: explicit home dir.
pub fn identity_mounts_at(home: &Path, covered: &[String]) -> Vec<Mount> {
    let flattened = flattened_config(home);
    let mut out: Vec<Mount> = Vec::new();
    for cand in identity_candidates(home, flattened.as_deref()) {
        if let Some((dest, host)) = out_of_tree_target(&cand, covered)
            && !out.iter().any(|m| m.dest == dest)
        {
            out.push(Mount {
                host,
                dest,
                ro: true,
                cache: false,
            });
        }
    }
    out
}

/// The identity files a sandboxed ssh would use: every `~/.ssh/id_*` entry
/// (default key names, including `.pub` companions) plus any
/// `IdentityFile`/`CertificateFile` paths named in the flattened config.
fn identity_candidates(home: &Path, flattened: Option<&str>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(home.join(".ssh")) {
        let mut names: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("id_"))
            .map(|e| e.path())
            .collect();
        names.sort();
        v.extend(names);
    }
    if let Some(cfg) = flattened {
        for p in config_identity_paths(cfg, home) {
            if !v.contains(&p) {
                v.push(p);
            }
        }
    }
    v
}

/// `IdentityFile`/`CertificateFile` values from a flattened ssh config.
/// Handles `~/` and absolute paths and the `%d` (home) token; values using
/// other `%` tokens or relative paths are skipped (ssh resolves those at
/// connect time and we cannot know the expansion here).
fn config_identity_paths(flattened: &str, home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in flattened.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        let mut parts = t.splitn(2, [' ', '\t', '=']);
        let (Some(key), Some(val)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !key.eq_ignore_ascii_case("identityfile") && !key.eq_ignore_ascii_case("certificatefile")
        {
            continue;
        }
        let val = val.trim().trim_matches('"');
        let path = if let Some(rest) = val.strip_prefix("~/") {
            home.join(rest)
        } else if let Some(rest) = val.strip_prefix("%d/") {
            home.join(rest)
        } else if val.starts_with('/') {
            PathBuf::from(val)
        } else {
            continue;
        };
        if path.to_string_lossy().contains('%') {
            continue;
        }
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// Walk the symlink chain from `link`. Hops whose targets stay inside a
/// `covered` prefix keep walking (they will resolve inside the sandbox); the
/// first hop that leaves every covered prefix yields
/// `(dest = that as-referenced path, host = canonicalized final target)`.
/// Returns `None` for non-symlinks, dangling or cyclic chains, chains that
/// resolve entirely inside covered prefixes, or final targets that are not
/// regular files.
fn out_of_tree_target(link: &Path, covered: &[String]) -> Option<(String, String)> {
    let mut cur = link.to_path_buf();
    for _ in 0..16 {
        // Not a symlink: a regular covered file (or a dangling path) — either
        // way there is nothing to mount.
        let target = std::fs::read_link(&cur).ok()?;
        let abs = if target.is_absolute() {
            normalize(&target)
        } else {
            normalize(&cur.parent()?.join(target))
        };
        if is_covered(&abs, covered) {
            cur = abs;
            continue;
        }
        let host = std::fs::canonicalize(&abs).ok()?;
        if !host.is_file() {
            return None;
        }
        return Some((
            abs.to_string_lossy().into_owned(),
            host.to_string_lossy().into_owned(),
        ));
    }
    None // hop cap: symlink cycle
}

fn is_covered(path: &Path, covered: &[String]) -> bool {
    covered.iter().any(|c| path.starts_with(c))
}

/// Lexical normalization (no filesystem access): resolves `.` and `..`
/// components so covered-prefix checks work on relative symlink targets.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// True when `path` is a connectable unix socket. Used to drop a dead
/// `SSH_AUTH_SOCK` from the sandbox env instead of forwarding a socket path
/// every ssh connection would fail to reach (`AddKeysToAgent` noise).
#[cfg(unix)]
pub fn unix_socket_alive(path: &str) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(not(unix))]
pub fn unix_socket_alive(_path: &str) -> bool {
    true // no cheap probe off-unix; keep the passthrough
}

/// Read the contents of every file an ssh `Include` token expands to (host
/// side), honoring `~`, absolute paths, paths relative to `~/.ssh`, and simple
/// `*`/`?` globs.
fn read_ssh_include(ssh_dir: &Path, home: &str, token: &str) -> Vec<String> {
    let path = if let Some(rest) = token.strip_prefix("~/") {
        Path::new(home).join(rest)
    } else if token.starts_with('/') {
        PathBuf::from(token)
    } else {
        ssh_dir.join(token)
    };
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    if name.contains('*') || name.contains('?') {
        let parent = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| ssh_dir.to_path_buf());
        let mut paths: Vec<PathBuf> = std::fs::read_dir(&parent)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| wildcard_match(&name, &e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
        paths.sort();
        paths
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect()
    } else {
        std::fs::read_to_string(&path).ok().into_iter().collect()
    }
}

/// Inline ssh `Include` directives so the output is a single self-contained
/// config. `read` returns the contents of every file an include token expands
/// to. Depth-guarded against include cycles.
fn flatten_ssh_config(content: &str, read: &dyn Fn(&str) -> Vec<String>, depth: u8) -> String {
    let mut out = String::new();
    for line in content.lines() {
        let t = line.trim_start();
        let is_include = t
            .get(..7)
            .is_some_and(|h| h.eq_ignore_ascii_case("include"))
            && t[7..].starts_with(char::is_whitespace);
        if is_include && depth < 16 {
            let args = t[7..].trim();
            out.push_str(&format!("# thegn: inlined `Include {args}`\n"));
            for token in args.split_whitespace() {
                for body in read(token) {
                    out.push_str(&flatten_ssh_config(&body, read, depth + 1));
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Minimal shell-style glob matcher supporting `*` and `?` (no char classes).
fn wildcard_match(pat: &str, name: &str) -> bool {
    fn helper(p: &[u8], n: &[u8]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some(b'*') => helper(&p[1..], n) || (!n.is_empty() && helper(p, &n[1..])),
            Some(b'?') => !n.is_empty() && helper(&p[1..], &n[1..]),
            Some(&c) => n.first() == Some(&c) && helper(&p[1..], &n[1..]),
        }
    }
    helper(pat.as_bytes(), name.as_bytes())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// Fresh scratch dir per test (house pattern: no tempfile dev-dep).
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sz-sshcreds-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn flatten_ssh_inlines_includes_and_guards() {
        let read = |tok: &str| -> Vec<String> {
            match tok {
                "inc" => vec!["Host foo\n  User bar\n".to_string()],
                "multi" => vec!["A\n".to_string(), "B\n".to_string()],
                _ => vec![],
            }
        };
        let out = flatten_ssh_config("Host *\n  AddKeysToAgent yes\n  Include inc\n", &read, 0);
        assert!(out.contains("AddKeysToAgent yes"));
        assert!(out.contains("Host foo") && out.contains("User bar"));
        // No live (non-comment) Include directive should remain.
        assert!(!out.lines().any(|l| {
            let t = l.trim_start();
            !t.starts_with('#')
                && t.get(..8)
                    .is_some_and(|h| h.eq_ignore_ascii_case("include "))
        }));
        // Multiple expansions of one token are concatenated.
        let multi = flatten_ssh_config("Include multi\n", &read, 0);
        assert!(multi.contains("A") && multi.contains("B"));
        // Unknown include leaves only the marker comment (no panic).
        assert!(flatten_ssh_config("Include missing\n", &read, 0).contains("inlined"));
    }

    #[test]
    fn flatten_ssh_ignores_include_substrings() {
        let read = |_: &str| Vec::new();
        let out = flatten_ssh_config("  IncludeFoo bar\n  Includes x\n", &read, 0);
        assert!(out.contains("IncludeFoo bar"));
        assert!(out.contains("Includes x"));
    }

    #[test]
    fn wildcard_match_handles_star_and_question() {
        assert!(wildcard_match("*.conf", "a.conf"));
        assert!(wildcard_match("h?st", "host"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("*.conf", "a.txt"));
        assert!(!wildcard_match("h?st", "ht"));
    }

    #[test]
    fn read_ssh_include_expands_tilde_relative_and_globs() {
        let root = scratch("include");
        let home = root.join("home");
        let ssh = home.join(".ssh");
        fs::create_dir_all(&ssh).unwrap();
        fs::write(ssh.join("rel"), "REL\n").unwrap();
        fs::write(home.join("tilde"), "TILDE\n").unwrap();
        fs::write(ssh.join("a.conf"), "GA\n").unwrap();
        fs::write(ssh.join("b.conf"), "GB\n").unwrap();
        let home_s = s(&home);
        assert_eq!(read_ssh_include(&ssh, &home_s, "rel"), vec!["REL\n"]);
        assert_eq!(read_ssh_include(&ssh, &home_s, "~/tilde"), vec!["TILDE\n"]);
        assert_eq!(
            read_ssh_include(&ssh, &home_s, &s(&ssh.join("rel"))),
            vec!["REL\n"]
        );
        assert_eq!(
            read_ssh_include(&ssh, &home_s, "*.conf"),
            vec!["GA\n", "GB\n"]
        );
        assert!(read_ssh_include(&ssh, &home_s, "missing").is_empty());
    }

    #[test]
    fn prepare_ssh_config_writes_flattened_0600() {
        let root = scratch("prepare");
        let home = root.join("home");
        let ssh = home.join(".ssh");
        fs::create_dir_all(&ssh).unwrap();
        fs::write(ssh.join("config"), "Host *\n  Include extra\n").unwrap();
        fs::write(ssh.join("extra"), "Host pantheon\n  User dev\n").unwrap();
        let state = root.join("state");
        let out = prepare_ssh_config_at(&home, &state).unwrap();
        let body = fs::read_to_string(&out).unwrap();
        assert!(body.contains("Host pantheon"));
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&out).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // No ~/.ssh/config at all → None.
        assert!(prepare_ssh_config_at(&root.join("nohome"), &state).is_none());
    }

    #[test]
    fn config_identity_paths_parses_supported_forms() {
        let root = scratch("cfgpaths");
        let home = root.join("home");
        let cfg = "# IdentityFile ~/.ssh/commented\n\
                   Host *.drush.in\n\
                   \x20 IdentityFile ~/.ssh/pantheon\n\
                   \x20 identityfile /abs/key\n\
                   \x20 CertificateFile %d/.ssh/cert\n\
                   \x20 IdentityFile ~/.ssh/%r-key\n\
                   \x20 IdentityFile relative/key\n\
                   \x20 IdentityFile ~/.ssh/pantheon\n\
                   Host other\n";
        let got = config_identity_paths(cfg, &home);
        assert_eq!(
            got,
            vec![
                home.join(".ssh/pantheon"),
                PathBuf::from("/abs/key"),
                home.join(".ssh/cert"),
            ]
        );
    }

    #[test]
    fn identity_candidates_globs_ids_and_merges_config() {
        let root = scratch("cands");
        let home = root.join("home");
        let ssh = home.join(".ssh");
        fs::create_dir_all(&ssh).unwrap();
        for f in ["id_ed25519", "id_ed25519.pub", "id_rsa", "known_hosts"] {
            fs::write(ssh.join(f), "x").unwrap();
        }
        let cfg = format!(
            "IdentityFile ~/.ssh/pantheon\nIdentityFile {}\n",
            s(&ssh.join("id_rsa")) // duplicate of the glob hit
        );
        let got = identity_candidates(&home, Some(&cfg));
        assert_eq!(
            got,
            vec![
                ssh.join("id_ed25519"),
                ssh.join("id_ed25519.pub"),
                ssh.join("id_rsa"),
                ssh.join("pantheon"),
            ]
        );
        // No .ssh dir: only config-derived candidates.
        assert!(identity_candidates(&root.join("nohome"), None).is_empty());
    }

    #[test]
    fn out_of_tree_target_resolves_agenix_shape() {
        // home/.ssh/id_rsa -> run/agenix/key, run/agenix -> run/agenix.d/1,
        // real file at run/agenix.d/1/key. Covered: home only.
        let root = scratch("agenix");
        let ssh = root.join("home/.ssh");
        let agenix_d = root.join("run/agenix.d/1");
        fs::create_dir_all(&ssh).unwrap();
        fs::create_dir_all(&agenix_d).unwrap();
        fs::write(agenix_d.join("key"), "SECRET").unwrap();
        symlink(root.join("run/agenix.d/1"), root.join("run/agenix")).unwrap();
        symlink(root.join("run/agenix/key"), ssh.join("id_rsa")).unwrap();
        let covered = vec![s(&root.join("home"))];
        let (dest, host) = out_of_tree_target(&ssh.join("id_rsa"), &covered).unwrap();
        // dest is the as-referenced (first out-of-tree) path so the $HOME
        // symlink resolves; host is the fully-resolved real file.
        assert_eq!(dest, s(&root.join("run/agenix/key")));
        assert_eq!(host, s(&fs::canonicalize(agenix_d.join("key")).unwrap()));
    }

    #[test]
    fn out_of_tree_target_walks_through_covered_hops() {
        // id -> home/secrets/key (covered) -> out/key (uncovered, real).
        let root = scratch("hops");
        let home = root.join("home");
        let ssh = home.join(".ssh");
        let out = root.join("out");
        fs::create_dir_all(&ssh).unwrap();
        fs::create_dir_all(home.join("secrets")).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("key"), "SECRET").unwrap();
        symlink(out.join("key"), home.join("secrets/key")).unwrap();
        // Relative target exercises lexical normalization ("../secrets/key").
        symlink(Path::new("../secrets/key"), ssh.join("id_ed25519")).unwrap();
        let covered = vec![s(&home)];
        let (dest, host) = out_of_tree_target(&ssh.join("id_ed25519"), &covered).unwrap();
        assert_eq!(dest, s(&out.join("key")));
        assert_eq!(host, s(&fs::canonicalize(out.join("key")).unwrap()));
    }

    #[test]
    fn out_of_tree_target_rejects_nonlinks_covered_dangling_cycles() {
        let root = scratch("reject");
        let home = root.join("home");
        let ssh = home.join(".ssh");
        fs::create_dir_all(&ssh).unwrap();
        let covered = vec![s(&home)];
        // Regular file: nothing to mount.
        fs::write(ssh.join("plain"), "x").unwrap();
        assert!(out_of_tree_target(&ssh.join("plain"), &covered).is_none());
        // Chain fully inside covered prefixes.
        fs::write(home.join("real"), "x").unwrap();
        symlink(home.join("real"), ssh.join("inside")).unwrap();
        assert!(out_of_tree_target(&ssh.join("inside"), &covered).is_none());
        // Dangling out-of-tree target.
        symlink(root.join("gone/key"), ssh.join("dangling")).unwrap();
        assert!(out_of_tree_target(&ssh.join("dangling"), &covered).is_none());
        // Out-of-tree target that is a directory, not a file.
        symlink(&root, ssh.join("dir")).unwrap();
        assert!(out_of_tree_target(&ssh.join("dir"), &covered).is_none());
        // Symlink cycle inside covered space hits the hop cap.
        symlink(home.join("b"), home.join("a")).unwrap();
        symlink(home.join("a"), home.join("b")).unwrap();
        assert!(out_of_tree_target(&home.join("a"), &covered).is_none());
        // "/" covers everything → always None (FileAccess::All/Host no-op).
        let agx = root.join("agx");
        fs::create_dir_all(&agx).unwrap();
        fs::write(agx.join("key"), "x").unwrap();
        symlink(agx.join("key"), ssh.join("id_all")).unwrap();
        assert!(out_of_tree_target(&ssh.join("id_all"), &["/".to_string()]).is_none());
        assert!(out_of_tree_target(&ssh.join("id_all"), &covered).is_some());
    }

    #[test]
    fn identity_mounts_at_are_ro_and_deduped() {
        let root = scratch("mounts");
        let home = root.join("home");
        let ssh = home.join(".ssh");
        let agx = root.join("agenix");
        fs::create_dir_all(&ssh).unwrap();
        fs::create_dir_all(&agx).unwrap();
        fs::write(agx.join("key"), "SECRET").unwrap();
        // Two links to the same secret → one mount.
        symlink(agx.join("key"), ssh.join("id_rsa")).unwrap();
        symlink(agx.join("key"), ssh.join("id_dup")).unwrap();
        // A plain on-disk key produces no mount.
        fs::write(ssh.join("id_plain"), "x").unwrap();
        let covered = vec![s(&home)];
        let mounts = identity_mounts_at(&home, &covered);
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].ro && !mounts[0].cache);
        assert_eq!(mounts[0].dest, s(&agx.join("key")));
        // Missing .ssh dir → empty.
        assert!(identity_mounts_at(&root.join("nohome"), &covered).is_empty());
    }

    #[test]
    fn normalize_resolves_dots_lexically() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), Path::new("/a/c/d"));
        assert_eq!(normalize(Path::new("/a/../../b")), Path::new("/b"));
    }

    #[test]
    fn unix_socket_alive_probes_connectability() {
        let root = scratch("sock");
        let path = root.join("live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(unix_socket_alive(&s(&path)));
        assert!(!unix_socket_alive(&s(&root.join("missing.sock"))));
        let plain = root.join("plain");
        fs::write(&plain, "x").unwrap();
        assert!(!unix_socket_alive(&s(&plain)));
    }
}
