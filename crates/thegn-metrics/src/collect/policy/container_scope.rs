//! Pure resolution rules for container cgroup v2 scopes.

use std::path::PathBuf;

use super::cgroup_classify::{OwnerKind, classify, scope_dir};

/// Resolve a unified-hierarchy `/proc/<pid>/cgroup` line to its cgroup path.
/// A libpod scope's `container` leaf is represented by the enclosing scope,
/// whose counters aggregate the container process subtree.
pub fn scope_from_cgroup_line(line: &str) -> Option<PathBuf> {
    let (hierarchy, rest) = line.split_once(':')?;
    let (controllers, path) = rest.split_once(':')?;
    if hierarchy != "0" || !controllers.is_empty() || path.is_empty() {
        return None;
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    let scope = scope_dir(path.as_bytes());
    if !matches!(classify(scope).kind, OwnerKind::Docker | OwnerKind::Podman) {
        return None;
    }
    let relative = scope.strip_prefix(b"/").unwrap_or(scope);
    if relative.is_empty() {
        return None;
    }
    Some(PathBuf::from(std::str::from_utf8(relative).ok()?))
}

/// Return engine cgroup locations in the order they should be probed.
/// The paths are relative to the cgroup v2 mount.
pub fn fallback_candidates(id: &str, uid: u32) -> Vec<PathBuf> {
    if id.is_empty() || id.contains('/') || id == "." || id == ".." {
        return Vec::new();
    }
    [
        format!("system.slice/docker-{id}.scope"),
        format!("docker/{id}"),
        format!("machine.slice/libpod-{id}.scope"),
        format!("user.slice/user-{uid}.slice/user@{uid}.service/user.slice/libpod-{id}.scope"),
        format!("user.slice/user-{uid}.slice/user@{uid}.service/app.slice/docker-{id}.scope"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{fallback_candidates, scope_from_cgroup_line};
    use std::path::PathBuf;

    #[test]
    fn resolves_unified_libpod_leaf_and_rejects_other_hierarchies() {
        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            scope_from_cgroup_line(&format!(
                "0::/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-{id}.scope/container"
            )),
            Some(PathBuf::from(format!(
                "user.slice/user-1000.slice/user@1000.service/user.slice/libpod-{id}.scope"
            )))
        );
        assert_eq!(
            scope_from_cgroup_line("0::/system.slice/docker-test.scope"),
            Some(PathBuf::from("system.slice/docker-test.scope"))
        );
        assert_eq!(scope_from_cgroup_line("2:cpu,cpuacct:/docker/test"), None);
        assert_eq!(scope_from_cgroup_line("0::/"), None);
        assert_eq!(scope_from_cgroup_line("0::/user.slice/some.service"), None);
    }

    #[test]
    fn resolved_scope_accepts_proc_cgroup_path() {
        let path = scope_from_cgroup_line("0::/system.slice/docker-test.scope").unwrap();
        assert!(
            crate::collect::linux::containers_stats::ResolvedContainerScope::new("test", path)
                .is_some()
        );
    }

    #[test]
    fn fallback_candidates_follow_engine_probe_order() {
        let id = "full-container-id";
        assert_eq!(
            fallback_candidates(id, 1000),
            [
                "system.slice/docker-full-container-id.scope",
                "docker/full-container-id",
                "machine.slice/libpod-full-container-id.scope",
                "user.slice/user-1000.slice/user@1000.service/user.slice/libpod-full-container-id.scope",
                "user.slice/user-1000.slice/user@1000.service/app.slice/docker-full-container-id.scope",
            ]
            .map(PathBuf::from)
        );
        assert!(fallback_candidates("../escape", 1000).is_empty());
    }
}
