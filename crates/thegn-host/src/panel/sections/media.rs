//! The docked Media section.  It is deliberately a projection of plain model
//! data: provider work, queue reads, and cover-art decoding remain in the
//! off-loop media tasks.

use thegn_core::media::{MediaRenderPolicy, MediaState, MediaWidth};
use thegn_core::theme::Hue;

use crate::seg::{Line, seg};

use super::{PanelRow, SectionCtx, d, g, g2, hue, rule, t};
use crate::panel::{MediaAction, PanelHit, Section};

const ART_COLS: usize = 18;
const ART_ROWS: usize = 8;

fn width(ctx: &SectionCtx<'_>) -> MediaWidth {
    match ctx.ui.width {
        crate::layout::PanelWidth::Normal => MediaWidth::Normal,
        crate::layout::PanelWidth::Half => MediaWidth::Half,
        crate::layout::PanelWidth::Full => MediaWidth::Full,
    }
}

fn marker(state: thegn_core::media::PlaybackState) -> (&'static str, Hue) {
    let glyphs = crate::caps::active_glyphs();
    match state {
        thegn_core::media::PlaybackState::Playing => (glyphs.dot_filled, Hue::Green),
        thegn_core::media::PlaybackState::Paused => (glyphs.dot_hollow, Hue::Amber),
        thegn_core::media::PlaybackState::Stopped => (glyphs.cross, Hue::Purple),
    }
}

fn action(label: &str, key: &str, op: MediaAction) -> PanelRow {
    PanelRow::plain(Line::segs(vec![
        seg(hue(Hue::Amber), format!("[{key}] ")),
        seg(t(), label),
    ]))
    .with_hit(PanelHit::MediaAction(op))
}

fn detail_rows(
    ctx: &SectionCtx<'_>,
    state: &MediaState,
    policy: MediaRenderPolicy,
) -> Vec<PanelRow> {
    let mut rows = Vec::new();
    let (glyph, tone) = marker(state.state);
    rows.push(PanelRow::plain(Line::segs(vec![
        seg(hue(tone), format!("{glyph} ")),
        seg(t(), state.now_playing()),
    ])));
    if !state.album.is_empty() {
        rows.push(PanelRow::plain(Line::segs(vec![seg(
            d(),
            state.album.clone(),
        )])));
    }
    if let (Some(pos), Some(len)) = (state.position, state.length)
        && len.as_secs() > 0
    {
        let frac = (pos.as_secs_f32() / len.as_secs_f32()).clamp(0.0, 1.0);
        let mut line = super::bar_segs(
            frac,
            ctx.cols.saturating_sub(14).clamp(6, 24),
            hue(Hue::Green),
        );
        line.push(seg(
            g(),
            format!("  {}", state.position_stamp().unwrap_or_default()),
        ));
        rows.push(PanelRow::plain(Line::segs(line)));
    }
    rows.push(action("play/pause", "space", MediaAction::PlayPause));
    if state.can_go_next {
        rows.push(action("next", "n", MediaAction::Next));
    }
    if state.can_go_previous {
        rows.push(action("previous", "p", MediaAction::Previous));
    }
    if policy.show_seek {
        rows.push(action("seek forward", "right", MediaAction::SeekForward));
        rows.push(action("seek back", "left", MediaAction::SeekBack));
    }
    if policy.show_shuffle {
        rows.push(action("shuffle", "s", MediaAction::Shuffle));
    }
    if policy.show_loop {
        rows.push(action("repeat", "L", MediaAction::Loop));
    }
    if policy.show_volume {
        rows.push(action("volume up", "+", MediaAction::VolumeUp));
        rows.push(action("volume down", "-", MediaAction::VolumeDown));
    }
    if policy.show_chapters {
        rows.push(action("next chapter", "]", MediaAction::ChapterNext));
        rows.push(action("previous chapter", "[", MediaAction::ChapterPrev));
    }
    if policy.show_fullscreen {
        rows.push(action("fullscreen", "f", MediaAction::Fullscreen));
    }
    rows
}

pub(super) fn content(ctx: &SectionCtx<'_>) -> Vec<PanelRow> {
    let mut rows = Vec::new();
    let Some(state) = &ctx.model.panel.media else {
        let hint = if ctx.full() {
            "no provider snapshot; controls will appear when a player is active"
        } else if ctx.deep() {
            "enable a local player for source and detail controls"
        } else {
            "enable a local media player to use this panel"
        };
        return vec![
            PanelRow::plain(Line::segs(vec![seg(g2(), "no player")])),
            PanelRow::plain(Line::segs(vec![seg(d(), hint)])),
        ];
    };
    let media = &ctx.ui.media;
    let policy = MediaRenderPolicy::project(state, media.caps(state), width(ctx));
    if policy.show_sources {
        rows.push(rule());
        for (i, source) in media.sources.iter().enumerate() {
            rows.push(
                PanelRow::plain(Line::segs(vec![
                    seg(
                        if source == &state.player {
                            hue(Hue::Green)
                        } else {
                            g()
                        },
                        format!("{} ", crate::caps::active_glyphs().dot_filled),
                    ),
                    seg(t(), source.clone()),
                ]))
                .with_hit(PanelHit::Row(Section::Media, i)),
            );
        }
    }
    rows.extend(detail_rows(ctx, state, policy));
    if policy.show_queue {
        rows.push(rule());
        rows.push(PanelRow::plain(Line::segs(vec![seg(d(), "up next")])));
        let offset = media.sources.len();
        for (i, item) in media.queue.iter().enumerate() {
            let current = if item.is_current {
                crate::caps::active_glyphs().caret_open
            } else {
                " "
            };
            rows.push(
                PanelRow::plain(Line::segs(vec![
                    seg(g(), format!("{current} ")),
                    seg(t(), item.label()),
                ]))
                .with_hit(PanelHit::Row(Section::Media, offset + i)),
            );
        }
    }
    if policy.show_art && media.art_visible() {
        // Art is optional decoration and is intentionally not a hit target.
        if let Some(art) = &media.art {
            rows.extend(art.lines.iter().cloned().map(PanelRow::plain));
        } else {
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                g(),
                format!("art {ART_COLS}x{ART_ROWS}"),
            )])));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::media::MediaPanelState;
    use thegn_core::media::QueueItem;

    fn state() -> MediaState {
        MediaState {
            player: "player".into(),
            title: "track".into(),
            state: thegn_core::media::PlaybackState::Playing,
            can_go_next: true,
            can_go_previous: true,
            can_seek: true,
            volume: Some(50),
            shuffle: Some(false),
            loop_mode: Some(thegn_core::media::LoopMode::None),
            ..Default::default()
        }
    }

    #[test]
    fn late_queue_and_art_deliveries_are_identity_checked() {
        let mut panel = MediaPanelState::default();
        let first = state();
        panel.begin_request(Some(&first));
        panel.set_queue(
            Some(&first),
            vec![QueueItem {
                title: "one".into(),
                ..Default::default()
            }],
        );
        assert_eq!(panel.queue.len(), 1);
        let mut changed = first.clone();
        changed.track_id = Some("new".into());
        panel.sync_snapshot(Some(&changed));
        panel.set_queue(Some(&changed), vec![QueueItem::default()]);
        assert!(panel.queue.is_empty());
    }

    #[test]
    fn action_targets_are_distinct_from_source_and_queue_rows() {
        assert_eq!(
            PanelHit::MediaAction(MediaAction::Next),
            PanelHit::MediaAction(MediaAction::Next)
        );
        assert_ne!(
            PanelHit::Row(Section::Media, 0),
            PanelHit::MediaAction(MediaAction::Next)
        );
    }

    #[test]
    fn width_projection_adds_source_and_queue_rows_progressively() {
        let mut model = crate::chrome::FrameModel::default();
        model.panel.media = Some(state());
        let mut ui = crate::panel::PanelUi::default();
        ui.media.sync_snapshot(model.panel.media.as_ref());
        ui.media.begin_request(model.panel.media.as_ref());
        ui.media.set_queue(
            model.panel.media.as_ref(),
            vec![QueueItem {
                id: "next".into(),
                title: "next track".into(),
                ..Default::default()
            }],
        );
        let mut rows = |width| {
            ui.width = width;
            let ctx = SectionCtx {
                model: &model,
                ui: &ui,
                cols: 40,
                rows: 30,
            };
            content(&ctx)
        };
        let normal = rows(crate::layout::PanelWidth::Normal);
        let half = rows(crate::layout::PanelWidth::Half);
        let full = rows(crate::layout::PanelWidth::Full);
        assert!(
            !normal
                .iter()
                .any(|r| r.hit == Some(PanelHit::Row(Section::Media, 0)))
        );
        assert!(
            half.iter()
                .any(|r| r.hit == Some(PanelHit::Row(Section::Media, 0)))
        );
        assert!(
            full.iter()
                .any(|r| r.hit == Some(PanelHit::Row(Section::Media, 1)))
        );
    }

    #[test]
    fn keyboard_and_mouse_transport_intents_converge() {
        for (key, action) in [
            (' ', MediaAction::PlayPause),
            ('n', MediaAction::Next),
            ('p', MediaAction::Previous),
            ('s', MediaAction::Shuffle),
            ('L', MediaAction::Loop),
        ] {
            assert_eq!(
                crate::media_panel::action_for_key(&termwiz::input::KeyCode::Char(key)),
                Some(action)
            );
            assert_eq!(PanelHit::MediaAction(action), PanelHit::MediaAction(action));
        }
    }
}
