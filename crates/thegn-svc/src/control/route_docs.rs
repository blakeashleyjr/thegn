//! Generator for the `## Routes` table in `docs/superpowers/specs/control-api.md`
//! (THE-192): one row per HTTP route, derived from the route registry
//! ([`API_CALLS`], [`ROUTES`]), the capability catalog and `ROUTE_TYPES`, so
//! a removed or renamed route cannot leave a stale documented entry. The doc
//! carries the output between [`BEGIN`] and [`END`]; a drift test regenerates
//! it and fails when the committed text differs.

use thegn_core::capability::{SURFACE_GAPS, lookup, scope_of};

use super::route_types::route_types_for;
use super::routes::{API_CALLS, ROUTES};

/// Marker opening the generated block in the doc.
pub const BEGIN: &str = "<!-- BEGIN GENERATED ROUTES -->";
/// Marker closing the generated block in the doc.
pub const END: &str = "<!-- END GENERATED ROUTES -->";

const HEADER: &str =
    "| Route | Capability | Scope | Stability | Surfaces | Request | Response | Notes |";
const SEPARATOR: &str = "| --- | --- | --- | --- | --- | --- | --- | --- |";
const RESERVED_ROW: &str = "| `POST /v1/push/register` | `—` | `—` | `—` | — | — | — | **reserved** for push notifications; absent in v1 |";

/// Notes for the unauthenticated routes (they have no catalog row).
const OPEN_NOTES: &[(&str, &str, &str)] = &[
    ("GET", "/health", "liveness"),
    ("GET", "/pair", "static pairing redeem page"),
    (
        "POST",
        "/v1/pair",
        "redeem a pairing code (unauthenticated; possession of the single-use code is the credential)",
    ),
];

/// One table cell: a pipe would end the cell and a newline the row.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn type_cell(text: &str) -> String {
    if text == "—" {
        text.to_owned()
    } else {
        cell(&format!("`{text}`"))
    }
}

fn method_rank(method: &str) -> u8 {
    match method {
        "GET" => 0,
        "POST" => 1,
        "DELETE" => 2,
        _ => 3,
    }
}

/// `(method, path, row)` for every documented route, unsorted.
fn rows() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (cap, method, path) in API_CALLS {
        let method = if *method == "WS" { "GET" } else { *method };
        let mut paths = vec![*path];
        if *cap == "events.subscribe" {
            paths.push("/v1/events/sse");
        }
        for path in paths {
            let capability = lookup(cap)
                .unwrap_or_else(|| panic!("API_CALLS capability {cap} is not in the catalog"));
            let stability = if capability.stub.is_some() {
                "stub"
            } else if capability.deprecated.is_some() {
                "deprecated"
            } else {
                "stable"
            };
            let surfaces = capability
                .surfaces
                .names()
                .into_iter()
                .filter(|surface| {
                    !SURFACE_GAPS.iter().any(|(gap_cap, gap_surface, _)| {
                        gap_cap == cap && gap_surface.as_str() == *surface
                    })
                })
                .collect::<Vec<_>>()
                .join(" ");
            let (request, response) = route_types_for(cap)
                .unwrap_or_else(|| panic!("ROUTE_TYPES has no row for capability {cap}"));
            let mut notes = capability.summary.to_owned();
            if let Some(replacement) = capability.deprecated {
                notes.push_str(&format!(" (use {replacement})"));
            }
            let row = format!(
                "| `{method} {path}` | `{cap}` | `{}` | `{stability}` | `{surfaces}` | {} | {} | {} |",
                scope_of(capability).as_str(),
                type_cell(request),
                type_cell(response),
                cell(&notes),
            );
            out.push((method.to_owned(), path.to_owned(), row));
        }
    }
    for route in ROUTES.iter().filter(|route| route.caps.is_empty()) {
        let (method, _, notes) = OPEN_NOTES
            .iter()
            .find(|(_, path, _)| *path == route.path)
            .unwrap_or_else(|| {
                panic!("unauthenticated route {} has no OPEN_NOTES row", route.path)
            });
        let row = format!(
            "| `{method} {}` | `—` | `—` | — | — | — | — | {} |",
            route.path,
            cell(notes),
        );
        out.push(((*method).to_owned(), route.path.to_owned(), row));
    }
    out
}

/// The Routes table (header, separator, rows sorted by path then method,
/// then the reserved push row), without a trailing newline.
pub fn render_routes_table() -> String {
    let mut rows = rows();
    rows.sort_by(|a, b| (&a.1, method_rank(&a.0)).cmp(&(&b.1, method_rank(&b.0))));
    let mut lines = vec![HEADER.to_owned(), SEPARATOR.to_owned()];
    lines.extend(rows.into_iter().map(|(_, _, row)| row));
    lines.push(RESERVED_ROW.to_owned());
    lines.join("\n")
}

/// Replace the text between the generated-block markers with `\n` + `table`
/// + `\n`. `Err` unless the doc has exactly one BEGIN followed by one END.
pub fn splice_generated(doc: &str, table: &str) -> Result<String, String> {
    let begins = doc.matches(BEGIN).count();
    let ends = doc.matches(END).count();
    if begins != 1 || ends != 1 {
        return Err(format!(
            "expected exactly one {BEGIN} and one {END}, found {begins} and {ends}"
        ));
    }
    let start = doc.find(BEGIN).unwrap_or_default() + BEGIN.len();
    let end = doc.find(END).unwrap_or_default();
    if end < start {
        return Err(format!("{END} comes before {BEGIN}"));
    }
    Ok(format!("{}\n{table}\n{}", &doc[..start], &doc[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/superpowers/specs/control-api.md")
    }

    /// Column-padding-insensitive form: treefmt re-aligns Markdown tables, so
    /// trim every cell of every table row, collapse separator rows and ignore
    /// blank lines (the formatter pads the markers with them).
    fn normalize(text: &str) -> String {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                if !line.trim_start().starts_with('|') {
                    return line.to_owned();
                }
                line.split('|')
                    .map(|cell| {
                        let cell = cell.trim();
                        if !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':') {
                            "---"
                        } else {
                            cell
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn generated_routes_table_matches_the_committed_doc() {
        let path = doc_path();
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let spliced = splice_generated(&doc, &render_routes_table())
            .unwrap_or_else(|error| panic!("control-api.md markers: {error}"));
        if std::env::var("THEGN_UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
            std::fs::write(&path, &spliced)
                .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
            return;
        }
        assert_eq!(
            normalize(&spliced),
            normalize(&doc),
            "the Routes table in control-api.md is stale; regenerate with \
             THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc control::route_docs"
        );
    }

    #[test]
    fn every_api_call_and_open_route_is_rendered_once() {
        let mut rendered: Vec<(String, String)> = render_routes_table()
            .lines()
            .filter(|line| line.starts_with("| `"))
            .map(|line| {
                let route = line
                    .split('|')
                    .nth(1)
                    .unwrap_or_default()
                    .trim()
                    .trim_matches('`');
                let (method, path) = route.split_once(' ').unwrap_or_default();
                (method.to_owned(), path.to_owned())
            })
            .collect();
        rendered.sort();
        let mut expected: Vec<(String, String)> = API_CALLS
            .iter()
            .map(|(_, method, path)| {
                (
                    if *method == "WS" { "GET" } else { *method }.to_owned(),
                    (*path).to_owned(),
                )
            })
            .collect();
        for (method, path) in [
            ("GET", "/v1/events/sse"),
            ("GET", "/health"),
            ("GET", "/pair"),
            ("POST", "/v1/pair"),
            ("POST", "/v1/push/register"),
        ] {
            expected.push((method.to_owned(), path.to_owned()));
        }
        expected.sort();
        assert_eq!(rendered, expected);
    }

    #[test]
    fn renderer_escapes_pipes_and_keeps_one_line_per_row() {
        let out = cell("a|b\nc");
        assert!(out.contains("a\\|b"), "{out}");
        assert!(!out.contains('\n'), "{out}");
    }

    #[test]
    fn splice_generated_requires_exactly_one_marker_pair() {
        assert!(splice_generated("no markers", "t").is_err());
        assert!(splice_generated(&format!("{BEGIN}{END}{BEGIN}{END}"), "t").is_err());
        assert!(splice_generated(&format!("{END} x {BEGIN}"), "t").is_err());
        assert_eq!(
            splice_generated(&format!("a{BEGIN}old{END}b"), "t").unwrap(),
            format!("a{BEGIN}\nt\n{END}b")
        );
    }
}
