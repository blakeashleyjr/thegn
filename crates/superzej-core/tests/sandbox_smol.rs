use superzej_core::config::SandboxBackend;
use superzej_core::sandbox::Backend;

#[test]
fn test_smol_backend_parsing() {
    assert_eq!(
        SandboxBackend::from_str_validated("smol").unwrap(),
        SandboxBackend::Smol
    );
    assert_eq!(
        SandboxBackend::from_str_validated("smolmachines").unwrap(),
        SandboxBackend::Smol
    );
}

#[test]
fn test_smol_backend_mapping() {
    let backend = Backend::from_config(SandboxBackend::Smol).unwrap();
    assert_eq!(backend, Backend::Smol);
    assert_eq!(backend.label(), "smolmachines");
    assert_eq!(backend.binary(), "smolmachines");
    assert!(backend.is_oci());
}
