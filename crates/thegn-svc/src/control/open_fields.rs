//! The daemon-skew contract for security-relevant [`OpenSpec`](super::OpenSpec)
//! fields (lead ruling L1; contract landed by WS00-P1-11).
//!
//! `OpenSpec` has no `deny_unknown_fields`, so a pane daemon older than the UI
//! silently drops a field it does not know. A field that carries a security
//! property (an env scrub, an egress lease, a shell intent) must therefore never
//! fail open against an older daemon. One rule covers every such field:
//!
//! 1. **Echo.** The daemon adds a field's name to the open reply's
//!    [`SessionInfo::honoured`](super::SessionInfo::honoured) only after it has
//!    applied that field to the
//!    spawn. A daemon older than the field's owner item never lists it, and a
//!    reply from a daemon older than WS00-P1-11 has no `honoured` at all
//!    (deserialised as empty).
//! 2. **Refusal.** Every client path that opens a session goes through
//!    [`ControlClient::open`](super::client::ControlClient::open), which runs
//!    [`check_honoured`] over
//!    [`OpenSpec::security_fields`](super::OpenSpec::security_fields). On a miss it kills
//!    the session it just opened and returns the [`DaemonSkew`] refusal
//!    (id `daemon-skew`): "the running pane daemon predates `<fields>`; restart
//!    it". A doctor line or a docs note is not an alternative to the refusal.
//! 3. **Scope.** In-process panes (`[daemon] enabled = false`) apply the fields
//!    directly; there is no skew to check. The remote mux HELLO `features` keep
//!    only transport capabilities that are not `OpenSpec` fields.
//! 4. **Defence in depth, not the guarantee.** An older daemon spawns before it
//!    replies, so the refusal kills the payload after a brief start. Field
//!    owners may add an extra guard an older daemon obeys before exec (WS02's
//!    `env -u NAME` prefix, WS09's empty `argv` with `shell`); neither replaces
//!    rule 2.
//! 5. **Classification.** Every `OpenSpec` property is in
//!    [`PRE_PROGRAMME_FIELDS`], in [`OpenField::ALL`] (by name), or in
//!    [`NON_SECURITY_FIELDS`] with a reason; a test enforces it.
//!
//! Owners after WS00: each field's owner item adds the `OpenSpec` field, maps it
//! in `OpenSpec::security_fields`, echoes its name from the daemon's open
//! handler, and adds the test `older_daemon_<field>_refused`: `env_scrub`
//! (WS02-P4-15), `egress_lease` (WS04-P2-09), `shell` (WS09-P5-05).

/// A security-relevant `OpenSpec` extension, named by its `OpenSpec` field.
/// Stub (WS00-P1-11): no such field exists on `OpenSpec` yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpenField {
    /// `env_scrub` (WS02-P4-15): the backend's env removal/clearing.
    EnvScrub,
    /// `egress_lease` (WS04-P2-09): the egress lease the session is bound to.
    EgressLease,
    /// `shell` (WS09-P5-05): a shell intent resolved by the daemon.
    Shell,
}

impl OpenField {
    /// Every security-relevant field, in a stable order.
    pub const ALL: [OpenField; 3] = [
        OpenField::EnvScrub,
        OpenField::EgressLease,
        OpenField::Shell,
    ];

    /// The field's `OpenSpec` property name, as echoed in `honoured`.
    pub const fn as_str(self) -> &'static str {
        match self {
            OpenField::EnvScrub => "env_scrub",
            OpenField::EgressLease => "egress_lease",
            OpenField::Shell => "shell",
        }
    }
}

/// The `OpenSpec` properties that existed before the sandbox + remote
/// programme (main at `cbf36fe3a`). None carries a programme security property.
pub const PRE_PROGRAMME_FIELDS: &[&str] = &[
    "argv",
    "cwd",
    "env",
    "rows",
    "cols",
    "worktree",
    "agent",
    "adopt",
    "already_capped",
    "automation_origin",
];

/// `OpenSpec` properties added since the programme began that carry no
/// security property, each with the reason. A new property must be listed
/// here or in [`OpenField::ALL`].
pub const NON_SECURITY_FIELDS: &[(&str, &str)] = &[];

/// The refusal returned when the daemon did not echo a security-relevant field
/// the client sent: the running daemon predates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSkew {
    /// The sent fields the daemon did not echo, in the order they were sent.
    pub missing: Vec<OpenField>,
}

impl DaemonSkew {
    /// The stable refusal id.
    pub const ID: &'static str = "daemon-skew";
}

impl std::fmt::Display for DaemonSkew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fields = self
            .missing
            .iter()
            .map(|field| format!("`{}`", field.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "{}: the running pane daemon predates {fields}; restart it",
            Self::ID
        )
    }
}

impl std::error::Error for DaemonSkew {}

/// Pure: every field in `sent` must appear in the daemon's `honoured` echo.
/// Names in `honoured` that the client did not send (or does not know) are
/// ignored.
pub fn check_honoured(sent: &[OpenField], honoured: &[String]) -> Result<(), DaemonSkew> {
    let mut missing: Vec<OpenField> = Vec::new();
    for field in sent {
        if !honoured.iter().any(|name| name == field.as_str()) && !missing.contains(field) {
            missing.push(*field);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(DaemonSkew { missing })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{OpenSpec, SessionInfo};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn open_field_names_are_stable() {
        let names: Vec<&str> = OpenField::ALL.iter().map(|f| f.as_str()).collect();
        assert_eq!(names, ["env_scrub", "egress_lease", "shell"]);
    }

    #[test]
    fn check_honoured_passes_when_nothing_sent() {
        assert_eq!(check_honoured(&[], &[]), Ok(()));
        assert_eq!(check_honoured(&[], &names(&["env_scrub"])), Ok(()));
        // WS00: no OpenSpec sets a security field, so every open passes.
        assert!(OpenSpec::default().security_fields().is_empty());
    }

    #[test]
    fn check_honoured_refuses_missing_field() {
        let err = check_honoured(
            &[OpenField::EnvScrub, OpenField::EgressLease],
            &names(&["env_scrub"]),
        )
        .unwrap_err();
        assert_eq!(err.missing, vec![OpenField::EgressLease]);
        assert_eq!(
            err.to_string(),
            "daemon-skew: the running pane daemon predates `egress_lease`; restart it"
        );

        // A reply from a daemon older than the echo: nothing is honoured.
        let err = check_honoured(&OpenField::ALL, &[]).unwrap_err();
        assert_eq!(err.missing, OpenField::ALL.to_vec());
        assert!(
            err.to_string()
                .contains("`env_scrub`, `egress_lease`, `shell`")
        );

        // A field sent twice is reported once.
        let err = check_honoured(&[OpenField::Shell, OpenField::Shell], &[]).unwrap_err();
        assert_eq!(err.missing, vec![OpenField::Shell]);
    }

    #[test]
    fn check_honoured_ignores_unknown_names() {
        assert_eq!(
            check_honoured(
                &[OpenField::Shell],
                &names(&["shell", "future_field", "", "SHELL"])
            ),
            Ok(())
        );
        assert!(check_honoured(&[OpenField::Shell], &names(&["SHELL"])).is_err());
    }

    #[test]
    fn session_info_without_honoured_deserialises_empty() {
        // An open reply from a daemon that predates WS00-P1-11.
        let info = SessionInfo {
            id: "s1".into(),
            program: "bash".into(),
            ..Default::default()
        };
        let value = serde_json::to_value(&info).unwrap();
        assert!(
            value.get("honoured").is_none(),
            "an empty echo is never serialised, so existing replies are byte-identical"
        );
        let back: SessionInfo = serde_json::from_value(value).unwrap();
        assert!(back.honoured.is_empty());
        assert_eq!(back, info);

        let echoed = SessionInfo {
            honoured: names(&["egress_lease"]),
            ..info
        };
        let value = serde_json::to_value(&echoed).unwrap();
        assert_eq!(value["honoured"], serde_json::json!(["egress_lease"]));
        assert_eq!(
            serde_json::from_value::<SessionInfo>(value).unwrap(),
            echoed
        );
    }

    #[test]
    fn open_spec_fields_are_classified() {
        let schema = schemars::schema_for!(OpenSpec);
        let value = serde_json::to_value(&schema).unwrap();
        let properties = value["properties"]
            .as_object()
            .expect("OpenSpec schema has properties");
        assert!(!properties.is_empty());
        for property in properties.keys() {
            let classified = PRE_PROGRAMME_FIELDS.contains(&property.as_str())
                || OpenField::ALL.iter().any(|f| f.as_str() == property)
                || NON_SECURITY_FIELDS
                    .iter()
                    .any(|(name, reason)| name == property && !reason.is_empty());
            assert!(
                classified,
                "OpenSpec.{property} is unclassified: add it to OpenField (and echo it \
                 from the daemon) or to NON_SECURITY_FIELDS with a reason"
            );
        }
    }
}
