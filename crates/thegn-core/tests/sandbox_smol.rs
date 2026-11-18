//! `smol` is a reserved backend kind (WS01-P3-01): accepted by config, never
//! a runtime candidate.

use thegn_core::config::SandboxBackend;
use thegn_core::sandbox::Backend;
use thegn_core::seam::Kind as _;

#[test]
fn from_str_any_accepts_reserved_aliases() {
    for (spelling, kind) in [
        ("smol", SandboxBackend::Smol),
        ("smolmachines", SandboxBackend::Smol),
        ("wsl", SandboxBackend::Wsl),
        ("appcontainer", SandboxBackend::WinAppContainer),
        ("winappcontainer", SandboxBackend::WinAppContainer),
    ] {
        assert_eq!(
            SandboxBackend::from_str_any(spelling),
            Some(kind),
            "{spelling}"
        );
        assert!(kind.is_reserved(), "{spelling}");
        let error = SandboxBackend::from_str_validated(spelling).unwrap_err();
        assert!(error.contains("is reserved"), "{spelling}: {error}");
        assert_eq!(Backend::parse(spelling), None, "{spelling}");
    }
    assert_eq!(SandboxBackend::from_str_any("bogus"), None);
    assert_eq!(
        SandboxBackend::from_str_any("podman"),
        Some(SandboxBackend::Podman)
    );
}

#[test]
fn smol_maps_to_a_reserved_runtime_backend() {
    let backend = Backend::from_config(SandboxBackend::Smol).unwrap();
    assert_eq!(backend, Backend::Smol);
    assert!(backend.is_reserved());
    assert!(!Backend::oci_runtimes().any(|b| b == Backend::Smol));
}
