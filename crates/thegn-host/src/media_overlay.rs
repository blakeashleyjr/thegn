//! The Now-Playing overlay (optional `[media]` feature): a centered modal control
//! panel summoned with `Alt-m`. Shows cover art, a scrubber, a navigable
//! transport + volume, the up-next queue, and — for video sources — chapter and
//! fullscreen controls. Follows the `pr_view`/`palette` overlay convention: a
//! local `Option<MediaOverlay>` on the loop, drawn via `open_layer`, owning every
//! key while open (`handle_key` → outcome the loop maps to a media op).

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use thegn_core::media::{LoopMode, MediaState, PlaybackState, QueueItem};
use thegn_core::theme::Hue;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::media_art::ArtMosaic;
use crate::seg::{self, Line, Tok, seg, sp};

/// Cover-art mosaic size in cells (see [`crate::media_art`]).
pub(crate) const ART_COLS: usize = 18;
pub(crate) const ART_ROWS: usize = 8;

const BOX_COLS: usize = 60;
/// Max up-next rows shown at once.
const QUEUE_ROWS: usize = 6;

/// The live overlay state.
pub(crate) struct MediaOverlay {
    /// The now-playing snapshot, refreshed from the media watcher each tick.
    pub snapshot: Option<MediaState>,
    /// The up-next list (fetched on open where the backend exposes one).
    pub queue: Vec<QueueItem>,
    /// Decoded cover art, when fetched + it still matches the current track.
    pub art: Option<ArtMosaic>,
    /// Selected queue row.
    sel: usize,
}

/// What a key did to the overlay.
pub(crate) enum MediaOverlayOutcome {
    /// Consumed; overlay stays open, nothing else to do.
    Pending,
    /// Dismiss the overlay.
    Close,
    /// Run this transport op off the loop (overlay stays open).
    Op(OverlayOp),
}

/// A transport op the overlay asks the loop to perform. The loop maps these onto
/// its internal `MediaOp` (overlay can't name that private type).
pub(crate) enum OverlayOp {
    PlayPause,
    Next,
    Previous,
    SeekForward,
    SeekBack,
    /// Jump to an absolute position (digit 1-9 = 10-90%, 0 = start).
    SetPosition(std::time::Duration),
    Shuffle,
    Loop,
    /// Set an absolute volume percent (the -/+ slider).
    SetVolume(u8),
    ChapterNext,
    ChapterPrev,
    Fullscreen,
    PlayQueue(String),
}

impl MediaOverlay {
    pub(crate) fn open(snapshot: Option<MediaState>) -> Self {
        // Start the selection on the currently-playing queue entry (filled in
        // once the queue arrives).
        MediaOverlay {
            snapshot,
            queue: Vec::new(),
            art: None,
            sel: 0,
        }
    }

    /// The cover-art URL worth fetching for the current track, if the config
    /// allows art and the backend exposed one. The loop uses this on open (and
    /// after a track change) to kick off an async fetch.
    pub(crate) fn wants_art(&self, show_art: bool) -> Option<String> {
        if !show_art {
            return None;
        }
        let url = self.snapshot.as_ref()?.art_url.as_ref()?;
        // Already have (or fetched) art for this url? Skip.
        if self.art.as_ref().map(|a| a.url.as_str()) == Some(url.as_str()) {
            return None;
        }
        Some(url.clone())
    }

    /// Accept a delivered queue.
    pub(crate) fn set_queue(&mut self, queue: Vec<QueueItem>) {
        self.sel = queue.iter().position(|q| q.is_current).unwrap_or(0);
        self.queue = queue;
    }

    /// Accept a delivered art mosaic if it still matches the current track.
    pub(crate) fn set_art(&mut self, art: ArtMosaic) {
        let matches =
            self.snapshot.as_ref().and_then(|s| s.art_url.as_deref()) == Some(art.url.as_str());
        if matches {
            self.art = Some(art);
        }
    }

    pub(crate) fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> MediaOverlayOutcome {
        use MediaOverlayOutcome::*;
        let ctrl = mods.contains(Modifiers::CTRL);
        if ctrl && matches!(key, KeyCode::Char('c' | 'C' | 'g' | 'G')) {
            return Close;
        }
        let is_video = self
            .snapshot
            .as_ref()
            .map(|s| s.kind.is_video())
            .unwrap_or(false);
        match key {
            KeyCode::Escape => Close,
            KeyCode::Char(' ') => Op(OverlayOp::PlayPause),
            KeyCode::Char('n' | 'N') => Op(OverlayOp::Next),
            KeyCode::Char('p' | 'P') => Op(OverlayOp::Previous),
            KeyCode::LeftArrow | KeyCode::Char('h' | ',') => Op(OverlayOp::SeekBack),
            KeyCode::RightArrow | KeyCode::Char('l' | '.') => Op(OverlayOp::SeekForward),
            KeyCode::Char('s' | 'S') => Op(OverlayOp::Shuffle),
            KeyCode::Char('r' | 'R') => Op(OverlayOp::Loop),
            KeyCode::Char('-' | '_') => self.volume_op(-5),
            KeyCode::Char('+' | '=') => self.volume_op(5),
            // Digit scrub: jump to N·10% of the track (0 = start).
            KeyCode::Char(d @ '0'..='9') => self.scrub_to(*d),
            KeyCode::Char('f' | 'F') if is_video => Op(OverlayOp::Fullscreen),
            KeyCode::Char('[') if is_video => Op(OverlayOp::ChapterPrev),
            KeyCode::Char(']') if is_video => Op(OverlayOp::ChapterNext),
            KeyCode::DownArrow | KeyCode::Char('j' | 'J') => {
                if !self.queue.is_empty() {
                    self.sel = (self.sel + 1).min(self.queue.len() - 1);
                }
                Pending
            }
            KeyCode::UpArrow | KeyCode::Char('k' | 'K') => {
                self.sel = self.sel.saturating_sub(1);
                Pending
            }
            KeyCode::Enter => match self.queue.get(self.sel) {
                Some(item) => Op(OverlayOp::PlayQueue(item.id.clone())),
                None => Op(OverlayOp::PlayPause),
            },
            _ => Pending,
        }
    }

    /// Set an absolute volume `delta` percent from the current level (the -/+
    /// slider), clamped to `0..=100`. `Pending` when the backend reports no
    /// volume (nothing to move).
    fn volume_op(&self, delta: i16) -> MediaOverlayOutcome {
        match self.snapshot.as_ref().and_then(|s| s.volume) {
            Some(cur) => {
                let next = (cur as i16 + delta).clamp(0, 100) as u8;
                MediaOverlayOutcome::Op(OverlayOp::SetVolume(next))
            }
            None => MediaOverlayOutcome::Pending,
        }
    }

    /// Scrub to `digit`·10% of the track length (0 → start). `Pending` when the
    /// length is unknown or the backend can't seek.
    fn scrub_to(&self, digit: char) -> MediaOverlayOutcome {
        let Some(m) = &self.snapshot else {
            return MediaOverlayOutcome::Pending;
        };
        let Some(len) = m.length.filter(|_| m.can_seek) else {
            return MediaOverlayOutcome::Pending;
        };
        let frac = digit.to_digit(10).unwrap_or(0) as f64 / 10.0;
        MediaOverlayOutcome::Op(OverlayOp::SetPosition(len.mul_f64(frac)))
    }

    /// Rows the content occupies, for sizing the layer box.
    fn content_rows(&self) -> usize {
        let is_video = self
            .snapshot
            .as_ref()
            .map(|s| s.kind.is_video())
            .unwrap_or(false);
        let head = ART_ROWS.max(4); // art column height (or metadata block)
        let controls = 1 /*scrubber*/ + 1 /*transport*/ + 1 /*toggles*/ + usize::from(is_video);
        let queue = if self.queue.is_empty() {
            0
        } else {
            2 + self.queue.len().min(QUEUE_ROWS) // rule + header + items
        };
        head + controls + queue + 1 /*footer*/
    }

    pub(crate) fn render(&self, surface: &mut Surface, screen: Rect) {
        let title = self
            .snapshot
            .as_ref()
            .map(|s| s.now_playing())
            .unwrap_or_else(|| "Nothing playing".into());
        let spec = LayerSpec {
            title: format!("Now Playing \u{2014} {title}"),
            badge: Some(" esc ".into()),
            cols: BOX_COLS,
            rows: self.content_rows(),
            anchor: Anchor::Center,
            dim: true,
            shadow: true,
            bg: Tok::Slot(S::Panel),
            border: Tok::Slot(S::Accent),
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        let Some(m) = &self.snapshot else {
            seg::draw_line(
                surface,
                inner.x,
                inner.y,
                inner.cols,
                &Line::segs(vec![seg(Tok::Slot(S::Dim), "No player is loaded.")]),
                panel,
            );
            return;
        };

        // --- Head: cover art (left) + metadata (right) ---
        let has_art = self.art.is_some();
        let meta_x = if has_art {
            inner.x + ART_COLS + 2
        } else {
            inner.x
        };
        let meta_w = inner.cols.saturating_sub(meta_x - inner.x);
        if let Some(art) = &self.art {
            for (row, line) in art.lines.iter().enumerate().take(ART_ROWS) {
                seg::draw_line(surface, inner.x, inner.y + row, ART_COLS, line, panel);
            }
        }
        // Metadata block.
        let glyph_fg = match m.state {
            PlaybackState::Playing => Tok::Hue(Hue::Green),
            PlaybackState::Paused => Tok::Hue(Hue::Amber),
            PlaybackState::Stopped => Tok::Slot(S::Faint),
        };
        let mut my = inner.y;
        seg::draw_line(
            surface,
            meta_x,
            my,
            meta_w,
            &Line::segs(vec![
                seg(glyph_fg, format!("{} ", m.state.glyph())),
                seg(Tok::Slot(S::Text), m.title.clone()).bold(),
            ]),
            panel,
        );
        my += 1;
        if !m.artist.is_empty() {
            seg::draw_line(
                surface,
                meta_x,
                my,
                meta_w,
                &Line::segs(vec![seg(Tok::Slot(S::Dim), m.artist.clone())]),
                panel,
            );
            my += 1;
        }
        if !m.album.is_empty() {
            seg::draw_line(
                surface,
                meta_x,
                my,
                meta_w,
                &Line::segs(vec![seg(Tok::Slot(S::Faint), m.album.clone())]),
                panel,
            );
            my += 1;
        }
        // A "video" tag + source, on the next metadata line.
        let mut tags = vec![seg(Tok::Slot(S::Ghost2), format!("via {}", m.player))];
        if m.kind.is_video() {
            tags.push(seg(Tok::Slot(S::Ghost), "  ·  "));
            tags.push(seg(Tok::Hue(Hue::Purple), "video"));
        }
        seg::draw_line(surface, meta_x, my, meta_w, &Line::segs(tags), panel);

        // Body starts below the head block.
        let mut y = inner.y + ART_ROWS.max(4);

        // --- Scrubber ---
        if let (Some(pos), Some(len)) = (m.position, m.length)
            && len.as_secs() > 0
        {
            let frac = (pos.as_secs_f32() / len.as_secs_f32()).clamp(0.0, 1.0);
            let stamp = m.position_stamp().unwrap_or_default();
            let bar_w = inner.cols.saturating_sub(stamp.len() + 2).clamp(8, 40);
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(scrubber_segs(frac, bar_w, m.can_seek, stamp)),
                panel,
            );
        } else {
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![seg(Tok::Slot(S::Ghost2), "(no position reported)")]),
                panel,
            );
        }
        y += 1;

        // --- Transport keycaps ---
        let mut tr: Vec<seg::Seg> = Vec::new();
        if m.can_go_previous {
            tr.push(seg(Tok::Slot(S::Text), "\u{23ee}"));
            tr.push(seg(Tok::Slot(S::Ghost2), " p  "));
        }
        if m.can_seek {
            tr.push(seg(Tok::Slot(S::Text), "\u{23ea}"));
            tr.push(seg(Tok::Slot(S::Ghost2), " \u{2190}  "));
        }
        tr.push(seg(Tok::Hue(Hue::Green), m.state.glyph()));
        tr.push(seg(Tok::Slot(S::Ghost2), " space  "));
        if m.can_seek {
            tr.push(seg(Tok::Slot(S::Text), "\u{23e9}"));
            tr.push(seg(Tok::Slot(S::Ghost2), " \u{2192}  "));
        }
        if m.can_go_next {
            tr.push(seg(Tok::Slot(S::Text), "\u{23ed}"));
            tr.push(seg(Tok::Slot(S::Ghost2), " n"));
        }
        seg::draw_line(surface, inner.x, y, inner.cols, &Line::segs(tr), panel);
        y += 1;

        // --- Toggles: shuffle / loop / volume ---
        let mut tg: Vec<seg::Seg> = Vec::new();
        if let Some(on) = m.shuffle {
            tg.push(seg(
                if on {
                    Tok::Hue(Hue::Green)
                } else {
                    Tok::Slot(S::Faint)
                },
                "\u{1f500}",
            ));
            tg.push(seg(
                Tok::Slot(S::Ghost2),
                if on { " s:on   " } else { " s:off   " },
            ));
        }
        if let Some(lm) = m.loop_mode {
            let label = match lm {
                LoopMode::None => "off",
                LoopMode::Track => "track",
                LoopMode::Playlist => "all",
            };
            tg.push(seg(
                if matches!(lm, LoopMode::None) {
                    Tok::Slot(S::Faint)
                } else {
                    Tok::Hue(Hue::Green)
                },
                "\u{1f501}",
            ));
            tg.push(seg(Tok::Slot(S::Ghost2), format!(" r:{label}   ")));
        }
        if let Some(v) = m.volume {
            tg.push(seg(Tok::Slot(S::Text), "\u{1f50a}"));
            tg.push(seg(Tok::Slot(S::Ghost2), " -/+ "));
            tg.push(seg(Tok::Slot(S::Dim), format!("{v}%")));
        }
        if !tg.is_empty() {
            seg::draw_line(surface, inner.x, y, inner.cols, &Line::segs(tg), panel);
            y += 1;
        }

        // --- Video row ---
        if m.kind.is_video() {
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![
                    seg(Tok::Slot(S::Text), "\u{23ee}\u{23ed}"),
                    seg(Tok::Slot(S::Ghost2), " [ ] chapter    "),
                    seg(Tok::Slot(S::Text), "\u{26f6}"),
                    seg(Tok::Slot(S::Ghost2), " f fullscreen"),
                ]),
                panel,
            );
            y += 1;
        }

        // --- Queue / up-next ---
        if !self.queue.is_empty() {
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::Fill {
                    ch: '\u{254c}',
                    fg: Tok::Slot(S::Ghost3),
                },
                panel,
            );
            y += 1;
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![seg(Tok::Slot(S::Ghost2), "Up next")]),
                panel,
            );
            y += 1;
            let start = self.sel.saturating_sub(QUEUE_ROWS - 1);
            for (i, item) in self.queue.iter().enumerate().skip(start).take(QUEUE_ROWS) {
                let selected = i == self.sel;
                let pad = if selected { Tok::SelAccent } else { panel };
                let marker = if item.is_current { "\u{25b8} " } else { "  " };
                let name = if selected {
                    seg(Tok::Slot(S::Text), item.label()).bold()
                } else {
                    seg(Tok::Slot(S::Dim), item.label())
                };
                seg::draw_line(
                    surface,
                    inner.x,
                    y,
                    inner.cols,
                    &Line::segs(vec![seg(Tok::Hue(Hue::Green), marker), name]),
                    pad,
                );
                y += 1;
            }
        }

        // --- Footer ---
        let footer = Line::split(
            vec![
                seg(Tok::Slot(S::Ghost2), "\u{2191}\u{2193}"),
                seg(Tok::Slot(S::Ghost), " queue   "),
                seg(Tok::Slot(S::Ghost2), "\u{21b5}"),
                seg(Tok::Slot(S::Ghost), " play   "),
                seg(Tok::Slot(S::Ghost2), "esc"),
                seg(Tok::Slot(S::Ghost), " close"),
            ],
            vec![sp(0)],
        );
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows.saturating_sub(1),
            inner.cols,
            &footer,
            panel,
        );
    }
}

/// A scrubber line: `m:ss ━━━●───── m:ss`. Dim knob when the backend can't seek.
fn scrubber_segs(frac: f32, w: usize, can_seek: bool, stamp: String) -> Vec<seg::Seg> {
    let filled = ((frac * w as f32).round() as usize).min(w);
    let knob_fg = if can_seek {
        Tok::Hue(Hue::Green)
    } else {
        Tok::Slot(S::Faint)
    };
    let mut segs = Vec::new();
    segs.push(seg(knob_fg, "\u{2501}".repeat(filled.saturating_sub(1))));
    if filled > 0 {
        segs.push(seg(knob_fg, "\u{25cf}")); // ●
    }
    segs.push(seg(Tok::Slot(S::Ghost3), "\u{2500}".repeat(w - filled)));
    segs.push(seg(Tok::Slot(S::Dim), format!("  {stamp}")));
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MediaState {
        MediaState {
            player: "mpv".into(),
            title: "Clip".into(),
            state: PlaybackState::Playing,
            can_seek: true,
            kind: thegn_core::media::MediaKind::Video,
            ..Default::default()
        }
    }

    #[test]
    fn queue_nav_clamps() {
        let mut ov = MediaOverlay::open(Some(sample()));
        ov.set_queue(vec![
            QueueItem {
                id: "0".into(),
                title: "a".into(),
                ..Default::default()
            },
            QueueItem {
                id: "1".into(),
                title: "b".into(),
                is_current: true,
                ..Default::default()
            },
        ]);
        assert_eq!(ov.sel, 1); // starts on current
        // Down clamps at the end.
        let _ = ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE);
        assert_eq!(ov.sel, 1);
        // Up moves to 0, and again clamps.
        let _ = ov.handle_key(&KeyCode::UpArrow, Modifiers::NONE);
        assert_eq!(ov.sel, 0);
        let _ = ov.handle_key(&KeyCode::UpArrow, Modifiers::NONE);
        assert_eq!(ov.sel, 0);
    }

    #[test]
    fn enter_on_queue_plays_selected() {
        let mut ov = MediaOverlay::open(Some(sample()));
        ov.set_queue(vec![QueueItem {
            id: "abc".into(),
            title: "a".into(),
            ..Default::default()
        }]);
        match ov.handle_key(&KeyCode::Enter, Modifiers::NONE) {
            MediaOverlayOutcome::Op(OverlayOp::PlayQueue(id)) => assert_eq!(id, "abc"),
            _ => panic!("expected PlayQueue"),
        }
    }

    #[test]
    fn video_only_keys_gate() {
        let mut ov = MediaOverlay::open(Some(sample()));
        assert!(matches!(
            ov.handle_key(&KeyCode::Char('f'), Modifiers::NONE),
            MediaOverlayOutcome::Op(OverlayOp::Fullscreen)
        ));
        // Audio: 'f' does nothing.
        let mut audio = sample();
        audio.kind = thegn_core::media::MediaKind::Audio;
        let mut ov = MediaOverlay::open(Some(audio));
        assert!(matches!(
            ov.handle_key(&KeyCode::Char('f'), Modifiers::NONE),
            MediaOverlayOutcome::Pending
        ));
    }
}
