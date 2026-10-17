//! Per-tab monitor presentation, navigation metadata, keys and footer hints.

use super::*;
use crate::seg::Seg;
use crate::telemetry::StatsSnapshot;

pub(super) mod containers;
pub(super) mod cpu;
pub(super) mod disk;
pub(super) mod gpu;
pub(super) mod memory;
pub(super) mod network;
pub(super) mod notifications;
pub(super) mod power;
pub(super) mod procs;
pub(super) mod thermal;

impl MonitorTab {
    /// Whether this machine has anything to show on the tab. A tab with no data
    /// is worse than a missing one: it reads as broken. `has_containers` is
    /// `!model.containers.is_empty()` — the "a container engine is present"
    /// signal the Containers tab hides on (like GPU/Power hiding with no
    /// device).
    pub(in crate::monitor) fn present(self, s: &StatsSnapshot, has_containers: bool) -> bool {
        match self {
            MonitorTab::Gpu => s.gpu_pct.is_some(),
            MonitorTab::Power => s.battery.is_some(),
            MonitorTab::Thermal => s.cpu_temp_c.is_some() || !s.temps.is_empty(),
            MonitorTab::Disk => !s.disks.is_empty(),
            MonitorTab::Network => s.net_bps.is_some() || !s.net_ifaces.is_empty(),
            MonitorTab::Containers => has_containers,
            MonitorTab::Notifications => false,
            // CPU, Memory and Processes are always meaningful.
            _ => true,
        }
    }
}

impl MonitorOverlay {
    pub(in crate::monitor) fn is_list_tab(&self) -> bool {
        matches!(
            self.tab,
            MonitorTab::Procs | MonitorTab::Disk | MonitorTab::Containers
        )
    }

    pub(in crate::monitor) fn row_len(&self) -> usize {
        match self.tab {
            MonitorTab::Procs => self.proc_rows.len(),
            MonitorTab::Disk => self.disk_rows.len(),
            MonitorTab::Containers => self.container_rows.len(),
            MonitorTab::Notifications => 0,
            _ => 0,
        }
    }

    pub(in crate::monitor) fn tab_key(&mut self, key: &KeyCode) -> MonitorOutcome {
        match key {
            KeyCode::Char('/') if self.tab == MonitorTab::Procs => {
                self.filtering = true;
                self.status = None;
                MonitorOutcome::Pending
            }
            KeyCode::Char('t') if self.tab == MonitorTab::Procs => {
                self.prefs.proc_tree = !self.prefs.proc_tree;
                self.sel = 0;
                MonitorOutcome::PrefsChanged
            }
            KeyCode::Char('x') if self.tab == MonitorTab::Procs => self.begin_signal(),
            KeyCode::Char('x') if self.tab == MonitorTab::Disk => self.begin_clean(),
            KeyCode::Char(c) if self.tab == MonitorTab::Procs => self.proc_key(*c),
            KeyCode::Enter if self.tab == MonitorTab::Containers => self.container_key('\r'),
            KeyCode::Char(c) if self.tab == MonitorTab::Containers => self.container_key(*c),
            _ => MonitorOutcome::Pending,
        }
    }
}

pub(super) fn hints(input: &footer::FooterInput<'_>, out: &mut Vec<Vec<Seg>>) {
    match input.tab {
        MonitorTab::Procs => procs::hints(input, out),
        MonitorTab::Disk => disk::hints(input, out),
        MonitorTab::Containers => containers::hints(input, out),
        _ => {}
    }
}
