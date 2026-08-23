//! The file-level ratchet: a shrink-only allowlist of files violating an
//! architectural rule, checked both ways.
//!
//! A ratchet freezes existing debt and makes new debt impossible: every file
//! whose (comment-stripped) body satisfies the `hit` predicate must be pinned
//! in `test/<name>`, and every pinned file must still hit — so paying debt
//! down forces the entry to be deleted, and the list can only shrink.
//!
//! The same helper serves every crate: pass the crate's `CARGO_MANIFEST_DIR`
//! (`env!` expands in the caller). Regeneration is the one sanctioned write,
//! gated on `THEGN_RATCHET_UPDATE=1` (wired by `just ratchet-update`); it keeps
//! the allowlist's leading `#` header block verbatim, so the reasons recorded
//! there survive.
//!
//! `thegn-media` / `thegn-metrics` are core-free leaf crates and carry a
//! verbatim private copy of this file; keep the three identical.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Read `test/<name>` relative to the workspace root (two levels above a
/// crate's manifest dir). `#` lines and blanks are ignored.
pub fn allowlist(manifest_dir: &str, name: &str) -> BTreeSet<String> {
    std::fs::read_to_string(allowlist_path(manifest_dir, name))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn allowlist_path(manifest_dir: &str, name: &str) -> PathBuf {
    PathBuf::from(manifest_dir).join("../../test").join(name)
}

/// Every `.rs` file under the crate's `src/`, as `(src-relative key, body)`,
/// sorted. Keys under any prefix in `exclude` are skipped, as are the ratchet
/// test files themselves (they name the patterns they forbid in their own
/// assertion messages).
pub fn sources(manifest_dir: &str, exclude: &[&str]) -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = PathBuf::from(manifest_dir).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .filter_map(|p| {
            let key = p
                .strip_prefix(&root)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            if key.ends_with("ratchet_tests.rs")
                || key == "ratchet.rs"
                || key == "test_support/ratchet.rs"
                || exclude.iter().any(|x| key.starts_with(x))
            {
                return None;
            }
            let body = std::fs::read_to_string(&p).ok()?;
            Some((key, body))
        })
        .collect()
}

/// Strip `//`-comments so prose naming a glyph, an API or a forbidden pattern
/// doesn't trip the scan. Crude but sufficient for line comments in normal
/// Rust source.
pub fn code_only(body: &str) -> String {
    body.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `body` (comment-stripped) contains a platform-conditional
/// attribute: `#[cfg(` followed — through any `not(`/`any(`/`all(` nesting —
/// by `unix`, `windows`, `target_os`, `target_family` or `target_env`. No
/// regex so the core-free leaf crates can carry a copy.
pub fn has_platform_cfg(body: &str) -> bool {
    const KEYS: [&str; 5] = [
        "unix",
        "windows",
        "target_os",
        "target_family",
        "target_env",
    ];
    let mut rest = body;
    while let Some(i) = rest.find("#[cfg(") {
        let mut inner = rest[i + "#[cfg(".len()..].trim_start();
        loop {
            let mut progressed = false;
            for wrap in ["not(", "any(", "all("] {
                if let Some(r) = inner.strip_prefix(wrap) {
                    inner = r.trim_start();
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        if KEYS.iter().any(|k| {
            inner
                .strip_prefix(k)
                .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
        }) {
            return true;
        }
        rest = &rest[i + 1..];
    }
    false
}

/// Run one ratchet. `hit(key, code_only_body)` decides whether a file
/// violates the rule; `why` is the one-paragraph explanation shown on a new
/// violation (what the rule protects and how to fix the file instead of
/// pinning it).
///
/// With `THEGN_RATCHET_UPDATE=1` the allowlist is rewritten from the current
/// hit set (header preserved) and the check is skipped.
pub fn file_ratchet(
    manifest_dir: &str,
    name: &str,
    exclude: &[&str],
    hit: impl Fn(&str, &str) -> bool,
    why: &str,
) {
    let found: BTreeSet<String> = sources(manifest_dir, exclude)
        .into_iter()
        .filter(|(key, body)| hit(key, &code_only(body)))
        .map(|(key, _)| key)
        .collect();

    if std::env::var("THEGN_RATCHET_UPDATE").as_deref() == Ok("1") {
        let path = allowlist_path(manifest_dir, name);
        let header: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .take_while(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
            .map(str::to_string)
            .collect();
        let mut out = header;
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.extend(found.iter().cloned());
        std::fs::write(&path, out.join("\n") + "\n")
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        return;
    }

    let allow = allowlist(manifest_dir, name);
    let unpinned: Vec<&String> = found.difference(&allow).collect();
    assert!(
        unpinned.is_empty(),
        "ratchet test/{name}: new violation in {unpinned:?}\n{why}\n\
         Fix the file, or — with a reason — pin it in test/{name} \
         (the list is shrink-only: prefer fixing)."
    );
    let stale: Vec<&String> = allow.difference(&found).collect();
    assert!(
        stale.is_empty(),
        "ratchet test/{name}: stale entries {stale:?} — these files no longer \
         violate the rule; the list is shrink-only, so delete them \
         (or run `just ratchet-update`)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_cfg_detection() {
        assert!(has_platform_cfg("#[cfg(unix)] fn a() {}"));
        assert!(has_platform_cfg("#[cfg(not(windows))]"));
        assert!(has_platform_cfg("#[cfg(any(target_os = \"macos\", unix))]"));
        assert!(has_platform_cfg(
            "#[cfg(all(not(target_env = \"msvc\"), feature = \"x\"))]"
        ));
        assert!(!has_platform_cfg("#[cfg(test)]"));
        assert!(!has_platform_cfg("#[cfg(feature = \"dev\")]"));
        assert!(!has_platform_cfg("#[cfg(unixy)]"));
        assert!(!has_platform_cfg(
            "#[cfg(kani)] #[cfg(any(test, feature = \"x\"))]"
        ));
    }

    #[test]
    fn code_only_strips_line_comments() {
        assert_eq!(code_only("a // b\n// c\nd"), "a \n\nd");
    }

    #[test]
    fn ratchet_round_trip_in_a_temp_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("crates").join("x");
        std::fs::create_dir_all(manifest.join("src/sub")).unwrap();
        std::fs::create_dir_all(tmp.path().join("test")).unwrap();
        std::fs::write(manifest.join("src/a.rs"), "fn a() { bad(); }").unwrap();
        std::fs::write(manifest.join("src/sub/b.rs"), "// bad()\nfn b() {}").unwrap();
        std::fs::write(manifest.join("src/c.rs"), "fn c() { bad(); }").unwrap();
        let md = manifest.to_string_lossy().to_string();
        let hit = |_: &str, body: &str| body.contains("bad(");

        // Unpinned violation fails.
        let r = std::panic::catch_unwind(|| file_ratchet(&md, "t.txt", &[], hit, "why"));
        assert!(r.is_err());

        // Regenerate, preserving a header.
        std::fs::write(tmp.path().join("test/t.txt"), "# header\n# two\n").unwrap();
        // SAFETY: test-local env var, single-threaded use within this test.
        unsafe { std::env::set_var("THEGN_RATCHET_UPDATE", "1") };
        file_ratchet(&md, "t.txt", &[], hit, "why");
        unsafe { std::env::remove_var("THEGN_RATCHET_UPDATE") };
        let written = std::fs::read_to_string(tmp.path().join("test/t.txt")).unwrap();
        assert_eq!(written, "# header\n# two\n\na.rs\nc.rs\n");
        assert_eq!(
            allowlist(&md, "t.txt"),
            ["a.rs", "c.rs"].into_iter().map(String::from).collect()
        );

        // Now clean. Comment-only b.rs is not a hit; excluded prefix drops c.rs.
        file_ratchet(&md, "t.txt", &[], hit, "why");
        std::fs::write(tmp.path().join("test/t.txt"), "a.rs\n").unwrap();
        file_ratchet(&md, "t.txt", &["c"], hit, "why");

        // A stale entry fails.
        std::fs::write(tmp.path().join("test/t.txt"), "a.rs\nc.rs\nzzz.rs\n").unwrap();
        let r = std::panic::catch_unwind(|| file_ratchet(&md, "t.txt", &[], hit, "why"));
        assert!(r.is_err());
    }
}
