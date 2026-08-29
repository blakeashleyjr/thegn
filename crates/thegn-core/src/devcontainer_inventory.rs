//! Single source of truth for devcontainer field handling.

use std::collections::BTreeSet;

/// How a devcontainer field is handled by the core contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldDisposition {
    Applied,
    Refused,
    Reserved,
    EditorOnly,
    Unknown,
}

/// Field names found in a source document, grouped for doctor and overlay
/// reporting. Every list is sorted and deduplicated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldInventory {
    pub applied: Vec<String>,
    pub refused: Vec<String>,
    pub reserved: Vec<String>,
    pub editor_only: Vec<String>,
    pub unknown: Vec<String>,
}

impl FieldInventory {
    /// Warning lines for fields that are not silently applied.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        warnings.extend(self.refused.iter().map(|key| {
            format!("devcontainer: `{key}` refused by design (would weaken sandbox isolation)")
        }));
        warnings.extend(
            self.reserved
                .iter()
                .map(|key| format!("devcontainer: `{key}` is reserved and not applied")),
        );
        warnings.extend(
            self.unknown.iter().map(|key| {
                format!("devcontainer: unknown key `{key}` is reserved and not applied")
            }),
        );
        warnings
    }
}

/// The exhaustive recognized-key table. Keys parsed into the normalized model
/// are still listed as applied even when a later phase owns their execution.
pub fn disposition(key: &str) -> FieldDisposition {
    match key {
        "image"
        | "build"
        | "dockerComposeFile"
        | "dockerFile"
        | "dockerfile"
        | "context"
        | "service"
        | "runServices"
        | "features"
        | "overrideFeatureInstallOrder"
        | "mounts"
        | "forwardPorts"
        | "containerEnv"
        | "remoteEnv"
        | "workspaceFolder"
        | "initializeCommand"
        | "onCreateCommand"
        | "updateContentCommand"
        | "postCreateCommand"
        | "postStartCommand"
        | "postAttachCommand" => FieldDisposition::Applied,
        "privileged" | "capAdd" | "securityOpt" | "runArgs" | "init" => FieldDisposition::Refused,
        "hostRequirements"
        | "portsAttributes"
        | "otherPortsAttributes"
        | "waitFor"
        | "userEnvProbe"
        | "shutdownAction"
        | "updateRemoteUID"
        | "updateRemoteUserUID"
        | "secrets"
        | "workspaceMount"
        | "overrideCommand"
        | "remoteUser"
        | "containerUser"
        | "dockerComposeOverrideFile"
        | "remoteUserUID" => FieldDisposition::Reserved,
        "customizations" => FieldDisposition::EditorOnly,
        _ => FieldDisposition::Unknown,
    }
}

/// Classify source keys exactly once, preserving lexicographic order and
/// emitting no duplicate warning for aliases repeated in a malformed document.
pub fn classify_keys<'a>(keys: impl IntoIterator<Item = &'a String>) -> FieldInventory {
    let mut out = FieldInventory::default();
    let mut seen = BTreeSet::new();
    for key in keys {
        if !seen.insert(key.as_str()) {
            continue;
        }
        match disposition(key) {
            FieldDisposition::Applied => out.applied.push(key.clone()),
            FieldDisposition::Refused => out.refused.push(key.clone()),
            FieldDisposition::Reserved => out.reserved.push(key.clone()),
            FieldDisposition::EditorOnly => out.editor_only.push(key.clone()),
            FieldDisposition::Unknown => out.unknown.push(key.clone()),
        }
    }
    out
}

/// All keys known by the parser/inventory contract. The test below is the
/// ratchet: adding a parser field without adding a disposition is impossible
/// to miss.
pub const RECOGNIZED_KEYS: &[&str] = &[
    "image",
    "dockerFile",
    "dockerfile",
    "context",
    "build",
    "dockerComposeFile",
    "service",
    "runServices",
    "features",
    "overrideFeatureInstallOrder",
    "mounts",
    "forwardPorts",
    "containerEnv",
    "remoteEnv",
    "workspaceFolder",
    "initializeCommand",
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
    "privileged",
    "capAdd",
    "securityOpt",
    "runArgs",
    "init",
    "hostRequirements",
    "portsAttributes",
    "otherPortsAttributes",
    "waitFor",
    "userEnvProbe",
    "shutdownAction",
    "updateRemoteUID",
    "updateRemoteUserUID",
    "secrets",
    "workspaceMount",
    "overrideCommand",
    "remoteUser",
    "containerUser",
    "customizations",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognized_table_is_exhaustive_and_editor_only_is_silent() {
        for key in RECOGNIZED_KEYS {
            assert_ne!(disposition(key), FieldDisposition::Unknown, "{key}");
        }
        let keys = RECOGNIZED_KEYS
            .iter()
            .map(|key| (*key).to_string())
            .collect::<BTreeSet<_>>();
        let inventory = classify_keys(&keys);
        assert!(
            inventory
                .warnings()
                .iter()
                .all(|warning| !warning.contains("customizations"))
        );
    }
}
