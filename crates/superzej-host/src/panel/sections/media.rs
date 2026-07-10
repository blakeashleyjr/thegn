//! The Media section (optional `[media]` feature): now-playing, a progress bar,
//! and the transport/shuffle/loop/volume state of the controlled player. Hidden
//! unless `[media] enabled`; empty when nothing is loaded.

use superzej_core::media::{LoopMode, MediaKind, PlaybackState};
use superzej_core::theme::Hue;

use crate::seg::{Line, seg, sp};

use super::{PanelRow, SectionCtx, bar_segs, d, g, g2, hint_row, hue, rule, t};

pub(super) fn content(ctx: &SectionCtx) -> Vec<PanelRow> {
    let mut rows: Vec<PanelRow> = Vec::new();
    // The full-width body leads with a seam, like the other full section bodies,
    // so the three width tiers render distinctly.
    if ctx.full() {
        rows.push(rule());
    }
    let Some(m) = &ctx.model.panel.media else {
        rows.push(PanelRow::plain(Line::segs(vec![seg(g(), "no player")])));
        rows.push(hint_row(&[
            ("⏯", "play/pause"),
            ("⏭", "next"),
            ("↵", "panel"),
        ]));
        if ctx.deep() {
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                g2(),
                "start a player (Spotify, mpv, VLC…); Alt-m ↵ opens the control panel".to_string(),
            )])));
        }
        return rows;
    };

    // Now-playing: ▶/❚❚ glyph + Artist — Title.
    let glyph_fg = match m.state {
        PlaybackState::Playing => hue(Hue::Green),
        PlaybackState::Paused => hue(Hue::Amber),
        PlaybackState::Stopped => g2(),
    };
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(glyph_fg, format!("{} ", m.state.glyph())),
        seg(t(), m.now_playing()),
    ])));

    if !m.album.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(d(), m.album.clone())])));
    }

    // Progress bar + position stamp, when the player exposes a position.
    if let (Some(pos), Some(len)) = (m.position, m.length)
        && len.as_secs() > 0
    {
        let frac = (pos.as_secs_f32() / len.as_secs_f32()).clamp(0.0, 1.0);
        let w = ctx.cols.saturating_sub(14).clamp(6, 24);
        let mut line = bar_segs(frac, w, hue(Hue::Green));
        line.insert(0, sp(0));
        if let Some(stamp) = m.position_stamp() {
            line.push(seg(g(), format!("  {stamp}")));
        }
        rows.push(PanelRow::plain(Line::segs(line)));
    }

    // Status line: shuffle / loop / volume, only the parts the backend reports.
    let mut status: Vec<crate::seg::Seg> = Vec::new();
    if let Some(on) = m.shuffle {
        status.push(seg(if on { hue(Hue::Green) } else { g() }, "🔀"));
        status.push(seg(g(), if on { " on" } else { " off" }));
    }
    if let Some(lm) = m.loop_mode {
        if !status.is_empty() {
            status.push(seg(g(), "  ·  "));
        }
        let label = match lm {
            LoopMode::None => "loop off",
            LoopMode::Track => "loop track",
            LoopMode::Playlist => "loop all",
        };
        status.push(seg(
            if matches!(lm, LoopMode::None) {
                g()
            } else {
                hue(Hue::Green)
            },
            "🔁",
        ));
        status.push(seg(g(), format!(" {label}")));
    }
    if let Some(v) = m.volume {
        if !status.is_empty() {
            status.push(seg(g(), "  ·  "));
        }
        status.push(seg(g(), format!("vol {v}%")));
    }
    if !status.is_empty() {
        rows.push(PanelRow::plain(Line::segs(status)));
    }

    // Video sources get a dedicated affordance row (chapters + fullscreen).
    if matches!(m.kind, MediaKind::Video) {
        rows.push(hint_row(&[("⏮⏭", "chapter"), ("⛶", "fullscreen")]));
    }

    // Transport hints: seek is offered only when the backend can seek.
    if m.can_seek {
        rows.push(hint_row(&[
            ("⏯", "play/pause"),
            ("⏪⏩", "seek"),
            ("↵", "panel"),
        ]));
    } else {
        rows.push(hint_row(&[
            ("⏯", "play/pause"),
            ("⏭", "next"),
            ("↵", "panel"),
        ]));
    }
    // Wider tiers add a dim source footer (also makes the three widths distinct).
    if ctx.deep() {
        let mut foot = vec![seg(g2(), format!("via {}", m.player))];
        if matches!(m.kind, MediaKind::Video) {
            foot.push(seg(g(), "  ·  "));
            foot.push(seg(hue(Hue::Purple), "video"));
        }
        rows.push(PanelRow::plain(Line::segs(foot)));
    }
    rows
}
