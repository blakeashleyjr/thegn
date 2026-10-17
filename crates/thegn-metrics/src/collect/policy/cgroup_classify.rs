//! Portable cgroup path ownership rules shared by the cgroup readers.

/// Source of truth: `thegn_core::sandbox_cpucap::CPU_SLICE`.
pub const THEGN_SLICE: &str = "thegn.slice";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    Pane,
    Slice,
    Podman,
    Docker,
    App,
    Unit,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner<'a> {
    pub kind: OwnerKind,
    pub name: &'a [u8],
}

fn segments(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
}

fn owner(kind: OwnerKind, name: &[u8]) -> Owner<'_> {
    Owner { kind, name }
}

pub fn is_thegn_slice(path: &[u8]) -> bool {
    segments(path).any(|part| part == THEGN_SLICE.as_bytes())
}

pub fn slice_prefix(path: &[u8]) -> Option<&[u8]> {
    let mut start = 0;
    for part in segments(path) {
        let offset = part.as_ptr() as usize - path.as_ptr() as usize;
        if part == THEGN_SLICE.as_bytes() {
            start = offset + part.len();
            return Some(&path[..start]);
        }
    }
    None
}

pub fn scope_dir(path: &[u8]) -> &[u8] {
    let Some(parent_end) = path.strip_suffix(b"/container").map(<[u8]>::len) else {
        return path;
    };
    let parent = &path[..parent_end];
    if segments(parent)
        .next_back()
        .is_some_and(|part| part.starts_with(b"libpod-") && part.ends_with(b".scope"))
    {
        parent
    } else {
        path
    }
}

pub fn classify(path: &[u8]) -> Owner<'_> {
    let mut previous = None;
    let mut final_segment = None;
    for segment in segments(path) {
        previous = final_segment;
        final_segment = Some(segment);
    }
    let Some(last) = final_segment else {
        return Owner {
            kind: OwnerKind::Other,
            name: b"",
        };
    };
    if last == THEGN_SLICE.as_bytes() {
        return owner(OwnerKind::Slice, last);
    }
    if last.starts_with(b"run-")
        && last.ends_with(b".scope")
        && previous == Some(THEGN_SLICE.as_bytes())
    {
        return owner(OwnerKind::Pane, last);
    }
    if last.starts_with(b"libpod-conmon-") && last.ends_with(b".scope") {
        return owner(OwnerKind::Other, last);
    }
    if last.starts_with(b"libpod-") && last.ends_with(b".scope") {
        return owner(
            OwnerKind::Podman,
            &last[b"libpod-".len()..last.len() - b".scope".len()],
        );
    }
    if last.starts_with(b"docker-") && last.ends_with(b".scope") {
        return owner(
            OwnerKind::Docker,
            &last[b"docker-".len()..last.len() - b".scope".len()],
        );
    }
    if previous == Some(b"docker".as_slice()) {
        return owner(OwnerKind::Docker, last);
    }
    if last.starts_with(b"app-") && last.ends_with(b".scope") {
        return owner(OwnerKind::App, last);
    }
    if last.ends_with(b".service") {
        return owner(OwnerKind::Unit, last);
    }
    owner(OwnerKind::Other, last)
}

#[cfg(test)]
mod tests {
    use super::{OwnerKind, classify, is_thegn_slice, scope_dir, slice_prefix};

    #[test]
    fn classifies_whole_segments_and_container_leaf() {
        let id = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(
            classify(b"/user.slice/thegn.slice/run-p15278-i4595.scope").kind,
            OwnerKind::Pane
        );
        assert_eq!(classify(b"/user.slice/thegn.slice").kind, OwnerKind::Slice);
        assert_eq!(
            classify(b"/user.slice/thegn.slice-other.scope").kind,
            OwnerKind::Other
        );
        let pod = [
            b"/machine.slice/libpod-".as_slice(),
            id,
            b".scope/container",
        ]
        .concat();
        let resolved = scope_dir(&pod);
        assert_eq!(classify(resolved).kind, OwnerKind::Podman);
        assert_eq!(classify(resolved).name, id);
        assert_eq!(
            classify(b"/machine.slice/libpod-conmon-abc.scope").kind,
            OwnerKind::Other
        );
        assert_eq!(
            classify(b"/system.slice/docker-abc.scope").kind,
            OwnerKind::Docker
        );
        assert_eq!(classify(b"/docker/abc").kind, OwnerKind::Docker);
        assert_eq!(classify(b"/app-foo.scope").kind, OwnerKind::App);
        assert_eq!(classify(b"/sshd.service").kind, OwnerKind::Unit);
        assert_eq!(classify(b"/").kind, OwnerKind::Other);
        assert_eq!(classify(b"").kind, OwnerKind::Other);
        assert!(is_thegn_slice(b"/a/thegn.slice/b"));
        assert!(!is_thegn_slice(b"/a/thegn.slice-other/b"));
        assert_eq!(
            slice_prefix(b"/a/thegn.slice/b"),
            Some(&b"/a/thegn.slice"[..])
        );
    }
}
