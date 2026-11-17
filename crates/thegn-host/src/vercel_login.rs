//! `model_credentials = "upload"`: read the host's claude/codex login files
//! (and only those) for the `vercel-exec` TTY bridge to upload.
//!
//! Spine stub (b156 lane 1): the signature is fixed; lane 4 fills the body.
//! Until then nothing is read, so nothing is uploaded.

/// The login payloads for `agents`, plus one reason line per skipped agent.
#[allow(dead_code)] // spine stub, filled by lane 4
pub(crate) fn read_host_logins(
    agents: &[String],
) -> (Vec<thegn_svc::vercel::login::LoginPayload>, Vec<String>) {
    let skipped = agents
        .iter()
        .map(|agent| format!("{agent}: model_credentials = upload is not available in this build"))
        .collect();
    (Vec::new(), skipped)
}
