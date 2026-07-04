//! Cloud-init user-data for a freshly-created VPS — a **pure** `#cloud-config`
//! builder (unit-tested; no yaml dependency, the document is simple enough to
//! emit by hand). Two shapes:
//!
//! - stock image: authorize the managed key, disable password auth, and install
//!   the provisioning prerequisites (git/curl/xz for the Nix installer) plus
//!   docker (so `[env.<name>.sandbox] backend = "docker"` works inside the VPS)
//!   — everything the provision pipeline would otherwise stall on.
//! - baked snapshot (`template = "snapshot:<id>"`): keys only — the tools are
//!   already in the image, so first boot stays fast.

/// Build the `#cloud-config` user-data. `pubkey` is the managed OpenSSH public
/// key line; `install_prereqs` is false for baked snapshot images.
pub fn user_data(pubkey: &str, install_prereqs: bool) -> String {
    // ssh_pwauth off + key-only root: the instance has a public IP, so password
    // auth must never be an option. The key is also registered via the provider
    // API (belt and braces — either path alone suffices).
    let mut doc = String::from("#cloud-config\n");
    doc.push_str("ssh_pwauth: false\n");
    doc.push_str("disable_root: false\n");
    doc.push_str("ssh_authorized_keys:\n");
    doc.push_str(&format!("  - {}\n", pubkey.trim()));
    if install_prereqs {
        doc.push_str("runcmd:\n");
        // Nix-installer prerequisites; `|| true` — a transient apt failure must
        // not fail cloud-init (the provision pipeline surfaces real breakage).
        doc.push_str(
            "  - [sh, -c, \"export DEBIAN_FRONTEND=noninteractive; \
             apt-get update -qq && apt-get install -y -qq git curl xz-utils ca-certificates || true\"]\n",
        );
        // Docker for the in-VPS `backend = \"docker\"` option. Idempotent.
        doc.push_str(
            "  - [sh, -c, \"command -v docker >/dev/null 2>&1 || \
             (curl -fsSL https://get.docker.com | sh) || true\"]\n",
        );
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_image_gets_keys_prereqs_and_docker() {
        let d = user_data("ssh-ed25519 AAAA superzej", true);
        assert!(d.starts_with("#cloud-config\n"));
        assert!(d.contains("ssh_pwauth: false"), "password auth must be off");
        assert!(d.contains("  - ssh-ed25519 AAAA superzej\n"));
        assert!(d.contains("xz-utils"), "nix installer prereqs");
        assert!(d.contains("get.docker.com"), "docker for backend=docker");
    }

    #[test]
    fn snapshot_image_is_keys_only() {
        let d = user_data("ssh-ed25519 AAAA superzej", false);
        assert!(d.contains("ssh_authorized_keys"));
        assert!(
            !d.contains("runcmd"),
            "baked image installs nothing at boot"
        );
        assert!(!d.contains("docker"));
    }
}
