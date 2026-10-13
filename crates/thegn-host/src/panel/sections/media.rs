//! The docked Media section.  It is deliberately a projection of plain model
//! data: provider work, queue reads, and cover-art decoding remain in the
//! off-loop media tasks.

use thegn_core::media::{MediaRenderPolicy, MediaState, MediaWidth, display_text};
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
            display_text(&state.album),
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
    let policy = MediaRenderPolicy::project(state, media.caps().unwrap_or_default(), width(ctx));
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
                    seg(t(), display_text(source)),
                ]))
                .with_hit(PanelHit::Row(Section::Media, i)),
            );
        }
        if media.sources_truncated {
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                d(),
                "+more sources not shown",
            )])));
        }
    }
    rows.extend(detail_rows(ctx, state, policy));
    if policy.show_queue {
        rows.push(rule());
        rows.push(PanelRow::plain(Line::segs(vec![seg(d(), "up next")])));
        let offset = media.sources.len();
        if media.queue_error.is_some() {
            rows.push(PanelRow::plain(Line::segs(vec![seg(
                d(),
                "queue unavailable",
            )])));
        } else {
            let cursor = if ctx.ui.open == Section::Media
                && (offset..offset + media.queue.len()).contains(&ctx.ui.cursor)
            {
                ctx.ui.cursor.saturating_sub(offset)
            } else {
                0
            };
            let radius = ctx.rows.max(8).saturating_mul(2).saturating_add(2);
            let lo = cursor.saturating_sub(radius);
            let hi = cursor
                .saturating_add(radius)
                .saturating_add(1)
                .min(media.queue.len());
            for (i, item) in media.queue.iter().enumerate() {
                if i < lo || i >= hi {
                    rows.push(
                        PanelRow::plain(Line::Segs(Vec::new()))
                            .with_hit(PanelHit::Row(Section::Media, offset + i)),
                    );
                    continue;
                }
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
            if media.queue_truncated > 0 {
                rows.push(PanelRow::plain(Line::segs(vec![seg(
                    d(),
                    format!("+{} more not shown", media.queue_truncated),
                )])));
            }
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
    use thegn_core::media::{MediaCaps, QueueItem};

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

    fn collection_fixture(queue_len: usize) -> (crate::chrome::FrameModel, crate::panel::PanelUi) {
        let mut model = crate::chrome::FrameModel::default();
        model.panel.media = Some(state());
        let mut ui = crate::panel::PanelUi {
            tab: crate::panel::PanelTab::System,
            width: crate::layout::PanelWidth::Full,
            ..Default::default()
        };
        ui.media.sync_snapshot(
            model.panel.media.as_ref(),
            Some(MediaCaps {
                queue: true,
                ..MediaCaps::default()
            }),
            0,
        );
        let source_request = ui
            .media
            .take_sources_request(model.panel.media.as_ref())
            .unwrap();
        ui.media.set_sources(
            source_request,
            model.panel.media.as_ref(),
            vec!["player".into()],
        );
        ui.media.begin_request(model.panel.media.as_ref());
        let queue_request = ui
            .media
            .take_queue_request(model.panel.media.as_ref())
            .unwrap();
        ui.media.set_queue(
            queue_request,
            model.panel.media.as_ref(),
            Ok((0..queue_len)
                .map(|index| QueueItem {
                    id: index.to_string(),
                    title: format!("track-{index}"),
                    ..Default::default()
                })
                .collect()),
        );
        (model, ui)
    }

    fn line_text(line: &Line) -> String {
        match line {
            Line::Segs(segs) | Line::Split { l: segs, .. } => {
                segs.iter().map(|part| part.text.as_str()).collect()
            }
            Line::SplitMinLeft { l, r, .. } => {
                l.iter().chain(r).map(|part| part.text.as_str()).collect()
            }
            Line::Blank | Line::Fill { .. } => String::new(),
        }
    }

    #[test]
    fn late_queue_and_art_deliveries_are_identity_checked() {
        let mut panel = MediaPanelState::default();
        let first = state();
        panel.begin_request(Some(&first));
        let first_request = panel.take_queue_request(Some(&first)).unwrap();
        panel.set_queue(
            first_request.clone(),
            Some(&first),
            Ok(vec![QueueItem {
                title: "one".into(),
                ..Default::default()
            }]),
        );
        assert_eq!(panel.queue.len(), 1);
        let mut changed = first.clone();
        changed.track_id = Some("new".into());
        let stale_request = panel
            .take_sources_request(Some(&first))
            .unwrap_or_else(|| panic!("source request expected"));
        panel.sync_snapshot(Some(&changed), None, 0);
        panel.set_queue(
            first_request,
            Some(&changed),
            Ok(vec![QueueItem::default()]),
        );
        assert!(panel.queue.is_empty());
        panel.set_sources(stale_request, Some(&changed), vec!["stale".into()]);
        assert!(panel.sources.is_empty());
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
        ui.media.sync_snapshot(
            model.panel.media.as_ref(),
            Some(MediaCaps {
                queue: true,
                ..MediaCaps::default()
            }),
            1,
        );
        let source_request = ui
            .media
            .take_sources_request(model.panel.media.as_ref())
            .unwrap();
        ui.media.set_sources(
            source_request,
            model.panel.media.as_ref(),
            vec!["player".into()],
        );
        ui.media.begin_request(model.panel.media.as_ref());
        let request = ui
            .media
            .take_queue_request(model.panel.media.as_ref())
            .unwrap();
        ui.media.set_queue(
            request,
            model.panel.media.as_ref(),
            Ok(vec![QueueItem {
                id: "next".into(),
                title: "next track".into(),
                ..Default::default()
            }]),
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
    fn capped_collection_rows_show_truncation_without_hit_targets() {
        let mut model = crate::chrome::FrameModel::default();
        model.panel.media = Some(state());
        let mut ui = crate::panel::PanelUi {
            width: crate::layout::PanelWidth::Full,
            ..Default::default()
        };
        ui.media.sync_snapshot(
            model.panel.media.as_ref(),
            Some(MediaCaps {
                queue: true,
                ..MediaCaps::default()
            }),
            0,
        );
        let source_request = ui
            .media
            .take_sources_request(model.panel.media.as_ref())
            .unwrap();
        ui.media.set_sources(
            source_request,
            model.panel.media.as_ref(),
            (0..thegn_core::media::MAX_SOURCES + 1)
                .map(|index| format!("source-{index}"))
                .collect(),
        );
        let queue_request = ui
            .media
            .take_queue_request(model.panel.media.as_ref())
            .unwrap();
        ui.media.set_queue(
            queue_request,
            model.panel.media.as_ref(),
            Ok((0..thegn_core::media::MAX_QUEUE_ITEMS + 2)
                .map(|index| QueueItem {
                    id: index.to_string(),
                    ..Default::default()
                })
                .collect()),
        );
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 40,
            rows: 30,
        };
        let rows = content(&ctx);
        let rendered_text = rows
            .iter()
            .filter_map(|row| match &row.line {
                Line::Segs(segs) => Some(
                    segs.iter()
                        .map(|part| part.text.as_str())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            rendered_text
                .iter()
                .any(|text| text.as_str() == "+more sources not shown")
        );
        assert!(
            rendered_text
                .iter()
                .any(|text| text.as_str() == "+2 more not shown")
        );
        assert!(rows.iter().filter_map(|row| row.hit).all(|hit| {
            !matches!(hit, PanelHit::Row(Section::Media, index)
                if index >= ui.media.sources.len() + thegn_core::media::MAX_QUEUE_ITEMS)
        }));
    }

    #[test]
    fn queue_rows_keep_navigation_hits_but_only_render_near_cursor() {
        let (model, mut ui) = collection_fixture(thegn_core::media::MAX_QUEUE_ITEMS);
        ui.open = Section::Media;
        let sources = ui.media.sources.len();
        let radius = 30_usize * 2 + 2;

        ui.cursor = 0;
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 30,
        };
        let rows = content(&ctx);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row.hit, Some(PanelHit::Row(Section::Media, _))))
                .count(),
            sources + thegn_core::media::MAX_QUEUE_ITEMS
        );
        assert!(
            rows.iter()
                .filter(|row| matches!(row.hit, Some(PanelHit::Row(Section::Media, index)) if index >= sources))
                .filter(|row| !line_text(&row.line).is_empty())
                .count()
                <= 2 * radius + 1
        );

        ui.cursor = sources + 400;
        let ctx = SectionCtx {
            model: &model,
            ui: &ui,
            cols: 80,
            rows: 30,
        };
        let rows = content(&ctx);
        let cursor_row = rows
            .iter()
            .find(|row| row.hit == Some(PanelHit::Row(Section::Media, ui.cursor)))
            .unwrap();
        assert!(line_text(&cursor_row.line).contains("track-400"));
    }

    #[test]
    fn built_media_panel_never_displays_placeholder_hits() {
        let (model, mut ui) = collection_fixture(450);
        ui.open = Section::Media;
        ui.cursor = ui.media.sources.len() + 400;
        for width in [
            crate::layout::PanelWidth::Full,
            crate::layout::PanelWidth::Half,
        ] {
            ui.width = width;
            let frame = crate::panel::frame::build_panel(&model, &ui, 60, 40, true);
            let hit_rows = frame
                .rows
                .iter()
                .filter(|row| matches!(row.hit, Some(PanelHit::Row(Section::Media, _))))
                .collect::<Vec<_>>();
            assert!(!hit_rows.is_empty());
            assert!(
                hit_rows
                    .iter()
                    .all(|row| !line_text(&row.line).trim().is_empty())
            );
            if width == crate::layout::PanelWidth::Full {
                assert!(hit_rows.iter().any(|row| {
                    row.hit == Some(PanelHit::Row(Section::Media, ui.cursor))
                        && line_text(&row.line).contains("track-400")
                }));
            }
        }
    }

    #[test]
    fn stale_queue_cursor_renders_head_rows_in_full_panel() {
        let (model, mut ui) = collection_fixture(thegn_core::media::MAX_QUEUE_ITEMS);
        ui.open = Section::Media;
        let sources = ui.media.sources.len();
        ui.cursor = sources + thegn_core::media::MAX_QUEUE_ITEMS + 50;

        let frame = crate::panel::frame::build_panel(&model, &ui, 60, 40, true);
        let queue_rows = frame
            .rows
            .iter()
            .filter(|row| {
                matches!(row.hit, Some(PanelHit::Row(Section::Media, index)) if index >= sources)
            })
            .collect::<Vec<_>>();

        assert!(!queue_rows.is_empty());
        assert!(
            queue_rows
                .iter()
                .all(|row| !line_text(&row.line).trim().is_empty())
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
            ('+', MediaAction::VolumeUp),
            ('-', MediaAction::VolumeDown),
            (']', MediaAction::ChapterNext),
            ('[', MediaAction::ChapterPrev),
            ('f', MediaAction::Fullscreen),
        ] {
            assert_eq!(
                crate::media_panel::action_for_key(&termwiz::input::KeyCode::Char(key)),
                Some(action)
            );
            assert_eq!(PanelHit::MediaAction(action), PanelHit::MediaAction(action));
        }
        for (key, action) in [
            (
                termwiz::input::KeyCode::RightArrow,
                MediaAction::SeekForward,
            ),
            (termwiz::input::KeyCode::LeftArrow, MediaAction::SeekBack),
        ] {
            assert_eq!(crate::media_panel::action_for_key(&key), Some(action));
            assert_eq!(PanelHit::MediaAction(action), PanelHit::MediaAction(action));
        }
    }

    #[test]
    fn every_painted_media_key_is_a_keyboard_key() {
        use termwiz::input::KeyCode;

        let source = include_str!("media.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("media renderer has a production section");
        let mut rest = source;
        let mut found = 0;
        while let Some(pos) = rest.find("action(\"") {
            rest = &rest[pos + "action(".len()..];
            let end = rest.find(')').expect("action call closes");
            let call = &rest[..end];
            let literals: Vec<&str> = call
                .split('"')
                .enumerate()
                .filter(|(index, _)| index % 2 == 1)
                .map(|(_, literal)| literal)
                .collect();
            let key = *literals.get(1).expect("painted action has a key label");
            let variant = call
                .split("MediaAction::")
                .nth(1)
                .expect("painted action has a MediaAction")
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .next()
                .unwrap();
            let code = match key {
                "space" => KeyCode::Char(' '),
                "right" => KeyCode::RightArrow,
                "left" => KeyCode::LeftArrow,
                chord => {
                    let mut chars = chord.chars();
                    let key = chars.next().expect("painted key is not empty");
                    assert!(chars.next().is_none(), "unsupported painted key {chord:?}");
                    KeyCode::Char(key)
                }
            };
            let actual = crate::media_panel::action_for_key(&code)
                .expect("every painted Media key must work on the keyboard");
            assert_eq!(format!("{actual:?}"), variant, "{key}");
            found += 1;
            rest = &rest[end + 1..];
        }
        assert!(found > 0, "the Media renderer still paints action rows");
    }
}
