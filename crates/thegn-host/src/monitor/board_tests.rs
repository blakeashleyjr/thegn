use super::*;
use crate::{
    compositor::Rect,
    sections::{self, Section},
    seg::Tok,
};
use termwiz::surface::Surface;
use thegn_metrics::collect::{CollectorId, CollectorSet};

const IDS: [PanelId; 3] = [PanelId("one"), PanelId("two"), PanelId("three")];
const VARIANTS: [Variant; 1] = [Variant {
    kind: VariantKind::Full,
    min: (1, 2),
    pref: (1, 2),
}];
const SPECS: [PanelSpec; 3] = [
    PanelSpec {
        id: IDS[0],
        priority: 1,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[1],
        priority: 1,
        variants: &VARIANTS,
    },
    PanelSpec {
        id: IDS[2],
        priority: 1,
        variants: &VARIANTS,
    },
];

struct ThreePanels;

impl TabView for ThreePanels {
    fn specs(&self) -> &'static [PanelSpec] {
        &SPECS
    }

    fn template(&self) -> Node {
        Node::Col(
            IDS.into_iter()
                .map(|id| (super::layout::Size::Weight(1), Node::Panel(id)))
                .collect(),
        )
    }

    fn demand(&self, panel: PanelId) -> CollectorSet {
        let mut result = CollectorSet::empty();
        let id = match panel {
            id if id == IDS[0] => CollectorId::Loadavg,
            id if id == IDS[1] => CollectorId::Uptime,
            _ => CollectorId::Psi,
        };
        result.insert(id);
        result
    }

    fn paint(
        &self,
        _surface: &mut Surface,
        _placed: &Placed,
        _rect: Rect,
        _scroll: usize,
        _sections: &[Section],
    ) {
    }
}

fn area() -> Rect {
    Rect {
        x: 3,
        y: 4,
        cols: 20,
        rows: 5,
    }
}

#[test]
fn focus_cycles_only_placed_panels_and_wraps() {
    let view = ThreePanels;
    let mut board = Board::default();
    let placed = board.placed(&view, area());
    assert_eq!(
        placed.iter().map(|panel| panel.id).collect::<Vec<_>>(),
        IDS[..2].to_vec()
    );
    board.focus_next(&placed);
    assert_eq!(board.focus, Some(IDS[0]));
    board.focus_next(&placed);
    assert_eq!(board.focus, Some(IDS[1]));
    board.focus_next(&placed);
    assert_eq!(board.focus, Some(IDS[0]));
    board.focus_prev(&placed);
    assert_eq!(board.focus, Some(IDS[1]));
}

#[test]
fn zoom_uses_solver_area_and_escape_unzooms_before_close() {
    let view = ThreePanels;
    let mut board = Board::default();
    let placed = board.placed(&view, area());
    board.focus_next(&placed);
    board.toggle_zoom(&placed);
    assert_eq!(board.solve(&view, area()).placed[0].rect, area());
    assert_eq!(board.solve(&view, area()).placed[0].id, IDS[0]);
    assert_eq!(board.esc(), EscOutcome::Unzoomed);
    assert_eq!(board.esc(), EscOutcome::Close);
}

#[test]
fn a_single_panel_never_focuses_or_zooms_and_escape_closes() {
    let mut board = Board::default();
    let placed = board.placed(&LegacyTab, area());
    board.focus_next(&placed);
    assert_eq!(board.focus, None);
    board.focus_prev(&placed);
    assert_eq!(board.focus, None);
    board.toggle_zoom(&placed);
    assert!(!board.is_zoomed());
    assert_eq!(board.esc(), EscOutcome::Close);
}

#[test]
fn demand_unions_placed_panels_and_legacy_demands_every_collector() {
    let view = ThreePanels;
    let board = Board::default();
    let demand = board.demand(&view, area(), 750);
    assert_eq!(demand.base_ms, 750);
    assert_eq!(demand.collectors.len(), 2);
    assert!(demand.collectors.contains(CollectorId::Loadavg));
    assert!(demand.collectors.contains(CollectorId::Uptime));
    assert!(!demand.collectors.contains(CollectorId::Psi));

    let legacy = board.demand(&LegacyTab, area(), 500);
    assert_eq!(legacy.collectors.len(), CollectorId::ALL.len() - 1);
    for id in CollectorId::ALL {
        assert_eq!(legacy.collectors.contains(id), !id.visible_only());
    }
}

#[test]
fn hidden_count_label_is_only_present_when_solver_drops_panels() {
    let board = Board::default();
    let hidden = board.hidden_count(&ThreePanels, area());
    assert_eq!(hidden, 1);
    assert_eq!(hidden_label(hidden).as_deref(), Some("+1 hidden"));
    assert_eq!(hidden_label(0), None);
}

#[test]
fn footer_shows_board_hints_only_for_multiple_panels_and_names_unzoom() {
    let prefs = super::super::MonitorPrefs::default();
    let footer = |board_panels, board_zoomed| {
        super::super::footer::line(super::super::footer::FooterInput {
            tab: super::super::MonitorTab::Cpu,
            prefs: &prefs,
            confirm: None,
            filtering: false,
            filter: "",
            notice: None,
            status: None,
            paused: false,
            container_ours: false,
            disk_rows: 0,
            board_panels,
            board_zoomed,
        })
    };
    let text = |line: crate::seg::Line| match line {
        crate::seg::Line::Segs(segs) => {
            segs.iter().map(|seg| seg.text.as_str()).collect::<String>()
        }
        crate::seg::Line::Split { l, r } | crate::seg::Line::SplitMinLeft { l, r, .. } => l
            .iter()
            .chain(r.iter())
            .map(|seg| seg.text.as_str())
            .collect::<String>(),
        _ => String::new(),
    };
    let single = text(footer(1, false));
    assert!(!single.contains("panel"));
    assert!(!single.contains("zoom"));
    let multi = text(footer(2, false));
    assert!(multi.contains("f panel"));
    assert!(multi.contains("z zoom"));
    assert!(text(footer(2, true)).contains("unzoom"));
}

#[test]
fn legacy_paint_matches_render_stack_cells_and_styles() {
    let body = vec![
        Section::Heading {
            label: "legacy heading".into(),
            note: Some("note".into()),
        },
        Section::KeyVal(vec![(
            "load".into(),
            "1.25".into(),
            Tok::Slot(crate::chrome::S::Accent),
        )]),
        Section::Heading {
            label: "second heading".into(),
            note: None,
        },
        Section::KeyVal(vec![(
            "uptime".into(),
            "longer body".into(),
            Tok::Slot(crate::chrome::S::Accent),
        )]),
    ];
    let rect = Rect {
        x: 1,
        y: 1,
        cols: 24,
        rows: 3,
    };
    let mut expected = Surface::new(28, 8);
    let mut actual = Surface::new(28, 8);
    sections::render_stack(&mut expected, rect, 1, &body);
    Board::default().paint(&LegacyTab, &mut actual, rect, 1, &body);
    assert_eq!(
        expected.screen_chars_to_string(),
        actual.screen_chars_to_string()
    );
    assert_eq!(expected.screen_cells(), actual.screen_cells());
}

#[test]
fn paint_skips_a_solver_rect_when_the_body_is_empty() {
    let rect = Rect {
        x: 2,
        y: 2,
        cols: 3,
        rows: 0,
    };
    let body = [Section::Heading {
        label: "must not draw".into(),
        note: None,
    }];
    let mut surface = Surface::new(8, 8);
    let before = surface.screen_chars_to_string();
    Board::default().paint(&LegacyTab, &mut surface, rect, 0, &body);
    assert_eq!(surface.screen_chars_to_string(), before);
}
