use std::path::Path;

fn repository_file(path: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("could not read {path}: {error}"))
}

#[test]
fn shipped_nix_images_keep_signature_checks_enabled() {
    let files = [
        "../../nix/sandbox-image.nix",
        "../../nix/fly-sandbox-image.nix",
        "../../config/sandbox-image/Containerfile",
        "../../config/config.toml.example",
    ];

    for path in files {
        let source = repository_file(path);
        for (line_number, line) in source.lines().enumerate() {
            let normalized: String = line
                .trim()
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect();
            assert!(
                !normalized.contains("require-sigs=false"),
                "{path}:{} disables Nix signature checks",
                line_number + 1
            );
            assert!(
                !line.contains("--no-check-sigs"),
                "{path}:{} bypasses Nix signature checks",
                line_number + 1
            );
        }
    }
}
