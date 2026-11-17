//! How a sandboxed process is launched (lead ruling L2): one enum for the kind
//! of launch, set by the caller.
//!
//! Stub landed by WS00-P1-12; semantics owned by WS03-P2-03a, which carries the
//! value in `LinuxConfineRequest.launch` (WS03-P2-03b sets it at every
//! production caller). Consumers: WS05-P1-05 (`SeatbeltInputs.launch`; WS05's
//! former `LaunchMode { Pane, Batch }` is deleted, `Pane` becoming
//! [`LaunchKind::Interactive`]), WS02 (`exec_batch` for [`LaunchKind::Batch`]),
//! and WS11-P3-12/P3-13 (the merge gate and LSP launch as `Batch`).
//!
//! The launch kind is orthogonal to the launch *class* (THE-791: who the code
//! belongs to, `launch_class`). The module name stays outside the frozen
//! `sandbox*.rs` glob (B7) so W2 lanes can import it during the freeze.

/// How a sandboxed process is launched; set by the caller. Stub (WS00-P1-12);
/// semantics owned by WS03-P2-03a.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum LaunchKind {
    /// A PTY pane: the exact-tty rule applies.
    #[default]
    Interactive,
    /// A non-PTY command (merge gate, queue agent, LSP): no tty rule.
    Batch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_kind_default_is_interactive() {
        assert_eq!(LaunchKind::default(), LaunchKind::Interactive);
        assert_ne!(LaunchKind::Interactive, LaunchKind::Batch);
    }

    #[test]
    fn launch_kind_is_copy_and_hashable() {
        let kind = LaunchKind::Batch;
        let copy = kind;
        assert_eq!(kind, copy);
        let set: std::collections::HashSet<LaunchKind> =
            [LaunchKind::Interactive, LaunchKind::Batch, kind]
                .into_iter()
                .collect();
        assert_eq!(set.len(), 2);
        assert_eq!(format!("{copy:?}"), "Batch");
    }
}
