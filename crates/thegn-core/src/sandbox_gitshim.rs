//! Make a **linked** worktree's git metadata resolve when the sandbox sees the
//! worktree at a path other than its host path.
//!
//! A linked worktree's `.git` is not a directory but a pointer *file*, and git
//! writes that pointer as an **absolute host path**:
//!
//! ```text
//! gitdir: C:/Users/you/repo/.git/worktrees/wt          # <worktree>/.git
//! C:/Users/you/wt/.git                                 # <gitdir>/gitdir
//! ../..                                                # <gitdir>/commondir
//! ```
//!
//! On unix that is fine: [`crate::sandbox::container_path`] is the identity, so
//! the bind lands at the same path and the pointer resolves by construction. On
//! native Windows the mount destination is `/mnt/<drive>/…`, the pointer still
//! says `C:/…`, and git reports `not a git repository: (null)`.
//!
//! Worse, `<git-common>/worktrees/<id>/gitdir` is not `is_absolute_path()` on
//! Linux, so git's `should_prune_worktree` classifies the entry as prunable
//! *before* the `--expire` gate. Because `<git-common>` is mounted whole, that
//! directory holds every sibling tab's metadata — one in-container
//! `git worktree prune` takes them all.
//!
//! This module plans the read-only bind-mounts that fix both: rewritten pointer
//! files that resolve under the mapping, and a read-only `worktrees` parent with
//! the pane's own entry overmounted read-write, so the pane keeps full function
//! (commit, rebase, index writes) while sibling metadata becomes unreachable.
//!
//! The whole thing is gated on the mapping being **non-identity**, not on
//! `cfg!(windows)`: a Windows thegn driving an ssh placement onto a Linux box
//! sees POSIX paths, and there the shim must correctly no-op.
//!
//! [`plan_with`] takes the mapping and the already-read git facts as arguments,
//! so the entire decision is pure and table-tested from the Linux coverage gate
//! — the same trick [`crate::sandbox::map_windows_path`] uses.

use std::path::{Path, PathBuf};

use crate::sandbox::{Mount, container_path};
use crate::{gitdir, util};

/// The extra mounts (and the host files they bind from) that make a linked
/// worktree resolve under a mapped destination.
///
/// `files` is `(host path, contents)` — planning only decides and
/// [`materialize`] writes, so planning performs no I/O of its own.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitShim {
    pub mounts: Vec<Mount>,
    pub files: Vec<(PathBuf, String)>,
}

/// The facts about a worktree's git layout that the plan depends on, read once
/// by [`plan`] so the decision itself stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    /// The `gitdir:` payload of `<worktree>/.git`, verbatim.
    pub pointer: String,
    /// Whether `<gitdir>/commondir` holds an absolute path. Git normally writes
    /// the relative `../..`, which needs no help.
    pub commondir_absolute: bool,
}

/// Whether `s` is absolute in **any** of the path syntaxes we may be reasoning
/// about, independent of the host we are running on.
///
/// `Path::is_absolute` answers for the *host*: on Linux `C:/Users/…` is
/// "relative", which is precisely the trap `gitdir::resolve_pointer` documents.
/// The shim reasons about a Windows layout from a Linux coverage gate, so it
/// needs the syntax-directed answer: POSIX root, drive-letter, or UNC.
fn is_abs_any(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')
        || s.starts_with('\\')
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'/' || b[2] == b'\\'))
}

/// Normalized comparison key: case- and separator-insensitive.
///
/// Unconditional rather than `cfg!(windows)`-gated, and safe because callers
/// only reach it once the mapping has been established as non-identity — i.e.
/// the paths in hand are Windows-shaped. Gating it on the host OS instead would
/// make the case-mismatch branch untestable from the Linux coverage gate.
///
/// Both transforms preserve byte length (`to_ascii_lowercase`, and `\` -> `/`),
/// so `starts_with` on the key stays a valid byte index into the original —
/// which is what lets us splice the untouched remainder onto the mapped prefix.
fn norm_key(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\\' {
                '/'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

/// Join a POSIX container path with a remainder that may still carry `\`.
fn posix_join(prefix: &str, remainder: &str) -> String {
    let rem = remainder.replace('\\', "/");
    let rem = rem.trim_start_matches('/');
    let prefix = prefix.trim_end_matches('/');
    if rem.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}/{rem}")
    }
}

/// The pure core: decide the shim given a mapping and the git facts.
///
/// `None` — "nothing to do" — whenever the layout already resolves: an identity
/// mapping (every unix host, and any remote POSIX placement), or a pointer that
/// is already relative.
pub(crate) fn plan_with(
    worktree: &Path,
    git_common: &Path,
    shim_dir: &Path,
    facts: &GitFacts,
    map: impl Fn(&str) -> String,
) -> Option<GitShim> {
    let wt = worktree.to_string_lossy().into_owned();
    let wt_dest = map(&wt);
    // Identity mapping ⇒ the bind lands at the host path and every pointer
    // resolves as git wrote it.
    if wt_dest == wt {
        return None;
    }
    // A relative pointer resolves under the mapping for free, because the mount
    // destination preserves the host tree's shape.
    if !is_abs_any(&facts.pointer) {
        return None;
    }

    // The pointer is absolute (checked just above), so it IS the gitdir. Using
    // it directly avoids `resolve_pointer`'s host-dependent join semantics —
    // which would treat `C:/…` as relative when this runs on Linux.
    let gd_s = facts.pointer.clone();
    let gc_s = git_common.to_string_lossy().into_owned();
    let gc_dest = map(&gc_s);

    // Anchor the rewrite on the MOUNT DESTINATION, not an independent mapping of
    // the pointer: git and the location may disagree on drive-letter case or
    // separators, and the shim has to agree with the `-v` we actually emit.
    let mut mounts = Vec::new();
    let gd_dest = if norm_key(&gd_s).starts_with(&norm_key(&gc_s)) {
        posix_join(&gc_dest, &gd_s[gc_s.len()..])
    } else {
        // `--separate-git-dir`: the gitdir lives outside the common dir, so it
        // is not covered by the git-common bind and needs one of its own.
        let d = map(&gd_s);
        mounts.push(Mount {
            host: gd_s.clone(),
            dest: d.clone(),
            ro: false,
            cache: false,
        });
        d
    };

    // Protect sibling tabs: `worktrees` read-only with this pane's own entry
    // overmounted read-write. Only meaningful when the gitdir really is
    // `<git-common>/worktrees/<id>` (the linked-worktree layout). Emitted before
    // the pointer shims so the parent bind never lands on top of them.
    let worktrees = posix_join(&gc_dest, "worktrees");
    if gd_dest.starts_with(&format!("{worktrees}/")) {
        mounts.push(Mount {
            host: git_common.join("worktrees").to_string_lossy().into_owned(),
            dest: worktrees,
            ro: true,
            cache: false,
        });
        mounts.push(Mount {
            host: gd_s.clone(),
            dest: gd_dest.clone(),
            ro: false,
            cache: false,
        });
    }

    let mut files = Vec::new();
    let mut push = |file: &str, contents: String, dest: String| {
        let host = shim_dir.join(file);
        files.push((host.clone(), contents));
        mounts.push(Mount {
            host: host.to_string_lossy().into_owned(),
            dest,
            ro: true,
            cache: false,
        });
    };

    // The two pointers. BOTH are required: shimming only `.git` and leaving the
    // original back-pointer in place still yields `not a git repository`,
    // because git validates the round trip during discovery.
    let dotgit_dest = posix_join(&wt_dest, ".git");
    push(
        "dotgit",
        format!("gitdir: {gd_dest}\n"),
        dotgit_dest.clone(),
    );
    push(
        "gitdir",
        format!("{dotgit_dest}\n"),
        posix_join(&gd_dest, "gitdir"),
    );

    // `commondir` is normally the relative `../..`, which resolves under any
    // mapping. Only an absolute one needs help — and the portable answer is to
    // make it relative rather than to map it.
    if facts.commondir_absolute {
        push(
            "commondir",
            "../..\n".to_string(),
            posix_join(&gd_dest, "commondir"),
        );
    }

    Some(GitShim { mounts, files })
}

/// Read the worktree's git facts. `None` for a main checkout, whose `.git` is a
/// directory rather than a pointer file, and for a malformed pointer.
fn facts_for(worktree: &Path) -> Option<GitFacts> {
    let contents = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let pointer = gitdir::parse_dotgit_pointer(&contents)?.to_string();
    let gd = gitdir::resolve_pointer(worktree, &pointer);
    let commondir_absolute = std::fs::read_to_string(gd.join("commondir"))
        .is_ok_and(|s| Path::new(s.trim()).is_absolute());
    Some(GitFacts {
        pointer,
        commondir_absolute,
    })
}

/// Plan the shim for `worktree`, whose repository's common dir is `git_common`.
///
/// `name` is the sandbox name, so the shim files land at a path that is
/// deterministic at spec-resolve time.
pub fn plan(worktree: &Path, git_common: &Path, name: &str) -> Option<GitShim> {
    let facts = facts_for(worktree)?;
    let dir = util::thegn_dir().join("gitshim").join(name);
    plan_with(worktree, git_common, &dir, &facts, container_path)
}

/// Write the planned shim files to the host. Idempotent, so a re-spawn that
/// re-plans the same shim is a no-op rewrite rather than an error.
pub fn materialize(files: &[(PathBuf, String)]) -> std::io::Result<()> {
    for (path, contents) in files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::map_windows_path;

    const WT: &str = r"C:\Users\u\wt";
    const GC: &str = r"C:\Users\u\repo\.git";
    const PTR: &str = "C:/Users/u/repo/.git/worktrees/wt";

    fn facts(pointer: &str, commondir_absolute: bool) -> GitFacts {
        GitFacts {
            pointer: pointer.into(),
            commondir_absolute,
        }
    }

    /// Plan against the Windows mapping regardless of the host we test on —
    /// which is the point of injecting the mapper.
    fn plan_win(wt: &str, gc: &str, f: &GitFacts) -> Option<GitShim> {
        plan_with(
            Path::new(wt),
            Path::new(gc),
            Path::new("/shim"),
            f,
            map_windows_path,
        )
    }

    /// Contents of the shim file with this basename.
    fn file(s: &GitShim, name: &str) -> Option<String> {
        s.files
            .iter()
            .find(|(p, _)| p.file_name().is_some_and(|n| n == name))
            .map(|(_, c)| c.clone())
    }

    fn mount(s: &GitShim, dest: &str) -> Option<Mount> {
        s.mounts.iter().find(|m| m.dest == dest).cloned()
    }

    #[test]
    fn identity_mapping_is_a_noop() {
        // Every unix host, and a remote POSIX placement driven from Windows.
        let f = facts("/home/u/repo/.git/worktrees/wt", false);
        assert_eq!(
            plan_with(
                Path::new("/home/u/wt"),
                Path::new("/home/u/repo/.git"),
                Path::new("/shim"),
                &f,
                |s: &str| s.to_string()
            ),
            None
        );
    }

    #[test]
    fn relative_pointer_needs_no_shim() {
        // `worktree.useRelativePaths` (git >= 2.48) resolves under the mapping
        // for free, because the destination preserves the host tree's shape.
        assert_eq!(
            plan_win(WT, GC, &facts("../repo/.git/worktrees/wt", false)),
            None
        );
    }

    #[test]
    fn linked_worktree_resolves_under_the_mapping() {
        let s = plan_win(WT, GC, &facts(PTR, false)).expect("shim planned");
        // Both pointers are rewritten into the mapped namespace…
        assert_eq!(
            file(&s, "dotgit").as_deref(),
            Some("gitdir: /mnt/c/Users/u/repo/.git/worktrees/wt\n")
        );
        // …including the back-pointer, which is load-bearing for RESOLUTION,
        // not merely for prune: shimming only `.git` still yields
        // `not a git repository`.
        assert_eq!(
            file(&s, "gitdir").as_deref(),
            Some("/mnt/c/Users/u/wt/.git\n")
        );
        // …and each is bound read-only over the container's view.
        let dg = mount(&s, "/mnt/c/Users/u/wt/.git").expect("dotgit bind");
        assert!(dg.ro, "the pointer shims must be read-only");
        assert!(mount(&s, "/mnt/c/Users/u/repo/.git/worktrees/wt/gitdir").is_some_and(|m| m.ro));
    }

    #[test]
    fn sibling_metadata_is_read_only_and_the_pane_keeps_its_own() {
        let s = plan_win(WT, GC, &facts(PTR, false)).expect("shim planned");
        let parent = mount(&s, "/mnt/c/Users/u/repo/.git/worktrees").expect("worktrees bind");
        assert!(parent.ro, "sibling metadata must not be writable");
        let own = mount(&s, "/mnt/c/Users/u/repo/.git/worktrees/wt").expect("own bind");
        assert!(!own.ro, "the pane must still commit in its own worktree");
        // Order matters: the ro parent must come first so the rw child overmounts
        // it, and both before the file pins so a directory bind never lands on
        // top of them. Same idiom as the `.git`(rw) -> `.git/config`(ro) pin.
        let idx = |d: &str| s.mounts.iter().position(|m| m.dest == d).unwrap();
        assert!(
            idx("/mnt/c/Users/u/repo/.git/worktrees")
                < idx("/mnt/c/Users/u/repo/.git/worktrees/wt")
        );
        assert!(idx("/mnt/c/Users/u/repo/.git/worktrees/wt") < idx("/mnt/c/Users/u/wt/.git"));
    }

    #[test]
    fn commondir_is_shimmed_only_when_absolute() {
        // Git normally writes `../..`, which resolves under any mapping.
        let rel = plan_win(WT, GC, &facts(PTR, false)).unwrap();
        assert_eq!(file(&rel, "commondir"), None);
        // An absolute one is normalised to the relative form rather than mapped
        // — strictly more portable.
        let abs = plan_win(WT, GC, &facts(PTR, true)).unwrap();
        assert_eq!(file(&abs, "commondir").as_deref(), Some("../..\n"));
        assert!(
            mount(&abs, "/mnt/c/Users/u/repo/.git/worktrees/wt/commondir").is_some_and(|m| m.ro)
        );
    }

    #[test]
    fn separate_git_dir_gets_its_own_mount() {
        // `--separate-git-dir`: the gitdir is outside the common dir, so the
        // git-common bind does not cover it.
        let s = plan_win(WT, GC, &facts("C:/elsewhere/gitdirs/wt", false)).expect("shim planned");
        let gd = mount(&s, "/mnt/c/elsewhere/gitdirs/wt").expect("separate gitdir bind");
        assert!(!gd.ro, "the gitdir itself stays writable");
        assert_eq!(
            file(&s, "dotgit").as_deref(),
            Some("gitdir: /mnt/c/elsewhere/gitdirs/wt\n")
        );
        // Not under `<git-common>/worktrees`, so no sibling-protection pair.
        assert_eq!(mount(&s, "/mnt/c/Users/u/repo/.git/worktrees"), None);
    }

    #[test]
    fn drive_case_and_separator_mismatch_still_anchors_on_the_mount_dest() {
        // git and `loc.path()` can disagree on drive-letter case and separators.
        // The rewrite is anchored on the mount destination, so the shim and the
        // `-v` we emit agree anyway — the failure mode this prevents is a shim
        // pointing somewhere nothing is mounted.
        let s = plan_win(WT, GC, &facts(r"c:\USERS\u\repo\.git\worktrees\wt", false))
            .expect("shim planned");
        assert_eq!(
            file(&s, "dotgit").as_deref(),
            Some("gitdir: /mnt/c/Users/u/repo/.git/worktrees/wt\n"),
            "the mapped prefix must come from git_common, not from the pointer's casing"
        );
    }

    #[test]
    fn main_checkout_has_no_pointer_to_shim() {
        // `.git` is a directory, so there is nothing pointing anywhere.
        let dir = std::env::temp_dir().join("thegn-gitshim-main-checkout");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(facts_for(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_writes_every_planned_file() {
        let dir = std::env::temp_dir().join("thegn-gitshim-materialize");
        let _ = std::fs::remove_dir_all(&dir);
        let s = plan_with(
            Path::new(WT),
            Path::new(GC),
            &dir,
            &facts(PTR, false),
            map_windows_path,
        )
        .unwrap();
        materialize(&s.files).expect("shim files written");
        for (path, contents) in &s.files {
            assert_eq!(&std::fs::read_to_string(path).unwrap(), contents);
        }
        // Idempotent: a re-spawn re-plans and rewrites without error.
        materialize(&s.files).expect("second materialize is a no-op rewrite");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
