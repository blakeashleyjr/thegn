//! Apple `container` internal networks (WS05-P0-02).
//!
//! Apple's `container` CLI has neither a `host` nor a `none` network keyword:
//! `--network none` would name a network called `none`, and there is no
//! host-network mode at all. The launch gate refuses both (`unexpressible`
//! refusal class `network-unexpressible`), so no argv ever carries either
//! keyword for Apple.
//!
//! The one offline-shaped mode Apple does offer is an *internal* network
//! (`container network create --internal`): a host-only vmnet that keeps the
//! container off the internet **but still reaches services bound on the Mac**.
//! That caveat is why `network = "none"` stays refused until on-device evidence
//! (V-AC-02 from WS05-P0-01) shows the internal network reaches neither the
//! internet nor the Mac; the positive branch of this item maps `none` onto this
//! network and credits `Offline`, the negative branch (today) only ships the
//! machinery.
//!
//! The machinery exists either way because the Apple relay path (WS04-P4-04,
//! WS05-P4-02) joins this network. Everything here is argv and data only — no
//! probe, no policy — so it is unit-testable on Linux. The one child it spawns
//! goes through `sandbox::run_local_output`, which bounds it by `PROBE_TIMEOUT`
//! and owns/waits for it.

use crate::sandbox::{Backend, SandboxSpec};

/// The internal network thegn creates for Apple's `container`.
pub const INTERNAL_NETWORK: &str = "thegn-none";

/// `container network create --internal thegn-none`.
///
/// Apple's `network create` fails when the network already exists, so callers
/// list first ([`ensure_internal_network`]) rather than use this argv alone.
pub fn ensure_internal_network_argv() -> [&'static str; 5] {
    [
        "container",
        "network",
        "create",
        "--internal",
        INTERNAL_NETWORK,
    ]
}

/// `container network ls --format json`, the listing read before a create.
fn network_ls_argv() -> [&'static str; 5] {
    ["container", "network", "ls", "--format", "json"]
}

/// Parse `container network ls --format json` into network names.
///
/// Apple's JSON shape is unverified until V-AC-02 supplies a fixture, so this
/// accepts the two shapes a `network ls` produces elsewhere: an array of
/// names, or an array of objects carrying a `name`/`Name` field. Any other
/// shape — invalid JSON included — parses to an empty list, and
/// [`ensure_internal_network`] then *creates* instead of assuming presence.
/// An unparseable listing is never proof that the network exists.
pub fn parse_network_ls(json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item {
            serde_json::Value::String(name) => Some(name.clone()),
            serde_json::Value::Object(fields) => fields
                .get("name")
                .or_else(|| fields.get("Name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// Whether one `network ls` run lists [`INTERNAL_NETWORK`].
///
/// A spawn failure, a non-zero exit or a malformed listing is "not present":
/// the caller then creates, which is the fail-closed direction here.
fn internal_network_present<F>(run: &mut F) -> bool
where
    F: FnMut(&[&'static str]) -> Option<String> + ?Sized,
{
    run(&network_ls_argv())
        .map(|json| parse_network_ls(&json))
        .unwrap_or_default()
        .iter()
        .any(|name| name == INTERNAL_NETWORK)
}

/// Create [`INTERNAL_NETWORK`] on demand, idempotently.
///
/// `run` executes one Apple `container` argv and yields its stdout only when the
/// child exited 0; `None` covers a non-zero exit, a spawn failure and a timeout
/// — the three-state shape `sandbox::run_local_output` returns. It is a
/// parameter so the whole decision is unit-tested without a Mac or the CLI.
///
/// Returns `Ok(true)` when this call created the network and `Ok(false)` when it
/// was already there. A failed create is an error only when a follow-up listing
/// still lacks the network: two worktrees can ensure at the same time, and the
/// loser's `network create` fails with "already exists", which is success here.
pub fn ensure_internal_network<F>(run: &mut F) -> anyhow::Result<bool>
where
    F: FnMut(&[&'static str]) -> Option<String> + ?Sized,
{
    if internal_network_present(&mut *run) {
        return Ok(false);
    }
    if run(&ensure_internal_network_argv()).is_some() {
        return Ok(true);
    }
    if internal_network_present(&mut *run) {
        return Ok(false);
    }
    anyhow::bail!(
        "`container network create --internal {INTERNAL_NETWORK}` failed and the network is still \
         absent; refusing to rely on it"
    )
}

/// The Apple `ensure` arm: create [`INTERNAL_NETWORK`] before a launch that will
/// join it. A no-op for every other backend, so a caller can invoke it
/// unconditionally before composing argv.
///
/// Every child is an owned, bounded `sandbox::run_local_output` call, so a
/// wedged `container` service fails the probe on `PROBE_TIMEOUT` instead of
/// hanging the launch. Under the negative branch nothing on the launch path
/// calls this yet: the relay path (WS04-P4-04, WS05-P4-02) is its first caller.
pub fn ensure_apple_internal_network(spec: &SandboxSpec) -> anyhow::Result<()> {
    if spec.backend != Backend::Apple {
        return Ok(());
    }
    let mut run = run_container;
    ensure_internal_network(&mut run).map(|_created| ())
}

/// One Apple `container` invocation through the owned, bounded child runner.
fn run_container(args: &[&'static str]) -> Option<String> {
    crate::sandbox::run_local_output(&[], args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_network_create_argv() {
        assert_eq!(
            ensure_internal_network_argv(),
            ["container", "network", "create", "--internal", "thegn-none"]
        );
        assert_eq!(INTERNAL_NETWORK, "thegn-none");
    }

    #[test]
    fn network_ls_parser_golden() {
        // The object shape (a name plus opaque metadata). V-AC-02 will replace
        // this fixture with the real `container network ls --format json` once
        // the on-device evidence exists; the parser must accept it before then.
        let objects = r#"[
            {"name": "default", "id": "sha256:1111"},
            {"name": "thegn-none", "id": "sha256:2222"}
        ]"#;
        assert_eq!(
            parse_network_ls(objects),
            vec!["default".to_string(), "thegn-none".to_string()]
        );

        // The bare-name shape.
        assert_eq!(
            parse_network_ls(r#"["default", "thegn-none"]"#),
            vec!["default".to_string(), "thegn-none".to_string()]
        );

        // Anything else is "not listed", never an accidental match.
        assert!(parse_network_ls("").is_empty());
        assert!(parse_network_ls("not json").is_empty());
        assert!(parse_network_ls(r#"{"networks": ["thegn-none"]}"#).is_empty());
        assert!(parse_network_ls(r#"[{"id": "sha256:2222"}]"#).is_empty());
    }

    #[test]
    fn internal_network_listed_is_not_recreated() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let mut run = |args: &[&'static str]| {
            calls.push(args.iter().map(|arg| (*arg).to_string()).collect());
            Some(r#"[{"name": "thegn-none"}]"#.to_string())
        };
        assert!(!ensure_internal_network(&mut run).expect("listing succeeds"));
        assert_eq!(
            calls.len(),
            1,
            "the listing alone must short-circuit create"
        );
        assert_eq!(calls[0], ["container", "network", "ls", "--format", "json"]);
    }

    #[test]
    fn internal_network_create_then_listed_is_idempotent() {
        let mut created = false;
        let mut run = |args: &[&'static str]| match args[2] {
            "ls" => Some(if created {
                r#"[{"name": "thegn-none"}]"#.to_string()
            } else {
                "[]".to_string()
            }),
            "create" => {
                created = true;
                Some(String::new())
            }
            other => panic!("unexpected apple network verb {other}"),
        };
        assert!(ensure_internal_network(&mut run).expect("creates the network"));
        // The second ensure sees the network and does not create again.
        assert!(!ensure_internal_network(&mut run).expect("already present"));
    }

    #[test]
    fn internal_network_create_race_is_not_an_error() {
        // The create loses ("already exists", non-zero) but a concurrent
        // ensure has made the network visible: success, not a refusal.
        let mut listed = false;
        let mut run = |args: &[&'static str]| match args[2] {
            "ls" => {
                let answer = Some(if listed {
                    r#"[{"name": "thegn-none"}]"#.to_string()
                } else {
                    "[]".to_string()
                });
                listed = true;
                answer
            }
            "create" => None,
            other => panic!("unexpected apple network verb {other}"),
        };
        assert!(!ensure_internal_network(&mut run).expect("race is success"));
    }

    #[test]
    fn internal_network_unparseable_listing_creates() {
        let mut saw_create = false;
        let mut run = |args: &[&'static str]| match args[2] {
            "ls" => Some("container: unexpected output".to_string()),
            "create" => {
                saw_create = true;
                Some(String::new())
            }
            other => panic!("unexpected apple network verb {other}"),
        };
        assert!(ensure_internal_network(&mut run).expect("creates the network"));
        assert!(
            saw_create,
            "an unparseable listing must create, not succeed"
        );
    }

    #[test]
    fn internal_network_failed_create_is_an_error() {
        let mut run = |args: &[&'static str]| match args[2] {
            "ls" => Some("[]".to_string()),
            "create" => None,
            other => panic!("unexpected apple network verb {other}"),
        };
        let error = ensure_internal_network(&mut run).expect_err("absent network is an error");
        assert!(error.to_string().contains(INTERNAL_NETWORK));
    }

    #[test]
    fn apple_ensure_arm_is_a_noop_off_apple() {
        // No child is spawned for a non-Apple spec (the test would fail loudly
        // if `container` were invoked here: the binary is absent on Linux).
        let mut spec = SandboxSpec::default();
        spec.backend = Backend::Podman;
        assert!(ensure_apple_internal_network(&spec).is_ok());
    }
}
