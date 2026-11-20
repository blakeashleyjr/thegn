//! The Lead indicator pinned to the bottom of the right panel (batch 180).
//! Pure: formats the cached `LeadBar` into rows; chrome.rs reserves the rows,
//! paints them and resolves clicks.

use thegn_core::lead_bar::{LeadBar, LeadState};
use thegn_core::theme::StateRole;

use crate::chrome::S;
use crate::seg::{self, Line, Seg, Tok, seg, sp};

use super::PanelHit;
use super::sections::PanelRow;

#[derive(Debug, Clone, Default)]
pub(crate) struct LeadFoot {
    pub rows: Vec<PanelRow>,
    /// (row index within `rows`, panel-relative x range) of the approvals chip.
    pub approvals: Option<(usize, std::ops::Range<usize>)>,
}

pub(crate) fn footer(bar: &LeadBar, cols: usize, rows: usize, jump_chord: &str) -> LeadFoot {
    let height = thegn_core::lead_bar::height(cols, rows);
    if height == 0 {
        return LeadFoot::default();
    }

    let g = crate::caps::active_glyphs();
    let (mark, mark_tok, word_tok) = match bar.state {
        LeadState::Working => (
            g.dot_filled,
            Tok::Slot(S::ActivityActive),
            Tok::Slot(S::Text),
        ),
        LeadState::Idle => (g.dot_hollow, Tok::Slot(S::Dim), Tok::Slot(S::Text)),
        LeadState::NotStarted => (g.dot_hollow, Tok::Slot(S::Ghost2), Tok::Slot(S::Ghost2)),
        LeadState::WaitingOnYou => (
            g.attention,
            Tok::State(StateRole::Warn),
            Tok::State(StateRole::Warn),
        ),
        LeadState::Paused => (
            g.dot_hollow,
            Tok::State(StateRole::Stale),
            Tok::State(StateRole::Stale),
        ),
        LeadState::Unknown => (
            g.warn,
            Tok::State(StateRole::Unknown),
            Tok::State(StateRole::Unknown),
        ),
        LeadState::Error => (
            g.cross,
            Tok::State(StateRole::Crit),
            Tok::State(StateRole::Crit),
        ),
    };

    let narrow = cols < 60;
    let chip = approval_seg(bar, g, narrow);
    let chip_width = chip
        .as_ref()
        .map(|s| seg::seg_width(std::slice::from_ref(s)));

    let lanes = match bar.lanes {
        Some(meter) => {
            let tok = if meter.full() {
                Tok::State(StateRole::Warn)
            } else {
                Tok::Slot(S::Text)
            };
            vec![seg(Tok::Slot(S::Dim), "lanes "), seg(tok, meter.text())]
        }
        None => vec![seg(Tok::State(StateRole::Unknown), "lanes ?")],
    };
    let spawns = spawn_segs(bar, narrow);

    let mut status = vec![
        sp(1),
        seg(mark_tok, mark),
        sp(1),
        seg(word_tok, bar.state.word()),
    ];
    if let Some(chip) = chip {
        status.extend([sp(2), chip, sp(2)]);
    }
    status.extend([sp(2)]);
    status.extend(lanes);
    status.extend([sp(2)]);
    status.extend(spawns);

    let chip_start = seg::seg_width(&[
        sp(1),
        seg(mark_tok, mark),
        sp(1),
        seg(word_tok, bar.state.word()),
        sp(2),
    ]);

    let line_a = Line::segs(status);
    let line_b = Line::split(last_segs(bar, g), hint_segs(bar, jump_chord, narrow, g));
    let lines = match height {
        1 => vec![Line::segs(single_line(
            mark,
            mark_tok,
            lanes_for_single(bar),
            chip_for_single(bar, g),
        ))],
        2 => vec![line_a, line_b],
        _ => vec![
            Line::segs(vec![
                sp(1),
                seg(Tok::Slot(S::Dim), "LEAD").bold(),
                sp(1),
                seg(Tok::Slot(S::Border), g.box_h.repeat(cols.saturating_sub(7))),
            ]),
            line_a,
            line_b,
        ],
    };

    let approvals = chip_width.map(|width| {
        let (row, start) = if height == 1 {
            (
                0,
                seg::seg_width(&[
                    sp(1),
                    seg(Tok::Slot(S::Dim), "LEAD"),
                    sp(1),
                    seg(mark_tok, mark),
                    sp(1),
                ]),
            )
        } else {
            (1, chip_start)
        };
        (row, start..start + width)
    });
    LeadFoot {
        rows: lines
            .into_iter()
            .map(|line| {
                PanelRow::plain(line)
                    .with_hit(PanelHit::LeadJump)
                    .with_bg(Tok::Slot(S::Panel2))
            })
            .collect(),
        approvals,
    }
}

fn approval_seg(bar: &LeadBar, g: &thegn_core::termcaps::GlyphSet, narrow: bool) -> Option<Seg> {
    if let Some(text) = bar.approvals_text() {
        if narrow {
            Some(seg(
                Tok::State(StateRole::Warn),
                format!(
                    " {} {} ",
                    g.attention,
                    bar.approvals_short().unwrap_or_default()
                ),
            ))
        } else {
            Some(Seg::chip(
                Tok::State(StateRole::Warn),
                format!(" {} {} ", g.attention, text),
            ))
        }
    } else if bar.approvals.is_none() {
        Some(seg(
            Tok::State(StateRole::Unknown),
            format!("{} ?", g.attention),
        ))
    } else {
        None
    }
}

fn spawn_segs(bar: &LeadBar, narrow: bool) -> Vec<Seg> {
    let Some(meter) = bar.spawns else {
        return if narrow {
            vec![seg(Tok::State(StateRole::Unknown), "?")]
        } else {
            vec![
                seg(Tok::Slot(S::Dim), "spawns "),
                seg(Tok::State(StateRole::Unknown), "?"),
            ]
        };
    };
    let width = if narrow { 5 } else { 8 };
    let (bar_s, track) = crate::caps::bar_track(meter.frac(), width);
    let bar_tok = if meter.full() {
        Tok::State(StateRole::Crit)
    } else if meter.frac() >= 0.75 {
        Tok::State(StateRole::Warn)
    } else {
        Tok::State(StateRole::Ok)
    };
    let mut out = Vec::new();
    if !narrow {
        out.push(seg(Tok::Slot(S::Dim), "spawns "));
    }
    out.extend([
        seg(bar_tok, bar_s),
        seg(Tok::Slot(S::Ghost3), track),
        sp(1),
        seg(Tok::Slot(S::Dim), meter.text()),
    ]);
    out
}

fn last_segs(bar: &LeadBar, g: &thegn_core::termcaps::GlyphSet) -> Vec<Seg> {
    if bar.state == LeadState::Error {
        return vec![
            seg(Tok::State(StateRole::Crit), g.cross),
            seg(
                Tok::State(StateRole::Crit),
                format!(" {}", bar.error.as_deref().unwrap_or("error")),
            ),
        ];
    }
    match (&bar.last, &bar.last_age) {
        (Some(last), Some(age)) => vec![
            seg(Tok::Slot(S::Dim), last),
            seg(Tok::Slot(S::Ghost3), format!(" {} ", g.middot)),
            seg(Tok::Slot(S::Ghost2), age),
        ],
        _ => vec![seg(Tok::Slot(S::Ghost2), "no Lead runs yet")],
    }
}

fn hint_segs(
    bar: &LeadBar,
    jump_chord: &str,
    narrow: bool,
    g: &thegn_core::termcaps::GlyphSet,
) -> Vec<Seg> {
    let mut out = Vec::new();
    if bar.state == LeadState::WaitingOnYou {
        out.extend([
            seg(Tok::Slot(S::Dim), "y/n"),
            seg(Tok::Slot(S::Ghost2), " approve/reject"),
            seg(Tok::Slot(S::Ghost3), format!(" {} ", g.middot)),
        ]);
    } else if !narrow {
        let command = if bar.state == LeadState::Paused {
            "thegn lead resume"
        } else {
            "thegn lead pause"
        };
        out.extend([
            seg(Tok::Slot(S::Dim), command),
            seg(Tok::Slot(S::Ghost3), format!(" {} ", g.middot)),
        ]);
    }
    if !out.is_empty() {
        out.push(seg(Tok::Slot(S::Dim), jump_chord));
        out.push(seg(Tok::Slot(S::Ghost2), " jump"));
    } else {
        out.push(seg(Tok::Slot(S::Dim), jump_chord));
    }
    out.push(sp(1));
    out
}

fn lanes_for_single(bar: &LeadBar) -> Vec<Seg> {
    match bar.lanes {
        Some(meter) => {
            let tok = if meter.full() {
                Tok::State(StateRole::Warn)
            } else {
                Tok::Slot(S::Text)
            };
            vec![seg(Tok::Slot(S::Dim), "lanes "), seg(tok, meter.text())]
        }
        None => vec![seg(Tok::State(StateRole::Unknown), "lanes ?")],
    }
}

fn chip_for_single(bar: &LeadBar, g: &thegn_core::termcaps::GlyphSet) -> Option<Seg> {
    if bar.approvals_text().is_some() {
        Some(seg(
            Tok::State(StateRole::Warn),
            format!(
                " {} {} ",
                g.attention,
                bar.approvals_short().unwrap_or_default()
            ),
        ))
    } else if bar.approvals.is_none() {
        Some(seg(
            Tok::State(StateRole::Unknown),
            format!("{} ?", g.attention),
        ))
    } else {
        None
    }
}

fn single_line(mark: &str, mark_tok: Tok, lanes: Vec<Seg>, chip: Option<Seg>) -> Vec<Seg> {
    let mut out = vec![
        sp(1),
        seg(Tok::Slot(S::Dim), "LEAD").bold(),
        sp(1),
        seg(mark_tok, mark),
        sp(1),
    ];
    if let Some(chip) = chip {
        out.push(chip);
        out.push(sp(1));
    }
    out.extend(lanes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::test_override::with_unicode;
    use crate::compositor::Rect;
    use thegn_core::lead_bar::{LeadState, Meter};
    use thegn_core::termcaps::UnicodeLevel;

    fn bar(state: LeadState, approvals: Option<u32>) -> LeadBar {
        LeadBar {
            state,
            tabs: 1,
            approvals,
            first_request: Some("THE-1 review".into()),
            lanes: Some(Meter { used: 2, limit: 4 }),
            spawns: Some(Meter { used: 3, limit: 8 }),
            last: Some("asked THE-1 review".into()),
            last_age: Some("2m".into()),
            error: None,
        }
    }

    fn text(line: &Line) -> String {
        match line {
            Line::Segs(segs) => segs.iter().map(|s| s.text.as_str()).collect(),
            Line::Split { l, r } | Line::SplitMinLeft { l, r, .. } => {
                l.iter().chain(r.iter()).map(|s| s.text.as_str()).collect()
            }
            _ => String::new(),
        }
    }

    #[test]
    fn footer_uses_height_boundaries() {
        assert!(
            footer(&bar(LeadState::Idle, None), 44, 6, "Alt i")
                .rows
                .is_empty()
        );
        assert_eq!(
            footer(&bar(LeadState::Idle, None), 44, 40, "Alt i")
                .rows
                .len(),
            2
        );
        let wide = footer(&bar(LeadState::Idle, None), 80, 40, "Alt i");
        assert_eq!(wide.rows.len(), 3);
        assert!(text(&wide.rows[0].line).contains("LEAD"));
        assert_eq!(
            footer(&bar(LeadState::Idle, None), 44, 12, "Alt i")
                .rows
                .len(),
            1
        );
    }

    #[test]
    fn waiting_footer_records_approval_chip_and_hints() {
        let foot = footer(&bar(LeadState::WaitingOnYou, Some(2)), 44, 40, "Alt i");
        assert!(text(&foot.rows[0].line).contains('2'));
        let (row, range) = foot.approvals.as_ref().expect("approval chip");
        assert_eq!(*row, 0);
        let line = match &foot.rows[*row].line {
            Line::Segs(segs) => segs,
            _ => panic!("status row is not a segment line"),
        };
        let mut x = 0;
        let inside: String = line
            .iter()
            .filter_map(|s| {
                let start = x;
                x += seg::seg_width(std::slice::from_ref(s));
                (start < range.end && range.start < x).then_some(s.text.as_str())
            })
            .collect();
        assert!(inside.contains(crate::caps::active_glyphs().attention));
        assert!(text(&foot.rows[1].line).contains("y/n"));
        assert!(text(&foot.rows[1].line).contains("Alt i"));
        assert!(range.start < range.end);
    }

    #[test]
    fn rows_have_panel_background_and_jump_hits() {
        for row in &footer(&bar(LeadState::Idle, None), 80, 40, "Alt i").rows {
            assert_eq!(row.bg, Some(Tok::Slot(S::Panel2)));
            assert_eq!(row.hit, Some(PanelHit::LeadJump));
        }
    }

    #[test]
    fn ascii_footer_uses_ascii_glyphs() {
        with_unicode(UnicodeLevel::Ascii, || {
            let foot = footer(&bar(LeadState::Working, Some(1)), 44, 40, "Alt i");
            let all = foot.rows.iter().map(|r| text(&r.line)).collect::<String>();
            assert!(!all.chars().any(|c| ('\u{2500}'..='\u{259f}').contains(&c)));
            assert!(all.contains('='));
        });
    }

    #[test]
    fn error_footer_shows_error_instead_of_last_action() {
        let mut lead = bar(LeadState::Error, None);
        lead.error = Some("launch failed".into());
        let foot = footer(&lead, 44, 40, "Alt i");
        assert!(text(&foot.rows[1].line).contains("launch failed"));
        assert!(!text(&foot.rows[1].line).contains("asked THE-1"));
    }

    #[test]
    fn chrome_hits_reserve_and_route_footer_rows() {
        let lead = bar(LeadState::WaitingOnYou, Some(2));
        let model = crate::chrome::FrameModel {
            sidebar_status: crate::sidebar::SidebarStatus {
                lead: Some(lead.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let ui = crate::panel::PanelUi::default();
        let rect = Rect::full(44, 40);
        let foot = footer(&lead, rect.cols, rect.rows, "Alt i");
        assert_eq!(
            crate::chrome::panel_hit_at(&model, &ui, rect, 3, rect.y + rect.rows - 1,),
            Some(PanelHit::LeadJump)
        );
        let (_, range) = foot.approvals.unwrap();
        assert_eq!(
            crate::chrome::panel_hit_at(&model, &ui, rect, range.start, rect.y + rect.rows - 2,),
            Some(PanelHit::LeadApprovals)
        );
        let no_lead = crate::chrome::FrameModel::default();
        assert_eq!(
            crate::chrome::panel_hits(&no_lead, &ui, rect),
            crate::panel::frame::build_panel(&no_lead, &ui, rect.cols, rect.rows, false)
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, row)| row.hit.map(|hit| (rect.y + i, hit)))
                .collect::<Vec<_>>()
        );
    }
}
