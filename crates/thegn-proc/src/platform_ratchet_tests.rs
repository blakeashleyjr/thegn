use std::{collections::BTreeSet, fs, path::Path};

#[test]
fn platform_cfgs_are_pinned() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("src");
    let mut found = BTreeSet::new();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "rs")
            && path
                .file_name()
                .is_some_and(|name| name != "platform_ratchet_tests.rs")
        {
            let contents = fs::read_to_string(&path).unwrap();
            if contents.lines().any(|line| {
                (line.contains("#[cfg(") || line.contains("cfg!("))
                    && ["unix", "windows", "target_os", "target_family"]
                        .iter()
                        .any(|needle| line.contains(needle))
            }) {
                found.insert(path.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let pinned: BTreeSet<_> =
        fs::read_to_string(manifest.join("../../test/platform-cfg-proc-ratchet.txt"))
            .unwrap()
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect();
    assert_eq!(found, pinned);
}
