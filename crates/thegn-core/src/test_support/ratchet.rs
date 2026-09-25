//! The file-level ratchet: a shrink-only allowlist of files violating an
//! architectural rule, checked both ways.
//!
//! A ratchet freezes existing debt and makes new debt impossible: every file
//! whose (comment-stripped) body satisfies the hit predicate must be pinned
//! in test/<name>, and every pinned file must still hit — so paying debt
//! down forces the entry to be deleted, and the list can only shrink.
//!
//! The same helper serves every crate: pass the crate's CARGO_MANIFEST_DIR
//! (env! expands in the caller). Regeneration is the one sanctioned write,
//! gated on THEGN_RATCHET_UPDATE=1 (wired by just ratchet-update); it keeps
//! the allowlist's leading # header block verbatim, so the reasons recorded
//! there survive. A ratchet with no pins and no hits must carry the explicit
//! `# RATCHET-EMPTY` marker; an unmarked empty allowlist is an error.
//!
//! thegn-media / thegn-metrics are core-free leaf crates and carry a
//! verbatim private copy of this file; keep the three identical. The scan is
//! not atomic with respect to concurrent working-tree edits: a scan racing an
//! edit may reflect either version, and the next run is the correction
//! mechanism.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

trait RatchetIo {
    fn read_dir(&self, path: &Path) -> io::Result<Vec<io::Result<PathBuf>>>;
    fn metadata(&self, path: &Path) -> io::Result<std::fs::Metadata>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
}

struct RealIo;

const EMPTY_ALLOWLIST_MARKER: &str = "# RATCHET-EMPTY";

struct Allowlist {
    entries: BTreeSet<String>,
    explicitly_empty: bool,
}

impl RatchetIo for RealIo {
    fn read_dir(&self, path: &Path) -> io::Result<Vec<io::Result<PathBuf>>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            entries.push(entry.map(|entry| entry.path()));
        }
        Ok(entries)
    }

    fn metadata(&self, path: &Path) -> io::Result<std::fs::Metadata> {
        std::fs::symlink_metadata(path)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        std::fs::write(path, contents)
    }
}

fn io_failure(operation: &str, path: &Path, error: impl std::fmt::Display) -> String {
    format!("{operation} {}: {error}", path.display())
}

fn allowlist_path(manifest_dir: &str, name: &str) -> PathBuf {
    PathBuf::from(manifest_dir).join("../../test").join(name)
}

fn read_allowlist<R: RatchetIo>(
    reader: &R,
    manifest_dir: &str,
    name: &str,
) -> Result<Allowlist, String> {
    let path = allowlist_path(manifest_dir, name);
    let contents = reader
        .read_to_string(&path)
        .map_err(|error| io_failure("read allowlist", &path, error))?;
    let entries: BTreeSet<String> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    let explicitly_empty = contents
        .lines()
        .map(str::trim)
        .any(|line| line == EMPTY_ALLOWLIST_MARKER);
    if explicitly_empty && !entries.is_empty() {
        return Err(format!(
            "allowlist {}: {EMPTY_ALLOWLIST_MARKER} cannot be combined with entries",
            path.display()
        ));
    }
    if entries.is_empty() && !explicitly_empty {
        return Err(format!(
            "allowlist {}: empty allowlist requires {EMPTY_ALLOWLIST_MARKER}",
            path.display()
        ));
    }
    Ok(Allowlist {
        entries,
        explicitly_empty,
    })
}

/// Read test/<name> relative to the workspace root (two levels above a
/// crate's manifest dir). # lines and blanks are ignored.
pub fn allowlist(manifest_dir: &str, name: &str) -> BTreeSet<String> {
    read_allowlist(&RealIo, manifest_dir, name)
        .map(|allowlist| allowlist.entries)
        .unwrap_or_else(|error| panic!("{error}"))
}

fn source_key(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path.strip_prefix(root).map_err(|error| {
        format!(
            "normalize source path {} relative to {}: {error}",
            path.display(),
            root.display()
        )
    })?;
    let relative = relative.to_str().ok_or_else(|| {
        format!(
            "normalize source path {} relative to {}: path is not valid UTF-8",
            path.display(),
            root.display()
        )
    })?;
    Ok(relative.replace('\\', "/"))
}

fn excluded(key: &str, exclude: &[&str]) -> bool {
    key.ends_with("ratchet_tests.rs")
        || key == "ratchet.rs"
        || key == "test_support/ratchet.rs"
        || exclude.iter().any(|prefix| key.starts_with(prefix))
}

fn collect_paths<R: RatchetIo>(
    reader: &R,
    root: &Path,
    dir: &Path,
    exclude: &[&str],
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    let metadata = reader
        .metadata(dir)
        .map_err(|error| io_failure("read source metadata", dir, error))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refuse symlinked source entry {}: symlinked paths are not scanned",
            dir.display()
        ));
    }
    let entries = reader
        .read_dir(dir)
        .map_err(|error| io_failure("read source directory", dir, error))?;
    for entry in entries {
        let path = entry.map_err(|error| io_failure("read directory entry", dir, error))?;
        let key = source_key(root, &path)?;
        let metadata = reader
            .metadata(&path)
            .map_err(|error| io_failure("read source metadata", &path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refuse symlinked source entry {}: symlinked paths are not scanned",
                path.display()
            ));
        }
        if metadata.is_dir() {
            collect_paths(reader, root, &path, exclude, out)?;
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !excluded(&key, exclude)
        {
            out.push((key, path));
        }
    }
    Ok(())
}

/// Every .rs file under the crate's src/, as (src-relative key, body),
/// sorted. Keys under any prefix in exclude are skipped, as are the ratchet
/// test files themselves (they name the patterns they forbid in their own
/// assertion messages). The scan is not atomic with respect to concurrent
/// working-tree edits; a racing edit may produce either version.
pub fn sources(manifest_dir: &str, exclude: &[&str]) -> Vec<(String, String)> {
    sources_with(&RealIo, manifest_dir, exclude).unwrap_or_else(|error| panic!("{error}"))
}

fn sources_with<R: RatchetIo>(
    reader: &R,
    manifest_dir: &str,
    exclude: &[&str],
) -> Result<Vec<(String, String)>, String> {
    let root = PathBuf::from(manifest_dir).join("src");
    let mut paths = Vec::new();
    collect_paths(reader, &root, &root, exclude, &mut paths)?;
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    paths
        .into_iter()
        .map(|(key, path)| {
            let body = reader
                .read_to_string(&path)
                .map_err(|error| io_failure("read source", &path, error))?;
            Ok((key, body))
        })
        .collect()
}

fn raw_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    if bytes.get(index) == Some(&b'b') {
        index += 1;
    }
    if bytes.get(index) != Some(&b'r') {
        return None;
    }
    index += 1;
    let mut hashes = 0;
    while bytes.get(index) == Some(&b'#') {
        hashes += 1;
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let mut closing = index + 1;
            let mut closing_hashes = 0;
            while closing_hashes < hashes && bytes.get(closing) == Some(&b'#') {
                closing += 1;
                closing_hashes += 1;
            }
            if closing_hashes == hashes {
                return Some(closing);
            }
        }
        index += 1;
    }
    Some(bytes.len())
}

fn quoted_string_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = index.saturating_add(2),
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// Strip comments while respecting ordinary, escaped, byte, and raw strings.
/// Block comments are nested as they are in Rust. Newlines in comments remain
/// so source locations and line-oriented diagnostics stay useful.
pub fn code_only(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut output = String::with_capacity(body.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = raw_string_end(bytes, index) {
            output.push_str(&body[index..end]);
            index = end;
            continue;
        }
        if bytes[index] == b'"' || (bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'"')) {
            let quote = if bytes[index] == b'"' {
                index
            } else {
                index + 1
            };
            let end = quoted_string_end(bytes, quote);
            output.push_str(&body[index..end]);
            index = end;
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"//") {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
                index += 1;
            }
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"/*") {
            index += 2;
            let mut depth = 1;
            while index < bytes.len() && depth > 0 {
                if bytes.get(index..index + 2) == Some(b"/*") {
                    depth += 1;
                    index += 2;
                } else if bytes.get(index..index + 2) == Some(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    if bytes[index] == b'\n' || bytes[index] == b'\r' {
                        output.push(bytes[index] as char);
                    }
                    index += 1;
                }
            }
            continue;
        }
        let character = body[index..].chars().next().expect("index is in body");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token<'a> {
    Hash,
    OpenBracket,
    CloseBracket,
    OpenParen,
    CloseParen,
    Comma,
    Ident(&'a str),
    Literal,
    Other,
}

fn lex_code<'a>(code: &'a str) -> Vec<Token<'a>> {
    let bytes = code.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = raw_string_end(bytes, index) {
            tokens.push(Token::Literal);
            index = end;
            continue;
        }
        if bytes[index] == b'"' || (bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'"')) {
            let quote = if bytes[index] == b'"' {
                index
            } else {
                index + 1
            };
            tokens.push(Token::Literal);
            index = quoted_string_end(bytes, quote);
            continue;
        }
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            tokens.push(Token::Ident(&code[start..index]));
            continue;
        }
        let token = match bytes[index] {
            b'#' => Token::Hash,
            b'[' => Token::OpenBracket,
            b']' => Token::CloseBracket,
            b'(' => Token::OpenParen,
            b')' => Token::CloseParen,
            b',' => Token::Comma,
            _ => Token::Other,
        };
        tokens.push(token);
        index += 1;
    }
    tokens
}

fn predicate(tokens: &[Token<'_>], mut index: usize) -> (bool, usize) {
    let Some(token) = tokens.get(index) else {
        return (false, index);
    };
    match token {
        Token::Ident(name) if tokens.get(index + 1) == Some(&Token::OpenParen) => {
            index += 2;
            let mut found = false;
            while index < tokens.len() && tokens[index] != Token::CloseParen {
                let (nested, next) = predicate(tokens, index);
                found |= nested;
                if next == index {
                    index += 1;
                } else {
                    index = next;
                }
                if tokens.get(index) == Some(&Token::Comma) {
                    index += 1;
                } else if tokens.get(index) == Some(&Token::CloseParen) {
                    index += 1;
                    break;
                }
            }
            let _ = name;
            (found, index)
        }
        Token::Ident(name) => {
            let found = matches!(
                *name,
                "unix" | "windows" | "target_os" | "target_family" | "target_env"
            );
            index += 1;
            let mut depth = 0;
            while index < tokens.len() {
                match tokens[index] {
                    Token::OpenParen => depth += 1,
                    Token::CloseParen if depth == 0 => break,
                    Token::CloseParen => depth -= 1,
                    Token::Comma if depth == 0 => break,
                    _ => {}
                }
                index += 1;
            }
            (found, index)
        }
        _ => (false, index + 1),
    }
}

/// Whether body contains a platform-conditional cfg or cfg_attr attribute.
/// The predicate walker visits every comma-separated sibling and nested
/// expression, so argument order cannot hide a platform leaf.
pub fn has_platform_cfg(body: &str) -> bool {
    let code = code_only(body);
    let tokens = lex_code(&code);
    let mut index = 0;
    while index + 3 < tokens.len() {
        if tokens[index] == Token::Hash && tokens[index + 1] == Token::OpenBracket {
            let is_cfg = matches!(tokens[index + 2], Token::Ident("cfg" | "cfg_attr"));
            if is_cfg && tokens[index + 3] == Token::OpenParen {
                let (found, _) = predicate(&tokens, index + 4);
                if found {
                    return true;
                }
            }
        }
        index += 1;
    }
    false
}

fn file_ratchet_with<R: RatchetIo>(
    reader: &R,
    manifest_dir: &str,
    name: &str,
    exclude: &[&str],
    hit: impl Fn(&str, &str) -> bool,
    why: &str,
    update: bool,
) -> Result<(), String> {
    let found: BTreeSet<String> = sources_with(reader, manifest_dir, exclude)?
        .into_iter()
        .filter(|(key, body)| hit(key, &code_only(body)))
        .map(|(key, _)| key)
        .collect();

    if update {
        let path = allowlist_path(manifest_dir, name);
        let header: Vec<String> = reader
            .read_to_string(&path)
            .map_err(|error| io_failure("read allowlist header", &path, error))?
            .lines()
            .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
            .filter(|line| line.trim() != EMPTY_ALLOWLIST_MARKER)
            .map(str::to_string)
            .collect();
        let mut output = header;
        if output.last().is_some_and(|line| !line.trim().is_empty()) {
            output.push(String::new());
        }
        if found.is_empty() {
            output.push(EMPTY_ALLOWLIST_MARKER.to_string());
        } else {
            output.extend(found.iter().cloned());
        }
        let contents = output.join("\n") + "\n";
        reader
            .write(&path, contents.as_bytes())
            .map_err(|error| io_failure("write allowlist", &path, error))?;
        return Ok(());
    }

    let allow = read_allowlist(reader, manifest_dir, name)?;
    let unpinned: Vec<&String> = found.difference(&allow.entries).collect();
    if !unpinned.is_empty() {
        return Err(format!(
            "ratchet test/{name}: new violation in {unpinned:?}\n{why}\n\
             Fix the file, or — with a reason — pin it in test/{name} \
             (the list is shrink-only: prefer fixing)."
        ));
    }
    if found.is_empty() && allow.explicitly_empty {
        return Ok(());
    }
    let stale: Vec<&String> = allow.entries.difference(&found).collect();
    if !stale.is_empty() {
        return Err(format!(
            "ratchet test/{name}: stale entries {stale:?} — these files no longer \
             violate the rule; the list is shrink-only, so delete them \
             (or run just ratchet-update)."
        ));
    }
    Ok(())
}

/// Run one ratchet. hit(key, code_only_body) decides whether a file violates
/// the rule; why is the one-paragraph explanation shown on a new violation.
///
/// With THEGN_RATCHET_UPDATE=1 the allowlist is rewritten from the current
/// hit set (header preserved) and the check is skipped.
pub fn file_ratchet(
    manifest_dir: &str,
    name: &str,
    exclude: &[&str],
    hit: impl Fn(&str, &str) -> bool,
    why: &str,
) {
    let update = std::env::var("THEGN_RATCHET_UPDATE").as_deref() == Ok("1");
    file_ratchet_with(&RealIo, manifest_dir, name, exclude, hit, why, update)
        .unwrap_or_else(|error| panic!("{error}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_cfg_detection_walks_complete_predicates() {
        assert!(has_platform_cfg(
            "#[cfg(all(feature = \"profiling\", unix))] fn a() {}"
        ));
        assert!(has_platform_cfg(
            "#[cfg(not(any(feature = \"x\", all(feature = \"y\", target_os = \"macos\"))))]"
        ));
        assert!(has_platform_cfg(
            "#[cfg_attr(all(feature = \"x\", target_family = \"unix\"), allow(dead_code))]"
        ));
        assert!(has_platform_cfg(
            "#[cfg(any(feature = \"x\", /* unix */ windows))]"
        ));
        assert!(!has_platform_cfg("#[cfg(feature = \"unix\")]"));
        assert!(!has_platform_cfg(
            "#[cfg(kani)] #[cfg(any(test, feature = \"x\"))]"
        ));
    }

    #[test]
    fn comments_and_strings_cannot_hide_or_create_attributes() {
        assert!(has_platform_cfg(
            "let ordinary = \"// #[cfg(unix)]\"; #[cfg(windows)] fn a() {}"
        ));
        assert!(has_platform_cfg(
            "let ordinary = \"escaped \\\" //\"; #[cfg(windows)] fn a() {}"
        ));
        assert!(has_platform_cfg(
            r##"let raw = r#""// #[cfg(unix)]"#; #[cfg(target_env = "gnu")]"##
        ));
        assert!(has_platform_cfg(
            "/* #[cfg(unix)] */ #[cfg(target_family = \"unix\")]"
        ));
        assert!(!has_platform_cfg("let ordinary = \"// #[cfg(unix)]\";"));
        assert!(!has_platform_cfg(r##"let raw = r#""// #[cfg(unix)]"#;"##));
    }

    #[test]
    fn code_only_strips_comments_without_touching_strings() {
        assert_eq!(code_only("a // b\n// c\nd"), "a \n\nd");
        assert_eq!(
            code_only(r##"let s = "\"// stays"; /* gone */ #[cfg(unix)]"##),
            r##"let s = "\"// stays";  #[cfg(unix)]"##
        );
        assert_eq!(
            code_only(
                r##"let s = r#""// stays /* too */"#; // gone
#[cfg(unix)]"##
            ),
            // The literal is `r#"…"#` whose CONTENT begins with a quote, i.e.
            // `"// stays /* too */`. Both quotes survive: the first belongs to
            // the content, the second closes the literal.
            "let s = r#\"\"// stays /* too */\"#; \n#[cfg(unix)]"
        );
    }

    #[derive(Clone, Copy)]
    enum Failure {
        Traversal,
        Entry,
        Metadata,
        Normalization,
        SourceRead,
        AllowlistRead,
    }

    struct FailingIo {
        failure: Failure,
    }

    impl RatchetIo for FailingIo {
        fn read_dir(&self, path: &Path) -> io::Result<Vec<io::Result<PathBuf>>> {
            if matches!(self.failure, Failure::Traversal) && path.ends_with("src") {
                return Err(io::Error::other("injected traversal failure"));
            }
            if matches!(self.failure, Failure::Entry) && path.ends_with("src") {
                return Ok(vec![Err(io::Error::other("injected entry failure"))]);
            }
            if matches!(self.failure, Failure::Normalization) && path.ends_with("src") {
                return Ok(vec![Ok(PathBuf::from("outside.rs"))]);
            }
            RealIo.read_dir(path)
        }

        fn metadata(&self, path: &Path) -> io::Result<std::fs::Metadata> {
            if matches!(self.failure, Failure::Metadata)
                && path.file_name().is_some_and(|x| x == "a.rs")
            {
                return Err(io::Error::other("injected metadata failure"));
            }
            RealIo.metadata(path)
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            if matches!(self.failure, Failure::SourceRead)
                && path.file_name().is_some_and(|x| x == "a.rs")
            {
                return Err(io::Error::other("injected source read failure"));
            }
            if matches!(self.failure, Failure::AllowlistRead)
                && path.file_name().is_some_and(|x| x == "t.txt")
            {
                return Err(io::Error::other("injected allowlist read failure"));
            }
            RealIo.read_to_string(path)
        }

        fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
            RealIo.write(path, contents)
        }
    }

    fn temp_crate() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("crates").join("x");
        std::fs::create_dir_all(manifest.join("src/sub")).unwrap();
        std::fs::create_dir_all(tmp.path().join("test")).unwrap();
        std::fs::write(manifest.join("src/a.rs"), "fn a() { bad(); }").unwrap();
        std::fs::write(manifest.join("src/sub/b.rs"), "// bad()\nfn b() {}\n").unwrap();
        std::fs::write(manifest.join("src/c.rs"), "fn c() { bad(); }").unwrap();
        std::fs::write(
            tmp.path().join("test/t.txt"),
            "# header\n# two\n# RATCHET-EMPTY\n",
        )
        .unwrap();
        (tmp, manifest.to_string_lossy().into_owned())
    }

    #[test]
    fn filesystem_failures_are_not_silently_omitted() {
        let cases = [
            (Failure::Traversal, "read source directory", "src"),
            (Failure::Entry, "read directory entry", "src"),
            (Failure::Metadata, "read source metadata", "a.rs"),
            (Failure::SourceRead, "read source", "a.rs"),
            (
                Failure::Normalization,
                "normalize source path",
                "outside.rs",
            ),
        ];
        for (failure, operation, path_fragment) in cases {
            let (tmp, manifest) = temp_crate();
            let error = sources_with(&FailingIo { failure }, &manifest, &[]).unwrap_err();
            assert!(error.contains(operation), "{error}");
            assert!(error.contains(path_fragment), "{error}");
            if !matches!(failure, Failure::Normalization) {
                assert!(error.contains("injected"), "{error}");
            }
            drop(tmp);
        }

        let (tmp, manifest) = temp_crate();
        let error = file_ratchet_with(
            &FailingIo {
                failure: Failure::AllowlistRead,
            },
            &manifest,
            "t.txt",
            &[],
            |_, _| false,
            "why",
            false,
        )
        .unwrap_err();
        assert!(error.contains("read allowlist"), "{error}");
        assert!(error.contains("t.txt"), "{error}");
        assert!(error.contains("injected"), "{error}");
        drop(tmp);
    }

    #[test]
    fn ratchet_round_trip_preserves_header_and_checks_both_sets() {
        let (tmp, manifest) = temp_crate();
        let hit = |_: &str, body: &str| body.contains("bad(");

        let error =
            file_ratchet_with(&RealIo, &manifest, "t.txt", &[], hit, "why", false).unwrap_err();
        assert!(error.contains("new violation"));

        file_ratchet_with(&RealIo, &manifest, "t.txt", &[], hit, "why", true).unwrap();
        let written = std::fs::read_to_string(tmp.path().join("test/t.txt")).unwrap();
        assert_eq!(written, "# header\n# two\n\na.rs\nc.rs\n");
        assert_eq!(
            allowlist(&manifest, "t.txt"),
            ["a.rs", "c.rs"].into_iter().map(String::from).collect()
        );

        file_ratchet_with(&RealIo, &manifest, "t.txt", &[], hit, "why", false).unwrap();
        std::fs::write(tmp.path().join("test/t.txt"), "a.rs\n").unwrap();
        file_ratchet_with(&RealIo, &manifest, "t.txt", &["c"], hit, "why", false).unwrap();

        std::fs::write(tmp.path().join("test/t.txt"), "a.rs\nc.rs\nzzz.rs\n").unwrap();
        let error =
            file_ratchet_with(&RealIo, &manifest, "t.txt", &[], hit, "why", false).unwrap_err();
        assert!(error.contains("stale entries"));
    }

    #[test]
    fn missing_allowlist_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("crates/x");
        std::fs::create_dir_all(manifest.join("src")).unwrap();
        std::fs::write(manifest.join("src/a.rs"), "fn a() {}\n").unwrap();
        let manifest = manifest.to_string_lossy().into_owned();
        let error = file_ratchet_with(
            &RealIo,
            &manifest,
            "missing.txt",
            &[],
            |_, _| false,
            "why",
            false,
        )
        .unwrap_err();
        assert!(error.contains("read allowlist"), "{error}");
        assert!(error.contains("missing.txt"), "{error}");
    }

    #[test]
    fn empty_allowlist_fails_closed_even_when_no_file_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("crates/x");
        std::fs::create_dir_all(manifest.join("src")).unwrap();
        std::fs::create_dir_all(tmp.path().join("test")).unwrap();
        std::fs::write(manifest.join("src/a.rs"), "fn a() {}\n").unwrap();
        let manifest = manifest.to_string_lossy().into_owned();
        for contents in ["", "# comment-only\n"] {
            std::fs::write(
                PathBuf::from(&manifest).join("../../test/empty.txt"),
                contents,
            )
            .unwrap();
            let error = file_ratchet_with(
                &RealIo,
                &manifest,
                "empty.txt",
                &[],
                |_, _| false,
                "why",
                false,
            )
            .unwrap_err();
            assert!(error.contains("empty allowlist"), "{error}");
            assert!(error.contains("empty.txt"), "{error}");
        }
    }

    #[test]
    fn explicit_empty_marker_allows_zero_hits_and_update_preserves_it() {
        let (tmp, manifest) = temp_crate();
        std::fs::write(tmp.path().join("test/t.txt"), "# header\n# RATCHET-EMPTY\n").unwrap();
        file_ratchet_with(&RealIo, &manifest, "t.txt", &[], |_, _| false, "why", false).unwrap();

        file_ratchet_with(&RealIo, &manifest, "t.txt", &[], |_, _| false, "why", true).unwrap();
        let expected = "# header\n\n# RATCHET-EMPTY\n";
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("test/t.txt")).unwrap(),
            expected
        );
        file_ratchet_with(&RealIo, &manifest, "t.txt", &[], |_, _| false, "why", true).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("test/t.txt")).unwrap(),
            expected
        );
    }

    #[test]
    fn empty_current_hit_set_fails_against_nonempty_allowlist() {
        let (tmp, manifest) = temp_crate();
        std::fs::write(tmp.path().join("test/t.txt"), "a.rs\n").unwrap();
        let error = file_ratchet_with(&RealIo, &manifest, "t.txt", &[], |_, _| false, "why", false)
            .unwrap_err();
        assert!(error.contains("stale entries"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_source_entries_fail_closed_without_recursing() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("crates/x");
        std::fs::create_dir_all(manifest.join("src")).unwrap();
        std::fs::write(manifest.join("src/a.rs"), "fn a() {}\n").unwrap();
        symlink(manifest.join("src"), manifest.join("src/loop")).unwrap();
        let manifest = manifest.to_string_lossy().into_owned();
        let error = sources_with(&RealIo, &manifest, &[]).unwrap_err();
        assert!(error.contains("refuse symlinked source entry"), "{error}");
        assert!(error.contains("loop"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_source_root_fails_closed_without_external_traversal() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let manifest = tmp.path().join("crates/x");
        let external_src = tmp.path().join("external-src");
        std::fs::create_dir_all(&manifest).unwrap();
        std::fs::create_dir_all(&external_src).unwrap();
        std::fs::write(external_src.join("escaped.rs"), "fn escaped() {}\n").unwrap();
        symlink(&external_src, manifest.join("src")).unwrap();
        let manifest = manifest.to_string_lossy().into_owned();

        let error = sources_with(&RealIo, &manifest, &[]).unwrap_err();
        assert!(error.contains("refuse symlinked source entry"), "{error}");
        assert!(error.contains("src"), "{error}");
    }

    #[test]
    fn ratchet_helper_copies_are_byte_identical() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let core =
            std::fs::read(root.join("crates/thegn-core/src/test_support/ratchet.rs")).unwrap();
        let media = std::fs::read(root.join("crates/thegn-media/src/ratchet.rs")).unwrap();
        let metrics = std::fs::read(root.join("crates/thegn-metrics/src/ratchet.rs")).unwrap();
        assert_eq!(core, media, "core and media ratchet helpers drifted");
        assert_eq!(core, metrics, "core and metrics ratchet helpers drifted");
    }
}
