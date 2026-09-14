//! Actual private POSIX CLI execution through the shipping cache/capture path.
#[cfg(unix)]
mod unix {
    use super::super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn cached_probe_executes_captured_inputs_once_and_preserves_nonzero_version() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join(CLI_NAME);
        let report = dir.path().join("invocations");
        std::fs::write(
            &executable,
            r##"#!/bin/sh
[ "$#" -eq 1 ] && [ "$1" = --version ] || exit 41
[ "${HOME+x}" != x ] || exit 42
printf '%s\n%s\n' "$PWD" "$SNAPSHOT_VALUE" >> "$REPORT"
printf '1.2.3-private-fixture\n'
printf 'diagnostic stderr\n' >&2
exit 23
"##,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = vec![
            ("PATH".into(), dir.path().as_os_str().into()),
            ("REPORT".into(), report.as_os_str().into()),
            ("SNAPSHOT_VALUE".into(), "captured-value".into()),
        ];
        let cache = Cache::default();
        for _ in 0..3 {
            let result = cache.get(
                || Inputs::from_snapshot(env.clone(), dir.path().into()),
                Inputs::run,
                Instant::now,
                PROBE_TIMEOUT,
            );
            assert_eq!(result.state, ProbeState::Degraded);
            assert_eq!(result.version.as_deref(), Some("1.2.3-private-fixture"));
            assert!(result.reason.as_deref().unwrap().contains("23"));
            assert_eq!(result.executable.as_deref(), executable.to_str());
        }
        assert_eq!(
            std::fs::read_to_string(&report).unwrap(),
            format!("{}\ncaptured-value\n", dir.path().display())
        );
        drop(cache);
        dir.close().unwrap();
    }
}
