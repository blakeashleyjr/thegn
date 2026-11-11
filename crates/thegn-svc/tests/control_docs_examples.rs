//! The hand-written control API examples and transport guidance are exercised:
//! `docs/superpowers/specs/control-api-examples.md` request bodies must
//! deserialize into the wire types they name and target registered routes, and
//! `control-api.md` must keep its transport matrix (THE-192).

use thegn_core::config_daemon::ServeExposure;
use thegn_core::control_error::ControlErrorCode;
use thegn_svc::control::ErrorBody;
use thegn_svc::control::http::{CommitBody, InputBody, PairBody, ResizeBody, StageBody};
use thegn_svc::control::routes::{API_CALLS, ROUTES};

/// The unauthenticated routes serve no capability, so they are not in
/// `API_CALLS`; their methods are pinned here and their rows asserted in ROUTES.
const OPEN_ROUTES: &[(&str, &str)] = &[("GET", "/health"), ("GET", "/pair"), ("POST", "/v1/pair")];

fn spec(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/superpowers/specs")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// `(tag, json body)` for every `<!-- example: TAG -->` followed by a json fence.
fn examples() -> Vec<(String, String)> {
    let doc = spec("control-api-examples.md");
    let mut out = Vec::new();
    let mut lines = doc.lines();
    while let Some(line) = lines.next() {
        let Some(tag) = line
            .trim()
            .strip_prefix("<!-- example:")
            .and_then(|rest| rest.strip_suffix("-->"))
        else {
            continue;
        };
        let tag = tag.trim().to_owned();
        let mut body = String::new();
        let mut in_fence = false;
        for line in lines.by_ref() {
            if !in_fence {
                if line.trim() == "```json" {
                    in_fence = true;
                } else if !line.trim().is_empty() {
                    panic!("example `{tag}` is not followed by a json fence");
                }
            } else if line.trim() == "```" {
                break;
            } else {
                body.push_str(line);
                body.push('\n');
            }
        }
        assert!(in_fence, "example `{tag}` has no json fence");
        out.push((tag, body));
    }
    out
}

/// `(method, path, type)` for the examples that tag a route.
fn route_examples() -> Vec<(String, String, String, String)> {
    examples()
        .into_iter()
        .filter(|(tag, _)| tag != "error envelope")
        .map(|(tag, body)| {
            let (route, ty) = tag
                .split_once(" as ")
                .unwrap_or_else(|| panic!("example tag `{tag}` must be `METHOD /path as Type`"));
            let (method, path) = route
                .split_once(' ')
                .unwrap_or_else(|| panic!("example route `{route}` must be `METHOD /path`"));
            (
                method.to_owned(),
                path.to_owned(),
                ty.trim().to_owned(),
                body,
            )
        })
        .collect()
}

#[test]
fn examples_deserialize_into_their_wire_types() {
    let examples = route_examples();
    assert!(examples.len() >= 5, "expected at least 5 route examples");
    for (method, path, ty, body) in &examples {
        let result = match ty.as_str() {
            "PairBody" => serde_json::from_str::<PairBody>(body).map(|_| ()),
            "InputBody" => serde_json::from_str::<InputBody>(body).map(|_| ()),
            "ResizeBody" => serde_json::from_str::<ResizeBody>(body).map(|_| ()),
            "StageBody" => serde_json::from_str::<StageBody>(body).map(|_| ()),
            "CommitBody" => serde_json::from_str::<CommitBody>(body).map(|_| ()),
            other => panic!("example {method} {path} names unknown wire type {other}"),
        };
        if let Err(error) = result {
            panic!("example {method} {path} does not parse as {ty}: {error}");
        }
    }
}

#[test]
fn example_routes_are_registered_with_the_documented_method() {
    for (method, path, _, _) in route_examples() {
        let mut registered: Vec<&str> = API_CALLS
            .iter()
            .filter(|(_, _, call_path)| *call_path == path)
            .map(|(_, call_method, _)| *call_method)
            .collect();
        for (open_method, open_path) in OPEN_ROUTES {
            if *open_path == path {
                assert!(
                    ROUTES
                        .iter()
                        .any(|route| route.path == path && route.caps.is_empty()),
                    "open route {path} has no capability-free ROUTES row"
                );
                registered.push(open_method);
            }
        }
        assert!(
            registered.contains(&method.as_str()),
            "example {method} {path} is not registered (methods for that path: {registered:?})"
        );
    }
}

#[test]
fn error_example_uses_a_known_code() {
    let examples = examples();
    let (_, body) = examples
        .iter()
        .find(|(tag, _)| tag == "error envelope")
        .expect("an `error envelope` example exists");
    let value: serde_json::Value = serde_json::from_str(body).expect("error example is json");
    assert!(value["error"].is_string(), "error must be a string");
    let code = value["code"].as_str().expect("code must be a string");
    assert!(
        ControlErrorCode::ALL
            .iter()
            .any(|known| known.as_str() == code),
        "unknown error code {code}"
    );
    let envelope: ErrorBody = serde_json::from_str(body).expect("example is an ErrorBody");
    assert_eq!(envelope.code.as_str(), code);
}

#[test]
fn transport_matrix_names_every_exposure() {
    let doc = spec("control-api.md");
    let heading = "## Transports and access modes";
    let section = doc
        .split(heading)
        .nth(1)
        .unwrap_or_else(|| panic!("control-api.md has a {heading} section"))
        .split("\n## ")
        .next()
        .expect("section has a boundary");
    for exposure in [
        ServeExposure::SafeLoopback,
        ServeExposure::TlsTerminated,
        ServeExposure::Tunnel,
        ServeExposure::UnsafePlaintext,
    ] {
        let name = format!("`{}`", exposure.as_str());
        assert!(section.contains(&name), "transport matrix lacks {name}");
    }
    let lower = section.to_lowercase();
    for word in ["local-admin", "scoped token", "unsupported"] {
        assert!(lower.contains(word), "transport matrix lacks `{word}`");
    }
}

#[test]
fn scope_write_rustdoc_has_no_browser_claim() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/thegn-core/src/control.rs");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let lines: Vec<&str> = source.lines().collect();
    let index = lines
        .iter()
        .position(|line| line.trim() == "Write,")
        .expect("Scope has a Write variant");
    let docs: Vec<&str> = lines[..index]
        .iter()
        .rev()
        .take_while(|line| line.trim_start().starts_with("///"))
        .copied()
        .collect();
    assert!(!docs.is_empty(), "Scope::Write has a doc comment");
    for line in docs {
        assert!(
            !line.contains("preview browser"),
            "stale browser claim on Scope::Write: {line}"
        );
    }
}
