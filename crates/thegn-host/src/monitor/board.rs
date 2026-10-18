//! Panel placement and navigation for a monitor tab.

use termwiz::surface::Surface;

use crate::{
    compositor::Rect,
    sections::{self, Section},
};
use thegn_metrics::collect::{CollectorId, CollectorSet};

use super::layout::{self, Node, PanelId, PanelSpec, Placed, Solution, Variant, VariantKind};
use super::tabs::overview::{OverviewModel, OverviewView};
use crate::monitor_sampler::Demand;

/// The small surface a tab exposes to the board. `rows` describes its
/// scrollable content.
pub(super) trait TabView {
    fn specs(&self) -> &'static [PanelSpec];
    fn template(&self) -> Node;
    fn demand(&self, panel: PanelId) -> CollectorSet;
    fn paint(
        &self,
        surface: &mut Surface,
        placed: &Placed,
        rect: Rect,
        scroll: usize,
        sections: &[Section],
    );
}

/// A tab that still renders its existing section stack as one full-area panel.
/// Its all-collector demand intentionally keeps F7 history filling as it does
/// today, while every other monitor tab remains on the legacy data path.
pub(super) struct LegacyTab;

pub(super) enum ActiveView<'a> {
    Legacy(LegacyTab),
    Cpu(super::tabs::cpu::view::CpuView<'a>),
    Overview(OverviewView<'a>),
    Power(super::tabs::power::view::PowerView<'a>),
    Memory(super::tabs::memory::view::MemoryView<'a>),
}

impl TabView for ActiveView<'_> {
    fn specs(&self) -> &'static [PanelSpec] {
        match self {
            Self::Legacy(view) => view.specs(),
            Self::Cpu(view) => view.specs(),
            Self::Overview(view) => view.specs(),
            Self::Power(view) => view.specs(),
            Self::Memory(view) => view.specs(),
        }
    }

    fn template(&self) -> Node {
        match self {
            Self::Legacy(view) => view.template(),
            Self::Cpu(view) => view.template(),
            Self::Overview(view) => view.template(),
            Self::Power(view) => view.template(),
            Self::Memory(view) => view.template(),
        }
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        match self {
            Self::Legacy(view) => view.demand(panel),
            Self::Cpu(view) => view.demand(panel),
            Self::Overview(view) => view.demand(panel),
            Self::Power(view) => view.demand(panel),
            Self::Memory(view) => view.demand(panel),
        }
    }

    fn paint(
        &self,
        surface: &mut Surface,
        placed: &Placed,
        rect: Rect,
        scroll: usize,
        sections: &[Section],
    ) {
        match self {
            Self::Legacy(view) => view.paint(surface, placed, rect, scroll, sections),
            Self::Cpu(view) => view.paint(surface, placed, rect, scroll, sections),
            Self::Overview(view) => view.paint(surface, placed, rect, scroll, sections),
            Self::Power(view) => view.paint(surface, placed, rect, scroll, sections),
            Self::Memory(view) => view.paint(surface, placed, rect, scroll, sections),
        }
    }
}

impl<'a> ActiveView<'a> {
    pub(super) fn for_tab(tab: super::MonitorTab, model: &'a OverviewModel) -> Self {
        if tab == super::MonitorTab::Overview {
            ActiveView::Overview(OverviewView(model))
        } else {
            ActiveView::Legacy(LegacyTab)
        }
    }
}

const LEGACY_PANEL: PanelId = PanelId("legacy");
const LEGACY_VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (1, 1),
    pref: (1, 1),
}];
const LEGACY_SPECS: [PanelSpec; 1] = [PanelSpec {
    id: LEGACY_PANEL,
    priority: 0,
    variants: &LEGACY_VARIANTS,
}];

impl TabView for LegacyTab {
    fn specs(&self) -> &'static [PanelSpec] {
        &LEGACY_SPECS
    }

    fn template(&self) -> Node {
        Node::Panel(LEGACY_PANEL)
    }

    fn demand(&self, _panel: PanelId) -> CollectorSet {
        let mut demand = CollectorSet::empty();
        for id in CollectorId::ALL {
            if !id.visible_only() {
                demand.insert(id);
            }
        }
        demand
    }

    fn paint(
        &self,
        surface: &mut Surface,
        placed: &Placed,
        rect: Rect,
        scroll: usize,
        sections: &[Section],
    ) {
        if placed.id == LEGACY_PANEL {
            sections::render_stack(surface, rect, scroll, sections);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EscOutcome {
    Unzoomed,
    Close,
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Board {
    focus: Option<PanelId>,
    zoom: Option<PanelId>,
}

impl Board {
    pub(super) fn solve(&self, view: &impl TabView, area: Rect) -> Solution {
        layout::solve(&view.template(), view.specs(), area, self.zoom, &[])
    }

    pub(super) fn placed(&self, view: &impl TabView, area: Rect) -> Vec<Placed> {
        self.solve(view, area).placed
    }

    pub(super) fn hidden_count(&self, view: &impl TabView, area: Rect) -> usize {
        self.solve(view, area).hidden()
    }

    pub(super) fn focus_next(&mut self, placed: &[Placed]) {
        self.move_focus(placed, 1);
    }

    pub(super) fn focus_prev(&mut self, placed: &[Placed]) {
        self.move_focus(placed, -1);
    }

    fn move_focus(&mut self, placed: &[Placed], delta: isize) {
        if placed.len() < 2 {
            self.focus = None;
            return;
        }
        let current = placed.iter().position(|panel| Some(panel.id) == self.focus);
        let next = match current {
            Some(index) => (index as isize + delta).rem_euclid(placed.len() as isize) as usize,
            None if delta < 0 => placed.len() - 1,
            None => 0,
        };
        self.focus = Some(placed[next].id);
    }

    pub(super) fn toggle_zoom(&mut self, placed: &[Placed]) {
        if self.zoom.is_some() {
            self.zoom = None;
        } else if placed.len() < 2 {
            self.focus = None;
        } else if let Some(focus) = self
            .focus
            .filter(|focus| placed.iter().any(|panel| panel.id == *focus))
        {
            self.zoom = Some(focus);
        } else if let Some(panel) = placed.first() {
            self.focus = Some(panel.id);
            self.zoom = Some(panel.id);
        }
    }

    pub(super) fn is_zoomed(&self) -> bool {
        self.zoom.is_some()
    }

    pub(super) fn esc(&mut self) -> EscOutcome {
        if self.zoom.take().is_some() {
            EscOutcome::Unzoomed
        } else {
            EscOutcome::Close
        }
    }

    pub(super) fn demand(&self, view: &impl TabView, area: Rect, base_ms: u32) -> Demand {
        let collectors =
            self.solve(view, area)
                .placed
                .iter()
                .fold(CollectorSet::empty(), |mut set, panel| {
                    for id in view.demand(panel.id).iter() {
                        set.insert(id);
                    }
                    set
                });
        Demand {
            collectors,
            base_ms,
        }
    }

    pub(super) fn paint(
        &self,
        view: &impl TabView,
        surface: &mut Surface,
        area: Rect,
        scroll: usize,
        sections: &[Section],
    ) {
        for placed in self.solve(view, area).placed {
            let Some(rect) = intersect(placed.rect, area) else {
                continue;
            };
            let mut clipped = placed;
            clipped.rect = rect;
            view.paint(surface, &clipped, rect, scroll, sections);
        }
    }
}

fn intersect(rect: Rect, area: Rect) -> Option<Rect> {
    let x = rect.x.max(area.x);
    let y = rect.y.max(area.y);
    let right = rect
        .x
        .saturating_add(rect.cols)
        .min(area.x.saturating_add(area.cols));
    let bottom = rect
        .y
        .saturating_add(rect.rows)
        .min(area.y.saturating_add(area.rows));
    (right > x && bottom > y).then_some(Rect {
        x,
        y,
        cols: right - x,
        rows: bottom - y,
    })
}

/// Text for the tab-bar's right cluster, kept pure for a focused regression test.
pub(super) fn hidden_label(hidden: usize) -> Option<String> {
    (hidden > 0).then(|| format!("+{hidden} hidden"))
}

#[cfg(test)]
#[path = "board_tests.rs"]
mod tests;
