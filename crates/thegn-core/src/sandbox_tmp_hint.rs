//! Pure helpers for warning about prompt paths hidden by a sandbox-private `/tmp`.

use crate::config::FileAccess;
use crate::sandbox::{BackendFamily, SandboxSpec};

const WARNING_PREFIX: &str = "thegn: warning: the prompt names";
const WARNING_SUFFIX: &str = "files written there will not survive the session. Write under the worktree (for example .thegn/) instead.";

/// Find distinct absolute paths under `/tmp` in prompt text, preserving order.
pub fn tmp_paths_in(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut offset = 0;
    while let Some(relative) = text[offset..].find("/tmp") {
        let start = offset + relative;
        let after_prefix = start + "/tmp".len();
        let previous = text[..start].chars().next_back();
        if previous
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/'))
        {
            offset = after_prefix;
            continue;
        }

        let next = text[after_prefix..].chars().next();
        if !matches!(next, Some('/'))
            && !is_token_end(next)
            && !next.is_some_and(is_trailing_punctuation)
        {
            offset = after_prefix;
            continue;
        }

        let end = text[after_prefix..]
            .char_indices()
            .find(|(_, ch)| is_token_end(Some(*ch)))
            .map_or(text.len(), |(index, _)| after_prefix + index);
        let path =
            text[start..end].trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '\'', '"', '`']);
        if (path == "/tmp" || path.starts_with("/tmp/"))
            && !paths.iter().any(|existing| existing == path)
        {
            paths.push(path.to_string());
        }
        offset = end.max(after_prefix);
    }
    paths
}

fn is_token_end(ch: Option<char>) -> bool {
    ch.is_none_or(|ch| ch.is_whitespace() || matches!(ch, '\'' | '"' | '`' | ')' | ',' | ';'))
}

fn is_trailing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '.' | ',' | ';' | ':' | ')' | ']' | '}' | '\'' | '"' | '`'
    )
}

/// Whether the resolved sandbox gives the worker a private `/tmp`.
pub fn has_private_tmp(spec: &SandboxSpec) -> bool {
    let family = spec.backend.profile().family;
    (family == BackendFamily::Bwrap
        && !matches!(spec.file_access, FileAccess::All | FileAccess::Host))
        || matches!(family, BackendFamily::Oci | BackendFamily::Systemd)
}

/// Keep only prompt paths whose `/tmp` contents are hidden by the sandbox.
/// Explicit writable mounts at or above a path make that path host-visible.
pub fn private_tmp_paths(spec: &SandboxSpec, paths: &[String]) -> Vec<String> {
    if !has_private_tmp(spec) {
        return Vec::new();
    }
    paths
        .iter()
        .filter(|path| {
            !spec
                .mounts
                .iter()
                .any(|mount| !mount.ro && std::path::Path::new(path).starts_with(&mount.dest))
        })
        .cloned()
        .collect()
}

/// Render the bounded warning lines for prompt paths hidden by a private `/tmp`.
pub fn warning_lines(env_name: &str, paths: &[String]) -> Vec<String> {
    let mut lines = paths
        .iter()
        .take(3)
        .map(|path| {
            format!(
                "{WARNING_PREFIX} {path} but this worktree's sandbox ({env_name}) has a private /tmp; {WARNING_SUFFIX}"
            )
        })
        .collect::<Vec<_>>();
    if paths.len() > 3 {
        lines.push(format!(
            "{WARNING_PREFIX} and {} more /tmp paths but this worktree's sandbox ({env_name}) has a private /tmp; {WARNING_SUFFIX}",
            paths.len() - 3
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Network;
    use crate::placement::Placement;
    use crate::sandbox::{Backend, Mount, SandboxLimits};
    use std::path::PathBuf;

    #[test]
    fn tmp_paths_in_finds_distinct_paths_and_strips_prompt_punctuation() {
        let text = "write /tmp/first. '/tmp/second:' `/tmp/first` /tmp. /tmp, /tmpfoo /var/tmp/no; [/tmp/third] and /tmp/second";
        assert_eq!(
            tmp_paths_in(text),
            vec!["/tmp/first", "/tmp/second", "/tmp", "/tmp/third"]
        );
    }

    #[test]
    fn tmp_paths_in_returns_none_when_no_tmp_path_is_named() {
        assert!(tmp_paths_in("use /tmpfoo and /var/tmp/report").is_empty());
    }

    #[test]
    fn has_private_tmp_matches_backend_argv_predicates() {
        assert!(has_private_tmp(&spec(
            Backend::Bwrap,
            FileAccess::Worktree,
            false
        )));
        assert!(!has_private_tmp(&spec(
            Backend::Bwrap,
            FileAccess::All,
            false
        )));
        assert!(!has_private_tmp(&spec(
            Backend::Bwrap,
            FileAccess::Host,
            false
        )));
        assert!(!has_private_tmp(&spec(
            Backend::None,
            FileAccess::Worktree,
            false
        )));
        assert!(has_private_tmp(&spec(
            Backend::Podman,
            FileAccess::All,
            true
        )));
        assert!(has_private_tmp(&spec(
            Backend::Podman,
            FileAccess::All,
            false
        )));
        assert!(has_private_tmp(&spec(
            Backend::Systemd,
            FileAccess::All,
            false
        )));
    }

    #[test]
    fn private_tmp_paths_excludes_paths_covered_by_mounts() {
        let mut sandbox = spec(Backend::Podman, FileAccess::All, false);
        sandbox.mounts.push(Mount {
            host: "/host/reports".into(),
            dest: "/tmp/reports".into(),
            ro: false,
            cache: false,
        });
        let paths = vec![
            "/tmp/reports/result.md".into(),
            "/tmp/other/result.md".into(),
            "/tmp/reports".into(),
        ];
        assert_eq!(
            private_tmp_paths(&sandbox, &paths),
            vec!["/tmp/other/result.md"]
        );
    }

    #[test]
    fn private_tmp_paths_excludes_paths_covered_by_mounts_for_bwrap() {
        let mut sandbox = spec(Backend::Bwrap, FileAccess::Worktree, false);
        sandbox.mounts.push(Mount {
            host: "/host/reports".into(),
            dest: "/tmp/reports".into(),
            ro: false,
            cache: false,
        });
        let paths = vec![
            "/tmp/reports/result.md".into(),
            "/tmp/other/result.md".into(),
            "/tmp/reports".into(),
        ];
        assert_eq!(
            private_tmp_paths(&sandbox, &paths),
            vec!["/tmp/other/result.md"]
        );
    }

    #[test]
    fn private_tmp_paths_keeps_paths_beside_a_mount() {
        let mut sandbox = spec(Backend::Bwrap, FileAccess::Worktree, false);
        sandbox.mounts.push(Mount {
            host: "/host/reports".into(),
            dest: "/tmp/reports".into(),
            ro: false,
            cache: false,
        });
        let paths = vec![
            "/tmp/reports/result.md".into(),
            "/tmp/reports-other/x".into(),
        ];
        assert_eq!(
            private_tmp_paths(&sandbox, &paths),
            vec!["/tmp/reports-other/x"]
        );
    }

    #[test]
    fn private_tmp_paths_keeps_paths_covered_by_read_only_mounts() {
        let mut sandbox = spec(Backend::Bwrap, FileAccess::Worktree, false);
        sandbox.mounts.push(Mount {
            host: "/host/reports".into(),
            dest: "/tmp/reports".into(),
            ro: true,
            cache: false,
        });
        let paths = vec!["/tmp/reports/result.md".into()];
        assert_eq!(private_tmp_paths(&sandbox, &paths), paths);
    }

    #[test]
    fn warning_lines_cap_paths_and_report_overflow() {
        let paths = (1..=5)
            .map(|n| format!("/tmp/path-{n}"))
            .collect::<Vec<_>>();
        let lines = warning_lines("pipeline", &paths);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("/tmp/path-1"));
        assert!(lines[2].contains("/tmp/path-3"));
        assert!(!lines.iter().any(|line| line.contains("/tmp/path-4")));
        assert!(lines[3].contains("and 2 more"));
        assert!(lines[3].contains("sandbox (pipeline)"));
    }

    fn spec(backend: Backend, file_access: FileAccess, read_only_root: bool) -> SandboxSpec {
        SandboxSpec {
            backend,
            placement: Placement::Local,
            image: Some("img:latest".into()),
            worktree: PathBuf::from("/wt/feat"),
            mounts: vec![Mount {
                host: "/wt/feat".into(),
                dest: "/wt/feat".into(),
                ro: false,
                cache: false,
            }],
            env: Vec::new(),
            env_overrides: std::collections::HashMap::new(),
            env_block: Vec::new(),
            network: Network::Nat,
            network_allow: Vec::new(),
            network_block: Vec::new(),
            read_only_root,
            no_new_privileges: false,
            pids_limit: None,
            drop_capabilities: Vec::new(),
            add_capabilities: Vec::new(),
            file_access,
            ports: Vec::new(),
            gpu: None,
            limits: SandboxLimits::default(),
            volumes: Vec::new(),
            compose: None,
            build: None,
            init_script: None,
            devenv: false,
            devenv_path: None,
            name: "thegn-test".into(),
            vpn: None,
            oci_host: None,
            oci_runtime: None,
            daemon_persistent: false,
            seal_home: None,
            volume_owners: Vec::new(),
        }
    }
}
