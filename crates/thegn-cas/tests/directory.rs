use thegn_cas::{Digest, Entry, encode_directory};

#[test]
fn canonical_directory_matches_independent_reapi_vector() {
    let file = |content, executable| Entry::File {
        digest: Digest::of(content),
        executable,
    };
    let mut entries = vec![
        ("link".into(), Entry::Symlink { target: "a".into() }),
        ("b".into(), file(b"run", true)),
        ("dir".into(), Entry::Directory(Digest::of(b""))),
        ("a".into(), file(b"hello", false)),
    ];
    for _ in 0..2 {
        let bytes = encode_directory(entries.clone()).unwrap();
        assert_eq!(bytes, include_bytes!("fixtures/directory.pb"));
        assert_eq!(
            Digest::of(&bytes).hash(),
            "f89f35af1a987e36f10471059c6beab164b49c758858b1da148d46995657e3a6"
        );
        entries.reverse();
    }
    assert!(encode_directory(vec![]).unwrap().is_empty());
}

#[test]
fn rejects_duplicate_and_invalid_names_before_hashing() {
    for name in ["", ".", "..", "a/b", "a\\b", "C:file", "bad\0name"] {
        assert!(encode_directory(vec![(name.into(), Entry::Directory(Digest::of(b"")))]).is_err());
    }
    assert!(
        encode_directory(vec![
            ("dup".into(), Entry::Directory(Digest::of(b""))),
            (
                "dup".into(),
                Entry::Symlink {
                    target: "relative".into()
                }
            ),
        ])
        .is_err()
    );
}
