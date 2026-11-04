use std::{collections::BTreeSet, fs, path::Path};

#[test]
fn platform_cfgs_are_confined_to_platform_rs() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = BTreeSet::new();
    for entry in fs::read_dir(source).unwrap() {
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
    assert_eq!(found, BTreeSet::from(["platform.rs".to_string()]));
}
