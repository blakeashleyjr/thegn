//! Relevant row-cache keys. No snapshot clone or PID-only equality shortcut.
use super::{MonitorOverlay, MonitorTab, ProcSort, build, procs_view};
use crate::chrome::FrameModel;
use crate::model_eq::ContentRevision;

#[derive(Debug)]
pub(super) struct ProcessRowsKey {
    revision: u64,
    state: crate::model_eq::ProcessViewState,
    sort: ProcSort,
    desc: bool,
    tree: bool,
    filter: String,
}
impl ProcessRowsKey {
    fn capture(overlay: &MonitorOverlay, model: &FrameModel) -> Self {
        Self {
            revision: model.process_revision,
            state: model.process_state,
            sort: overlay.prefs.proc_sort,
            desc: overlay.prefs.proc_desc,
            tree: overlay.prefs.proc_tree,
            filter: overlay.filter.clone(),
        }
    }
    fn matches(&self, overlay: &MonitorOverlay, model: &FrameModel) -> bool {
        self.revision == model.process_revision
            && self.state == model.process_state
            && self.sort == overlay.prefs.proc_sort
            && self.desc == overlay.prefs.proc_desc
            && self.tree == overlay.prefs.proc_tree
            && self.filter == overlay.filter
    }
}
#[derive(Default)]
pub(super) struct Lists {
    process: Option<ProcessRowsKey>,
    disk: Option<ContentRevision>,
}
impl MonitorOverlay {
    pub(super) fn process_rows_current(&self, model: &FrameModel) -> bool {
        self.lists
            .process
            .as_ref()
            .is_some_and(|key| key.matches(self, model))
    }
    pub(super) fn refresh_active_rows(
        &mut self,
        model: &FrameModel,
        now_secs: u64,
        preserve_process: bool,
    ) {
        match self.tab {
            MonitorTab::Procs if !self.process_rows_current(model) => {
                let selected = preserve_process
                    .then(|| {
                        self.proc_rows
                            .get(self.sel)
                            .map(|row| (row.pid, row.start_time))
                    })
                    .flatten();
                self.proc_rows = procs_view::rows(&model.procs, self.proc_view());
                self.lists.process = Some(ProcessRowsKey::capture(self, model));
                if let Some(identity) = selected
                    && let Some(index) = self
                        .proc_rows
                        .iter()
                        .position(|row| (row.pid, row.start_time) == identity)
                {
                    self.sel = index;
                }
            }
            MonitorTab::Disk => {
                if !self
                    .lists
                    .disk
                    .is_some_and(|old| old.same_cacheable(model.monitor_disk_revision))
                {
                    self.disk_rows = build::worktree_disk_rows(model, now_secs);
                    self.lists.disk = Some(model.monitor_disk_revision);
                } else {
                    // Ages progress from cached measurement timestamps without
                    // walking/sorting the model maps or re-allocating labels.
                    for row in &mut self.disk_rows {
                        row.age_secs = row
                            .measured_at_secs
                            .map(|stamp| now_secs.saturating_sub(stamp));
                    }
                }
            }
            _ => {}
        }
    }
}
