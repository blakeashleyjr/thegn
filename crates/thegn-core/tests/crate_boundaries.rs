//! Crate dependency boundaries (the platform-portability spec).
//!
//! `thegn-core` is substrate-agnostic: no async runtime, no terminal, no PTY,
//! no HTTP client, no forge SDK — so its logic stays pure, portable and
//! 95%-covered. The substrates are owned by specific crates and nothing else
//! may take a *direct* dependency on them. This test reads every workspace
//! member's `Cargo.toml` (normal, build and target-specific dependencies —
//! dev-dependencies are exempt, tests may use anything) and pins the rule.
//!
//! cargo-deny's `[[bans.deny]] wrappers` can't express this (it demands every
//! third-party parent be listed too); `deny.toml` keeps only the outright
//! bans (`vt100`, `russh`).

use std::collections::BTreeMap;
use std::path::Path;

/// substrate crate → workspace crates allowed to depend on it directly.
///
/// `thegn-proxy` is a DELIBERATE addition to the async/HTTP substrates (tokio,
/// reqwest, axum): it is a standalone network daemon binary (`tgproxy`) that
/// terminates HTTP from agent CLIs and streams to upstream providers — exactly
/// the role `thegn-host` and `thegn-svc` already play for their substrates, and
/// impossible to fill without a runtime and a client. The substrate-free rule
/// protects `thegn-core` (see `core_is_substrate_free` below), not every crate;
/// what matters is that the *shared logic* stays in core and the substrates stay
/// pinned to the crates that own an I/O edge. It owns no terminal/PTY substrate,
/// so it appears in no other list here.
const OWNERS: &[(&str, &[&str])] = &[
    (
        "tokio",
        &[
            "thegn-host",
            "thegn-svc",
            "thegn-proxy",
            "thegn-media",
            "gtui-app",
            "gtui-embed",
            "gtui-query",
        ],
    ),
    ("termwiz", &["thegn-host"]),
    ("portable-pty", &["thegn-host"]),
    (
        "reqwest",
        &["thegn-host", "thegn-svc", "thegn-proxy", "gtui-query"],
    ),
    ("octocrab", &["thegn-svc"]),
    ("axum", &["thegn-host", "thegn-svc", "thegn-proxy"]),
    ("alacritty_terminal", &["thegn-host"]),
    ("gix", &["thegn-svc"]),
];

/// Crates that must stay substrate-free entirely (beyond `OWNERS`): the
/// forbidden-for-everyone-but-owners set is the keys of `OWNERS`; these names
/// additionally may not appear as direct deps of `thegn-core` at all.
const CORE_FORBIDDEN: &[&str] = &[
    "tokio",
    "termwiz",
    "portable-pty",
    "reqwest",
    "octocrab",
    "axum",
    "alacritty_terminal",
    "hyper",
    "russh",
    "vt100",
    "gix",
];

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `(crate name, direct non-dev dependency names)` for every member.
fn members() -> BTreeMap<String, Vec<String>> {
    let root = workspace_root();
    let ws: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for m in ws["workspace"]["members"].as_array().unwrap() {
        let dir = root.join(m.as_str().unwrap());
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(dir.join("Cargo.toml")).unwrap()).unwrap();
        let name = manifest["package"]["name"].as_str().unwrap().to_string();
        let mut deps = Vec::new();
        let mut take = |tbl: Option<&toml::Value>| {
            if let Some(t) = tbl.and_then(|t| t.as_table()) {
                for (k, v) in t {
                    // `foo = { package = "bar" }` renames; use the real crate.
                    let real = v
                        .get("package")
                        .and_then(|p| p.as_str())
                        .unwrap_or(k)
                        .to_string();
                    deps.push(real);
                }
            }
        };
        take(manifest.get("dependencies"));
        take(manifest.get("build-dependencies"));
        if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
            for (_, t) in targets {
                take(t.get("dependencies"));
                take(t.get("build-dependencies"));
            }
        }
        out.insert(name, deps);
    }
    out
}

#[test]
fn substrates_are_only_used_by_their_owners() {
    let members = members();
    let mut problems = Vec::new();
    for (substrate, owners) in OWNERS {
        for (krate, deps) in &members {
            if deps.iter().any(|d| d == substrate) && !owners.contains(&krate.as_str()) {
                problems.push(format!(
                    "{krate} depends directly on `{substrate}`; only {owners:?} may \
                     (see crates/thegn-core/tests/crate_boundaries.rs)"
                ));
            }
        }
        // Owners are a statement of fact, not aspiration: a listed owner that
        // no longer uses the substrate should be removed.
        for owner in *owners {
            let uses = members
                .get(*owner)
                .unwrap_or_else(|| panic!("owner {owner} is not a workspace member"))
                .iter()
                .any(|d| d == substrate);
            assert!(
                uses,
                "{owner} is listed as an owner of `{substrate}` but no longer depends on it"
            );
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn core_is_substrate_free() {
    let members = members();
    let core = &members["thegn-core"];
    let bad: Vec<&String> = core
        .iter()
        .filter(|d| CORE_FORBIDDEN.contains(&d.as_str()))
        .collect();
    assert!(
        bad.is_empty(),
        "thegn-core must stay substrate-agnostic (CLAUDE.md); remove {bad:?} or put the \
         code in thegn-svc / thegn-host"
    );
    // The sanctioned leaf edges, so a change here is deliberate.
    assert!(core.iter().any(|d| d == "thegn-media"));
}

/// `[workspace.lints]` (notably `let_underscore_future = "deny"`, the gate
/// ARCHITECTURE.md §9 names) applies ONLY to members that opt in with
/// `[lints] workspace = true`. A member that forgets silently loses every
/// workspace lint — which is exactly what happened to `thegn-proxy` (THE-77 F3).
#[test]
fn every_member_inherits_workspace_lints() {
    let root = workspace_root();
    let ws: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap()).unwrap();
    let mut missing = Vec::new();
    for m in ws["workspace"]["members"].as_array().unwrap() {
        let rel = m.as_str().unwrap();
        let manifest_path = root.join(rel).join("Cargo.toml");
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        let opted_in = manifest
            .get("lints")
            .and_then(|l| l.get("workspace"))
            .and_then(toml::Value::as_bool)
            == Some(true);
        if !opted_in {
            missing.push(format!("{rel}/Cargo.toml"));
        }
    }
    assert!(
        missing.is_empty(),
        "these members do not inherit [workspace.lints] — add\n\n    [lints]\n    workspace = \
         true\n\nto each of: {missing:?}"
    );
}

#[test]
fn every_member_is_covered() {
    // A new crate must be placed: either it owns nothing (fine) or it is added
    // to OWNERS. This just pins the member list so renames surface here.
    let names: Vec<String> = members().keys().cloned().collect();
    for owner in OWNERS.iter().flat_map(|(_, o)| o.iter()) {
        assert!(names.iter().any(|n| n == owner), "unknown owner {owner}");
    }
    assert!(names.len() >= 11, "{names:?}");
}
