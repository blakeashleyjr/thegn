//! Pure content-type routing + document models for the preview pane.
//!
//! The host's preview pane routes a previewed file to a *render route* by this
//! module's [`route_for`]: plain text (tree-sitter, AF 396) and images (the
//! graphics path, AF 399) are the pre-existing routes; CSV, Jupyter, Mermaid,
//! and PDF are the document-viewer additions (AF 775). Everything here is pure
//! (no I/O, no termwiz) so the extension/sniff mapping and the CSV/Jupyter
//! document models are unit-tested in the core-coverage gate; the host owns the
//! off-loop read/rasterize and the actual rendering.

use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The render route a previewed file takes.
///
/// Routing is extension-first with a content sniff only to disambiguate the
/// text/binary fallback: a file with no (or an unknown) extension is `Text` when
/// it looks like UTF-8 text and `Unknown` (unpreviewable) when it looks binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewRoute {
    /// Plain text through the tree-sitter route (AF 396).
    Text,
    /// A raster image through the graphics route (AF 399).
    Image,
    /// A comma/tab-separated table ([`CsvTable`]).
    Csv,
    /// A Jupyter notebook ([`Notebook`]).
    Jupyter,
    /// A Mermaid diagram (rasterized to the graphics route, source as fallback).
    Mermaid,
    /// A PDF document (rasterized to the graphics route, text as fallback).
    Pdf,
    /// Not previewable (binary with no recognized type).
    Unknown,
}

/// Lowercased final extension of `path`, if any.
fn ext_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Heuristic: does `head` (the first bytes of the file) look like binary?
///
/// A NUL byte is the canonical text/binary discriminator — the same rule the
/// existing text preview uses to reject binaries.
fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

/// Map a previewed file to its [`PreviewRoute`].
///
/// `head` is a prefix of the file's bytes (may be empty when unavailable); it is
/// consulted only for the extension-less / unknown-extension fallback.
pub fn route_for(path: &Path, head: &[u8]) -> PreviewRoute {
    match ext_of(path).as_deref() {
        Some("csv" | "tsv") => PreviewRoute::Csv,
        Some("ipynb") => PreviewRoute::Jupyter,
        Some("mmd" | "mermaid") => PreviewRoute::Mermaid,
        Some("pdf") => PreviewRoute::Pdf,
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "avif") => {
            PreviewRoute::Image
        }
        // Known-text extensions never need a sniff.
        Some(
            "txt" | "md" | "markdown" | "rs" | "toml" | "json" | "yaml" | "yml" | "js" | "ts"
            | "py" | "go" | "c" | "h" | "cpp" | "hpp" | "sh" | "lock" | "cfg" | "ini" | "html"
            | "css" | "xml" | "sql",
        ) => PreviewRoute::Text,
        // No / unrecognized extension: fall back on a content sniff.
        _ => {
            if head.is_empty() || !looks_binary(head) {
                PreviewRoute::Text
            } else {
                PreviewRoute::Unknown
            }
        }
    }
}

/// The field delimiter for a CSV/TSV path: tab for `.tsv`, comma otherwise.
fn delimiter_for(path: &Path) -> u8 {
    match ext_of(path).as_deref() {
        Some("tsv") => b'\t',
        _ => b',',
    }
}

// ── CSV table model ───────────────────────────────────────────────────────────

/// A parsed, bounded CSV/TSV table: rows of string cells plus per-column display
/// widths, ready for the host to render as a scrollable grid.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CsvTable {
    /// Rows of cells. The first row is treated as the header by the renderer;
    /// this model does not itself distinguish it.
    pub rows: Vec<Vec<String>>,
    /// Column count = the widest row's field count (short rows render blanks).
    pub cols: usize,
    /// Per-column display width, each clamped to [`MAX_COL_WIDTH`].
    pub col_widths: Vec<usize>,
    /// True when parsing stopped at [`MAX_ROWS`] (more rows exist on disk).
    pub truncated: bool,
}

/// Row cap: a preview is bounded so a huge CSV never balloons memory/scrollback.
pub const MAX_ROWS: usize = 10_000;
/// Per-column display-width cap so one wide field can't dominate the grid.
pub const MAX_COL_WIDTH: usize = 40;

impl CsvTable {
    /// Parse CSV/TSV `content` for `path` (delimiter picked from the extension).
    ///
    /// RFC-4180-ish: double-quoted fields may contain the delimiter, newlines,
    /// and escaped quotes (`""`). Bounded at [`MAX_ROWS`].
    pub fn parse(path: &Path, content: &str) -> CsvTable {
        Self::parse_with_delim(content, delimiter_for(path))
    }

    fn parse_with_delim(content: &str, delim: u8) -> CsvTable {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut row: Vec<String> = Vec::new();
        // Accumulate field content as raw bytes, decoding to UTF-8 at field end:
        // the CSV structural bytes (delimiter, `"`, CR, LF) are all ASCII and so
        // never collide with a UTF-8 multibyte continuation byte.
        let mut field: Vec<u8> = Vec::new();
        let mut in_quotes = false;
        let mut truncated = false;
        let bytes = content.as_bytes();
        let mut i = 0;

        let take_field = |field: &mut Vec<u8>| -> String {
            String::from_utf8_lossy(&std::mem::take(field)).into_owned()
        };

        macro_rules! end_field {
            () => {
                row.push(take_field(&mut field))
            };
        }
        // Push the current field+row; trip `truncated` once the row cap is hit.
        macro_rules! end_row {
            () => {{
                end_field!();
                rows.push(std::mem::take(&mut row));
                if rows.len() >= MAX_ROWS {
                    truncated = true;
                }
            }};
        }

        while i < bytes.len() {
            let b = bytes[i];
            if in_quotes {
                if b == b'"' {
                    if bytes.get(i + 1) == Some(&b'"') {
                        field.push(b'"');
                        i += 2;
                        continue;
                    }
                    in_quotes = false;
                    i += 1;
                    continue;
                }
                field.push(b);
                i += 1;
                continue;
            }
            match b {
                b'"' => in_quotes = true,
                _ if b == delim => end_field!(),
                b'\n' => {
                    end_row!();
                    if truncated {
                        break;
                    }
                }
                b'\r' => {} // swallow CR (CRLF and lone CR both normalize)
                _ => field.push(b),
            }
            i += 1;
        }
        // Trailing field/row with no terminating newline.
        if !truncated && (!field.is_empty() || !row.is_empty()) {
            end_row!();
        }

        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut col_widths = vec![0usize; cols];
        for r in &rows {
            for (c, cell) in r.iter().enumerate() {
                let w = cell.chars().count().min(MAX_COL_WIDTH);
                if w > col_widths[c] {
                    col_widths[c] = w;
                }
            }
        }
        CsvTable {
            rows,
            cols,
            col_widths,
            truncated,
        }
    }

    /// Cell at `(row, col)`, or `""` when a short row has no such column.
    pub fn cell(&self, row: usize, col: usize) -> &str {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .map(String::as_str)
            .unwrap_or("")
    }
}

// ── Jupyter notebook model ────────────────────────────────────────────────────

/// A classified notebook cell kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// A code cell — the host highlights its source via the text route (AF 396).
    Code,
    /// A markdown cell — rendered as text.
    Markdown,
    /// A raw cell — rendered as text verbatim.
    Raw,
}

/// One notebook cell: its kind, joined source, and a count of image outputs
/// (which the host routes to the graphics path, AF 399).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookCell {
    pub kind: CellKind,
    pub source: String,
    /// Number of image/rich outputs the cell produced (code cells only).
    pub image_outputs: usize,
}

/// An ordered notebook: cells in document order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notebook {
    pub cells: Vec<NotebookCell>,
}

impl Notebook {
    /// Parse a `.ipynb` document. Returns `Err` on malformed JSON or a missing
    /// `cells` array; unknown cell types classify as [`CellKind::Raw`].
    pub fn parse(content: &str) -> Result<Notebook, String> {
        let v: serde_json::Value =
            serde_json::from_str(content).map_err(|e| format!("invalid notebook JSON: {e}"))?;
        let cells_json = v
            .get("cells")
            .and_then(|c| c.as_array())
            .ok_or_else(|| "notebook has no `cells` array".to_string())?;
        let mut cells = Vec::with_capacity(cells_json.len());
        for c in cells_json {
            let kind = match c.get("cell_type").and_then(|t| t.as_str()) {
                Some("code") => CellKind::Code,
                Some("markdown") => CellKind::Markdown,
                _ => CellKind::Raw,
            };
            let source = join_source(c.get("source"));
            let image_outputs = if kind == CellKind::Code {
                count_image_outputs(c.get("outputs"))
            } else {
                0
            };
            cells.push(NotebookCell {
                kind,
                source,
                image_outputs,
            });
        }
        Ok(Notebook { cells })
    }
}

/// `source` in `.ipynb` is either a string or an array of line-strings; join to
/// one string preserving order.
fn join_source(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(lines)) => lines
            .iter()
            .filter_map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Count outputs that carry an image MIME bundle (`image/png`, `image/jpeg`, …)
/// so the host knows how many graphics-route panes a cell needs.
fn count_image_outputs(v: Option<&serde_json::Value>) -> usize {
    let Some(outputs) = v.and_then(|o| o.as_array()) else {
        return 0;
    };
    outputs
        .iter()
        .filter(|o| {
            o.get("data")
                .and_then(|d| d.as_object())
                .is_some_and(|d| d.keys().any(|k| k.starts_with("image/")))
        })
        .count()
}

// ── Frontend dev-server discovery + fetch policy ─────────────────────────────

/// Maximum pane-output characters inspected in one parser call.
pub const MAX_PORT_HINT_CHARS: usize = 64 * 1024;
/// Maximum `package.json` text accepted by the pure package-script parser.
pub const MAX_PACKAGE_JSON_BYTES: usize = 1024 * 1024;

/// Where a dev-server port candidate came from, in precedence order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum PortHintSource {
    Config,
    PaneOutput,
    PackageScript,
}

impl PortHintSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::PaneOutput => "pane-output",
            Self::PackageScript => "package-script",
        }
    }
}

impl std::fmt::Display for PortHintSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One valid loopback dev-server candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PortHint {
    pub port: u16,
    /// `localhost`, `127.0.0.1`, or `::1`. Flag/config hints use `localhost`.
    pub host: String,
    pub source: PortHintSource,
}

impl PortHint {
    pub fn configured(port: u16) -> Option<Self> {
        valid_port(port).then(|| Self {
            port,
            host: "localhost".into(),
            source: PortHintSource::Config,
        })
    }

    /// Canonical HTTP URL for this candidate.
    pub fn url(&self) -> String {
        if self.host.contains(':') {
            format!("http://[{}]:{}/", self.host, self.port)
        } else {
            format!("http://{}:{}/", self.host, self.port)
        }
    }
}

fn valid_port(port: u16) -> bool {
    port != 0
}

fn parsed_port(raw: &str) -> Option<u16> {
    raw.parse::<u16>().ok().filter(|port| valid_port(*port))
}

fn captured_port(captures: &regex::Captures<'_>, input: &str) -> Option<u16> {
    let capture = captures.name("port")?;
    if input[capture.end()..]
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphanumeric())
    {
        return None;
    }
    parsed_port(capture.as_str())
}

static LOOPBACK_PORT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:^|[^A-Za-z0-9_.-])(?:https?://)?(?P<host>localhost|127\.0\.0\.1|\[::1\]):(?P<port>[0-9]{1,6})",
    )
    .expect("valid loopback-port regex")
});
static FLAG_PORT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?:^|[\s\"'`])(?:--port|-p)(?:\s*=\s*|\s+)[\"']?(?P<port>[0-9]{1,6})[\"']?"#)
        .expect("valid port-flag regex")
});
static ENV_PORT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?:^|[^A-Za-z0-9_])PORT\s*=\s*["']?(?P<port>[0-9]{1,6})["']?"#)
        .expect("valid PORT regex")
});

/// Strip terminal escape/control sequences and bound parser work.
pub fn sanitize_port_hint_text(input: &str) -> String {
    let bounded: String = input.chars().take(MAX_PORT_HINT_CHARS).collect();
    // Use the shared terminal state machine rather than a regex: OSC/DCS
    // payloads can be unterminated or truncated at the parser bound, and their
    // hidden text must never be mistaken for a visible preview hint.
    let no_ansi = crate::history::AnsiStripper::strip_str(&bounded);
    no_ansi
        .chars()
        .filter_map(|ch| match ch {
            '\n' | '\r' | '\t' => Some(ch),
            '\u{1b}' => None,
            ch if ch.is_control() => Some(' '),
            ch => Some(ch),
        })
        .collect()
}

#[derive(Debug)]
struct FoundHint {
    offset: usize,
    hint: PortHint,
}

fn parse_explicit_hints(input: &str, source: PortHintSource, urls: bool) -> Vec<PortHint> {
    let clean = sanitize_port_hint_text(input);
    let mut found = Vec::new();
    if urls {
        for captures in LOOPBACK_PORT_RE.captures_iter(&clean) {
            let Some(port) = captured_port(&captures, &clean) else {
                continue;
            };
            let host = captures
                .name("host")
                .map(|m| m.as_str().trim_matches(['[', ']']).to_ascii_lowercase())
                .unwrap_or_else(|| "localhost".into());
            found.push(FoundHint {
                offset: captures.get(0).map_or(0, |m| m.start()),
                hint: PortHint { port, host, source },
            });
        }
    }
    for regex in [&*FLAG_PORT_RE, &*ENV_PORT_RE] {
        for captures in regex.captures_iter(&clean) {
            let Some(port) = captured_port(&captures, &clean) else {
                continue;
            };
            found.push(FoundHint {
                offset: captures.get(0).map_or(0, |m| m.start()),
                hint: PortHint {
                    port,
                    host: "localhost".into(),
                    source,
                },
            });
        }
    }
    found.sort_by_key(|item| item.offset);
    let mut hints = Vec::new();
    for item in found {
        if !hints
            .iter()
            .any(|hint: &PortHint| hint.port == item.hint.port)
        {
            hints.push(item.hint);
        }
    }
    hints
}

/// Parse bounded pane output for explicit loopback URLs, port flags, or `PORT=`.
pub fn parse_port_hints(input: &str) -> Vec<PortHint> {
    parse_explicit_hints(input, PortHintSource::PaneOutput, true)
}

/// More explicit alias used by host-side pane plumbing.
pub fn parse_pane_port_hints(input: &str) -> Vec<PortHint> {
    parse_port_hints(input)
}

/// Parse only `scripts.dev` and `scripts.start` from supplied `package.json`.
///
/// Scripts are never executed. Only literal `--port`, `-p`, and `PORT=` values
/// are considered; framework defaults and arbitrary URLs are deliberately not
/// inferred.
pub fn parse_package_script_hints(package_json: &str) -> Result<Vec<PortHint>, String> {
    if package_json.len() > MAX_PACKAGE_JSON_BYTES {
        return Err(format!(
            "package.json exceeds {MAX_PACKAGE_JSON_BYTES} byte preview limit"
        ));
    }
    let value: serde_json::Value = serde_json::from_str(package_json)
        .map_err(|error| format!("invalid package.json: {error}"))?;
    let Some(scripts) = value.get("scripts").and_then(serde_json::Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut hints = Vec::new();
    for name in ["dev", "start"] {
        let Some(script) = scripts.get(name).and_then(serde_json::Value::as_str) else {
            continue;
        };
        for hint in parse_explicit_hints(script, PortHintSource::PackageScript, false) {
            if !hints
                .iter()
                .any(|existing: &PortHint| existing.port == hint.port)
            {
                hints.push(hint);
            }
        }
    }
    Ok(hints)
}

/// Alias matching the package-level operation rather than its return value.
pub fn parse_package_scripts(package_json: &str) -> Result<Vec<PortHint>, String> {
    parse_package_script_hints(package_json)
}

/// Sort by source precedence and port, removing duplicate ports deterministically.
pub fn merge_hints(hints: impl IntoIterator<Item = PortHint>) -> Vec<PortHint> {
    let mut hints: Vec<PortHint> = hints
        .into_iter()
        .filter(|hint| valid_port(hint.port))
        .collect();
    hints.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then(left.port.cmp(&right.port))
            .then(left.host.cmp(&right.host))
    });
    let mut merged = Vec::new();
    for hint in hints {
        if !merged
            .iter()
            .any(|existing: &PortHint| existing.port == hint.port)
        {
            merged.push(hint);
        }
    }
    merged
}

/// Merge all discovery sources using config → pane → package precedence.
pub fn merge_port_hints(
    configured_ports: &[u16],
    pane_output: impl IntoIterator<Item = PortHint>,
    package_scripts: impl IntoIterator<Item = PortHint>,
) -> Vec<PortHint> {
    merge_hints(
        configured_ports
            .iter()
            .filter_map(|port| PortHint::configured(*port))
            .chain(pane_output)
            .chain(package_scripts),
    )
}

/// Reachability known from event-driven pane/forward lifecycle facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PreviewStatus {
    Up,
    Down,
    Unknown,
}

impl PreviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for PreviewStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Live, memory-only target metadata shared by host projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreviewTarget {
    pub worktree: String,
    pub port: u16,
    pub url: String,
    pub source: PortHintSource,
    pub pane: Option<String>,
    pub session: Option<String>,
    pub status: PreviewStatus,
}

impl PreviewTarget {
    pub fn from_hint(worktree: impl Into<String>, hint: PortHint) -> Self {
        Self {
            worktree: worktree.into(),
            port: hint.port,
            url: hint.url(),
            source: hint.source,
            pane: None,
            session: None,
            status: PreviewStatus::Unknown,
        }
    }
}

/// Choose an active target: reachable first, then unknown, then down; within a
/// lifecycle class use source precedence and ascending port order.
pub fn select_target(targets: &[PreviewTarget]) -> Option<&PreviewTarget> {
    fn status_rank(status: PreviewStatus) -> u8 {
        match status {
            PreviewStatus::Up => 0,
            PreviewStatus::Unknown => 1,
            PreviewStatus::Down => 2,
        }
    }
    targets.iter().min_by(|left, right| {
        status_rank(left.status)
            .cmp(&status_rank(right.status))
            .then(left.source.cmp(&right.source))
            .then(left.port.cmp(&right.port))
            .then(left.url.cmp(&right.url))
    })
}

/// Parsed facts needed by the host fetcher after URL policy validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ValidatedPreviewUrl {
    pub url: String,
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub is_loopback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewUrlError {
    Invalid,
    UnsupportedScheme,
    CredentialsForbidden,
    ExternalForbidden,
}

impl std::fmt::Display for PreviewUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "invalid preview URL",
            Self::UnsupportedScheme => "preview URL must use http or https",
            Self::CredentialsForbidden => "preview URL credentials are forbidden",
            Self::ExternalForbidden => "preview URL must use localhost or a loopback address",
        })
    }
}

impl std::error::Error for PreviewUrlError {}

fn split_authority(authority: &str) -> Result<(String, Option<u16>), PreviewUrlError> {
    if authority.is_empty() || authority.contains('@') {
        return Err(if authority.contains('@') {
            PreviewUrlError::CredentialsForbidden
        } else {
            PreviewUrlError::Invalid
        });
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']').ok_or(PreviewUrlError::Invalid)?;
        if host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || matches!(byte, b':' | b'.'))
        {
            return Err(PreviewUrlError::Invalid);
        }
        let port = if suffix.is_empty() {
            None
        } else {
            let raw = suffix.strip_prefix(':').ok_or(PreviewUrlError::Invalid)?;
            Some(parsed_port(raw).ok_or(PreviewUrlError::Invalid)?)
        };
        return Ok((host.to_ascii_lowercase(), port));
    }
    if authority.matches(':').count() > 1 {
        return Err(PreviewUrlError::Invalid);
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, raw)) => (
            host,
            Some(parsed_port(raw).ok_or(PreviewUrlError::Invalid)?),
        ),
        None => (authority, None),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(PreviewUrlError::Invalid);
    }
    Ok((host.to_ascii_lowercase(), port))
}

/// Validate an initial fetch target against the localhost-only default policy.
pub fn validate_preview_url(
    url: &str,
    allow_external_urls: bool,
) -> Result<ValidatedPreviewUrl, PreviewUrlError> {
    if url.is_empty()
        || url
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
    {
        return Err(PreviewUrlError::Invalid);
    }
    let (scheme, remainder) = url.split_once("://").ok_or(PreviewUrlError::Invalid)?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return Err(PreviewUrlError::UnsupportedScheme);
    }
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let (host, port) = split_authority(&remainder[..authority_end])?;
    let is_loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1");
    if !is_loopback && !allow_external_urls {
        return Err(PreviewUrlError::ExternalForbidden);
    }
    Ok(ValidatedPreviewUrl {
        url: url.into(),
        scheme,
        host,
        port,
        is_loopback,
    })
}

/// Revalidate an absolute redirect target with exactly the initial URL policy.
/// Relative `Location` values must be resolved by the host before this call.
pub fn validate_preview_redirect(
    resolved_url: &str,
    allow_external_urls: bool,
) -> Result<ValidatedPreviewUrl, PreviewUrlError> {
    validate_preview_url(resolved_url, allow_external_urls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn routes_by_extension() {
        assert_eq!(route_for(&p("a.csv"), b""), PreviewRoute::Csv);
        assert_eq!(route_for(&p("a.tsv"), b""), PreviewRoute::Csv);
        assert_eq!(route_for(&p("nb.ipynb"), b""), PreviewRoute::Jupyter);
        assert_eq!(route_for(&p("d.mmd"), b""), PreviewRoute::Mermaid);
        assert_eq!(route_for(&p("d.mermaid"), b""), PreviewRoute::Mermaid);
        assert_eq!(route_for(&p("doc.pdf"), b""), PreviewRoute::Pdf);
        assert_eq!(route_for(&p("i.PNG"), b""), PreviewRoute::Image);
        assert_eq!(route_for(&p("i.jpeg"), b""), PreviewRoute::Image);
        assert_eq!(route_for(&p("main.rs"), b""), PreviewRoute::Text);
    }

    #[test]
    fn unknown_extension_sniffs_text_vs_binary() {
        // No extension, textual bytes → Text.
        assert_eq!(route_for(&p("README"), b"hello world"), PreviewRoute::Text);
        // No extension, empty head → Text (optimistic).
        assert_eq!(route_for(&p("README"), b""), PreviewRoute::Text);
        // Unknown extension, binary bytes → Unknown.
        assert_eq!(route_for(&p("blob.xyz"), b"a\0b"), PreviewRoute::Unknown);
        // Unknown extension, text bytes → Text.
        assert_eq!(route_for(&p("blob.xyz"), b"plain"), PreviewRoute::Text);
    }

    #[test]
    fn csv_parses_simple_grid_and_widths() {
        let t = CsvTable::parse(&p("a.csv"), "name,age\nalice,30\nbob,7\n");
        assert_eq!(t.cols, 2);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.cell(0, 0), "name");
        assert_eq!(t.cell(1, 0), "alice");
        assert_eq!(t.cell(2, 1), "7");
        // widths: col0 = max("name"=4,"alice"=5,"bob"=3)=5; col1 = max(3,2,1)=3
        assert_eq!(t.col_widths, vec![5, 3]);
        assert!(!t.truncated);
    }

    #[test]
    fn csv_handles_quotes_delimiters_and_newlines() {
        let t = CsvTable::parse(&p("a.csv"), "a,\"b,c\",\"line\none\"\n\"q\"\"x\",y,z\n");
        assert_eq!(t.cols, 3);
        assert_eq!(t.cell(0, 1), "b,c");
        assert_eq!(t.cell(0, 2), "line\none");
        assert_eq!(t.cell(1, 0), "q\"x");
    }

    #[test]
    fn csv_preserves_multibyte_utf8() {
        let t = CsvTable::parse(&p("a.csv"), "café,naïve\nüber,日本\n");
        assert_eq!(t.cell(0, 0), "café");
        assert_eq!(t.cell(0, 1), "naïve");
        assert_eq!(t.cell(1, 1), "日本");
        // width counts chars, not bytes
        assert_eq!(t.col_widths[0], 4); // "café"
    }

    #[test]
    fn tsv_uses_tab_delimiter() {
        let t = CsvTable::parse(&p("a.tsv"), "x\ty\n1\t2\n");
        assert_eq!(t.cols, 2);
        assert_eq!(t.cell(1, 1), "2");
    }

    #[test]
    fn csv_trailing_row_without_newline_and_short_rows() {
        let t = CsvTable::parse(&p("a.csv"), "a,b,c\n1,2");
        assert_eq!(t.cols, 3);
        assert_eq!(t.rows.len(), 2);
        // short row → missing column reads as ""
        assert_eq!(t.cell(1, 2), "");
    }

    #[test]
    fn csv_column_width_is_capped() {
        let wide = "x".repeat(100);
        let t = CsvTable::parse(&p("a.csv"), &format!("{wide}\n"));
        assert_eq!(t.col_widths, vec![MAX_COL_WIDTH]);
    }

    #[test]
    fn csv_row_cap_truncates() {
        let mut s = String::new();
        for i in 0..(MAX_ROWS + 50) {
            s.push_str(&format!("{i}\n"));
        }
        let t = CsvTable::parse(&p("a.csv"), &s);
        assert!(t.truncated);
        assert_eq!(t.rows.len(), MAX_ROWS);
    }

    #[test]
    fn empty_csv_is_empty_table() {
        let t = CsvTable::parse(&p("a.csv"), "");
        assert_eq!(t.cols, 0);
        assert!(t.rows.is_empty());
        assert_eq!(t.cell(0, 0), "");
    }

    #[test]
    fn notebook_orders_and_classifies_cells() {
        let nb = r##"{
            "cells": [
                {"cell_type": "markdown", "source": ["# Title\n", "text"]},
                {"cell_type": "code", "source": "print(1)", "outputs": []},
                {"cell_type": "raw", "source": "verbatim"},
                {"cell_type": "weird", "source": "x"}
            ]
        }"##;
        let n = Notebook::parse(nb).unwrap();
        assert_eq!(n.cells.len(), 4);
        assert_eq!(n.cells[0].kind, CellKind::Markdown);
        assert_eq!(n.cells[0].source, "# Title\ntext");
        assert_eq!(n.cells[1].kind, CellKind::Code);
        assert_eq!(n.cells[1].source, "print(1)");
        assert_eq!(n.cells[2].kind, CellKind::Raw);
        // unknown cell_type classifies as Raw
        assert_eq!(n.cells[3].kind, CellKind::Raw);
    }

    #[test]
    fn notebook_counts_image_outputs_on_code_cells() {
        let nb = r##"{
            "cells": [
                {"cell_type": "code", "source": "plot()", "outputs": [
                    {"output_type": "display_data", "data": {"image/png": "iVBOR..."}},
                    {"output_type": "stream", "text": "hi"},
                    {"output_type": "execute_result", "data": {"text/plain": "42"}}
                ]}
            ]
        }"##;
        let n = Notebook::parse(nb).unwrap();
        assert_eq!(n.cells[0].image_outputs, 1);
    }

    #[test]
    fn notebook_rejects_malformed() {
        assert!(Notebook::parse("not json").is_err());
        assert!(Notebook::parse(r#"{"nope": 1}"#).is_err());
    }

    #[test]
    fn port_hints_accept_explicit_loopback_grammar_and_strip_ansi() {
        let output = concat!(
            "\u{1b}[32mLocal:\u{1b}[0m http://localhost:5173/app\n",
            "network 127.0.0.1:3000\n",
            "ipv6 http://[::1]:8080/\n",
            "vite --port=4173\n",
            "other -p 9000\n",
            "PORT=7000 npm run dev\n",
        );
        let hints = parse_port_hints(output);
        assert_eq!(
            hints.iter().map(|hint| hint.port).collect::<Vec<_>>(),
            vec![5173, 3000, 8080, 4173, 9000, 7000]
        );
        assert_eq!(hints[0].host, "localhost");
        assert_eq!(hints[1].host, "127.0.0.1");
        assert_eq!(hints[2].host, "::1");
        assert!(
            hints
                .iter()
                .all(|hint| hint.source == PortHintSource::PaneOutput)
        );
    }

    #[test]
    fn port_hints_reject_external_urls_malformed_ports_and_duplicates() {
        let hints = parse_port_hints(
            "https://example.com:4444 http://localhost:0 localhost:65536 \
             localhost:5173 localhost:5173 localhost:6000oops DATABASE_PORT=9999 \
             --port nope --port 7000bad PORT=8000bad -p 42",
        );
        assert_eq!(
            hints.iter().map(|hint| hint.port).collect::<Vec<_>>(),
            vec![5173, 42]
        );
    }

    #[test]
    fn port_hints_ignore_unterminated_terminal_control_payloads() {
        for hidden in [
            "\u{1b}]0;http://localhost:6666",
            "\u{1b}Phttp://localhost:7777",
        ] {
            assert!(parse_port_hints(hidden).is_empty(), "{hidden:?}");
        }
    }

    #[test]
    fn package_scripts_parse_known_scripts_without_execution_or_defaults() {
        let package = r#"{
            "scripts": {
                "dev": "vite --port 5173 && echo done",
                "start": "PORT=3000 node server.js",
                "preview": "vite preview -p 4173",
                "lint": "echo --port 9999"
            }
        }"#;
        let hints = parse_package_scripts(package).unwrap();
        assert_eq!(
            hints.iter().map(|hint| hint.port).collect::<Vec<_>>(),
            vec![5173, 3000]
        );
        assert!(
            hints
                .iter()
                .all(|hint| hint.source == PortHintSource::PackageScript)
        );
    }

    #[test]
    fn package_script_json_edges_are_safe_and_bounded() {
        assert!(parse_package_scripts("not json").is_err());
        assert!(
            parse_package_scripts(r#"{"scripts": []}"#)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_package_scripts(r#"{"scripts":{"dev":["vite","--port","2"]}}"#)
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_package_scripts(r#"{"scripts":{"dev":"vite","start":"next"}}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_package_scripts(&" ".repeat(MAX_PACKAGE_JSON_BYTES + 1)).is_err());
    }

    #[test]
    fn hint_merge_is_precedence_ordered_stable_and_deduplicated() {
        let pane = vec![
            PortHint {
                port: 5173,
                host: "127.0.0.1".into(),
                source: PortHintSource::PaneOutput,
            },
            PortHint {
                port: 8080,
                host: "::1".into(),
                source: PortHintSource::PaneOutput,
            },
        ];
        let package = vec![
            PortHint {
                port: 5173,
                host: "localhost".into(),
                source: PortHintSource::PackageScript,
            },
            PortHint {
                port: 3000,
                host: "localhost".into(),
                source: PortHintSource::PackageScript,
            },
        ];
        let merged = merge_port_hints(&[9000, 5173], pane, package);
        assert_eq!(
            merged.iter().map(|hint| hint.port).collect::<Vec<_>>(),
            vec![5173, 9000, 8080, 3000]
        );
        assert_eq!(merged[0].source, PortHintSource::Config);
        assert_eq!(merged[2].source, PortHintSource::PaneOutput);
        assert_eq!(merged[3].source, PortHintSource::PackageScript);
    }

    #[test]
    fn target_selection_prefers_live_then_source_then_port() {
        let target = |port, source, status| PreviewTarget {
            worktree: "repo-feature".into(),
            port,
            url: format!("http://localhost:{port}/"),
            source,
            pane: None,
            session: None,
            status,
        };
        let targets = vec![
            target(3000, PortHintSource::Config, PreviewStatus::Unknown),
            target(8080, PortHintSource::PaneOutput, PreviewStatus::Up),
            target(5173, PortHintSource::PaneOutput, PreviewStatus::Up),
            target(2000, PortHintSource::Config, PreviewStatus::Down),
        ];
        assert_eq!(select_target(&targets).unwrap().port, 5173);

        let hint = PortHint::configured(9000).unwrap();
        let target = PreviewTarget::from_hint("repo", hint);
        assert_eq!(target.status, PreviewStatus::Unknown);
        assert_eq!(target.url, "http://localhost:9000/");
        assert_eq!(PreviewStatus::Down.to_string(), "down");
        assert_eq!(PortHintSource::PaneOutput.to_string(), "pane-output");
    }

    #[test]
    fn fetch_policy_accepts_only_exact_loopback_authorities_by_default() {
        for url in [
            "http://localhost:5173/",
            "https://LOCALHOST/path",
            "http://127.0.0.1:3000?q=1",
            "http://[::1]:8080/",
        ] {
            let validated = validate_preview_url(url, false).unwrap();
            assert!(validated.is_loopback, "{url}");
        }
        for url in [
            "http://example.com:5173/",
            "http://localhost.example.com:5173/",
            "http://127.0.0.2:5173/",
            "http://[::2]:5173/",
        ] {
            assert_eq!(
                validate_preview_url(url, false),
                Err(PreviewUrlError::ExternalForbidden),
                "{url}"
            );
        }
    }

    #[test]
    fn fetch_policy_external_opt_in_keeps_protocol_and_credential_guards() {
        let external = validate_preview_url("https://example.com:8443/app", true).unwrap();
        assert!(!external.is_loopback);
        assert_eq!(external.host, "example.com");
        assert_eq!(external.port, Some(8443));
        assert_eq!(
            validate_preview_url("ftp://example.com/file", true),
            Err(PreviewUrlError::UnsupportedScheme)
        );
        assert_eq!(
            validate_preview_url("http://user:pass@localhost:3000/", true),
            Err(PreviewUrlError::CredentialsForbidden)
        );
        for malformed in [
            "localhost:3000",
            "http://localhost:0/",
            "http://::1:3000/",
            "http://localhost:99999/",
            "http://localhost:3000\\evil",
            "http://localhost:3000/\r\nHost: evil",
        ] {
            assert_eq!(
                validate_preview_url(malformed, true),
                Err(PreviewUrlError::Invalid),
                "{malformed:?}"
            );
        }
    }

    #[test]
    fn redirect_targets_are_revalidated_with_the_same_policy() {
        assert!(validate_preview_redirect("http://[::1]:4000/next", false).is_ok());
        assert_eq!(
            validate_preview_redirect("https://example.com/escape", false),
            Err(PreviewUrlError::ExternalForbidden)
        );
        assert!(validate_preview_redirect("https://example.com/escape", true).is_ok());
    }
}
