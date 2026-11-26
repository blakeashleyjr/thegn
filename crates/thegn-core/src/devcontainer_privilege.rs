//! Canonical privilege requests declared by devcontainer inputs.

use std::path::PathBuf;

/// A devcontainer capability that must be covered by an explicit privilege grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeRequest {
    Privileged,
    CapAdd(String),
    SecurityOpt(String),
    Device(String),
    EngineSocket(PathBuf),
    Init,
    RunArg(String),
    Entrypoint(String),
}

/// Where a privilege request originated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestOrigin {
    RepoFile,
    Feature(String),
    ImageLabel,
    ComposeService(String),
    CreatedContainer,
}
