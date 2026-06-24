//! Summoned-layer compositing: dim the backdrop, cast a shadow, paint a
//! boxed panel — the mockup's recipe ("1 dim backdrop · 2 paint panel ·
//! 3 cast shadow") for modals and the palette (the confirm dialog and the
//! command palette).
//!
//! Dimming and shadows work by cell replacement: read the composed cells back
//! from the scratch surface and re-emit them remapped, exactly how a real TUI
//! repaints. The remap is deterministic, so after the frame a layer opens,
//! damage tracking only re-emits cells whose backdrop actually changed.

use termwiz::cell::CellAttributes;
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::{Change, Position, Surface};

use crate::chrome::{self, S};
use crate::compositor::Rect;
use crate::seg::{self, Seg, Tok};

/// How much of a foreground color survives dimming.
const DIM_FG_KEEP: f32 = 0.35;
/// How much of an explicit background color survives dimming.
const DIM_BG_KEEP: f32 = 0.45;

fn parse_rgb(frag: &str) -> SrgbaTuple {
    let mut it = frag.split(';').filter_map(|s| s.trim().parse::<u8>().ok());
    match (it.next(), it.next(), it.next()) {
        (Some(r), Some(g), Some(b)) => {
            SrgbaTuple(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
        }
        _ => SrgbaTuple(0.0, 0.0, 0.0, 1.0),
    }
}

fn lerp(a: SrgbaTuple, b: SrgbaTuple, keep: f32) -> SrgbaTuple {
    // keep = how much of `a` survives over base `b`.
    SrgbaTuple(
        b.0 + (a.0 - b.0) * keep,
        b.1 + (a.1 - b.1) * keep,
        b.2 + (a.2 - b.2) * keep,
        1.0,
    )
}

fn truecolor(t: SrgbaTuple) -> ColorAttribute {
    ColorAttribute::TrueColorWithDefaultFallback(t)
}

/// Remap a foreground toward bg0. Non-truecolor cells (terminal default /
/// palette-indexed pane content) can't be lerped, so they collapse to the
/// ghost2 step of the ramp — total, deterministic.
fn dim_fg(c: ColorAttribute, bg0: SrgbaTuple, ghost2: SrgbaTuple) -> ColorAttribute {
    match c {
        ColorAttribute::TrueColorWithDefaultFallback(t)
        | ColorAttribute::TrueColorWithPaletteFallback(t, _) => {
            truecolor(lerp(t, bg0, DIM_FG_KEEP))
        }
        ColorAttribute::PaletteIndex(_) | ColorAttribute::Default => truecolor(ghost2),
    }
}

/// Remap a background toward bg0. Default/indexed backgrounds become bg0.
fn dim_bg(c: ColorAttribute, bg0: SrgbaTuple) -> ColorAttribute {
    match c {
        ColorAttribute::TrueColorWithDefaultFallback(t)
        | ColorAttribute::TrueColorWithPaletteFallback(t, _) => {
            truecolor(lerp(t, bg0, DIM_BG_KEEP))
        }
        ColorAttribute::PaletteIndex(_) | ColorAttribute::Default => truecolor(bg0),
    }
}

/// One row-run of repainted cells with shared attributes.
struct Run {
    x: usize,
    y: usize,
    text: String,
    attrs: CellAttributes,
}

fn flush_runs(surface: &mut Surface, runs: Vec<Run>) {
    for run in runs {
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(run.x),
            y: Position::Absolute(run.y),
        });
        surface.add_change(Change::AllAttributes(run.attrs));
        surface.add_change(Change::Text(run.text));
    }
    surface.add_change(Change::AllAttributes(CellAttributes::default()));
}

/// Repaint every cell in `rect` through `remap(fg, bg) -> (fg', bg')`,
/// keeping glyphs. Attributes other than color are dropped — the backdrop
/// reads as flat, faint structure.
fn repaint_rect(
    surface: &mut Surface,
    rect: Rect,
    remap: impl Fn(ColorAttribute, ColorAttribute) -> (ColorAttribute, ColorAttribute),
) {
    let mut runs: Vec<Run> = Vec::new();
    {
        let cells = surface.screen_cells();
        for y in rect.y..rect.y + rect.rows {
            let Some(row) = cells.get(y) else { break };
            let mut current: Option<Run> = None;
            for x in rect.x..rect.x + rect.cols {
                let Some(cell) = row.get(x) else { break };
                let (fg, bg) = remap(cell.attrs().foreground(), cell.attrs().background());
                let mut attrs = CellAttributes::default();
                attrs.set_foreground(fg);
                attrs.set_background(bg);
                let glyph = cell.str();
                let glyph = if glyph.is_empty() { " " } else { glyph };
                match &mut current {
                    Some(run) if run.attrs == attrs => run.text.push_str(glyph),
                    _ => {
                        if let Some(done) = current.take() {
                            runs.push(done);
                        }
                        current = Some(Run {
                            x,
                            y,
                            text: glyph.to_string(),
                            attrs,
                        });
                    }
                }
            }
            if let Some(done) = current.take() {
                runs.push(done);
            }
        }
    }
    flush_runs(surface, runs);
}

/// Dim-repaint `rect`: every cell re-emitted in the faint palette, glyphs
/// kept — content stays legible as structure (the mockup's dim backdrop).
pub fn dim_rect(surface: &mut Surface, rect: Rect) {
    let (bg0, ghost2) = chrome::with_palette(|p| (parse_rgb(&p.bg0), parse_rgb(&p.ghost2)));
    repaint_rect(surface, rect, |fg, bg| {
        (dim_fg(fg, bg0, ghost2), dim_bg(bg, bg0))
    });
}

/// Cast the 1-cell-offset shadow of box `r`: a 2-col strip to its right
/// (one row down) and the row below it (two cols in), repainted shadow-fg on
/// shadow-bg with glyphs kept. Clipped to `screen`.
pub fn shadow_of(surface: &mut Surface, r: Rect, screen: Rect) {
    let (sfg, sbg) = chrome::with_palette(|p| {
        (
            chrome::theme_color(&p.shadow_fg),
            chrome::theme_color(&p.shadow_bg),
        )
    });
    let remap = move |_fg: ColorAttribute, _bg: ColorAttribute| (sfg, sbg);
    let clip = |rect: Rect| -> Option<Rect> {
        let x1 = (rect.x + rect.cols).min(screen.x + screen.cols);
        let y1 = (rect.y + rect.rows).min(screen.y + screen.rows);
        if rect.x >= x1 || rect.y >= y1 {
            return None;
        }
        Some(Rect {
            x: rect.x,
            y: rect.y,
            cols: x1 - rect.x,
            rows: y1 - rect.y,
        })
    };
    // Right strip: rows r.y+1 ..= r.y+r.rows (the extra row is the corner
    // shared with the bottom strip).
    if let Some(right) = clip(Rect {
        x: r.x + r.cols,
        y: r.y + 1,
        cols: 2,
        rows: r.rows,
    }) {
        repaint_rect(surface, right, remap);
    }
    // Bottom strip: the row below the box, shifted two cols right.
    if let Some(bottom) = clip(Rect {
        x: r.x + 2,
        y: r.y + r.rows,
        cols: r.cols.saturating_sub(2),
        rows: 1,
    }) {
        repaint_rect(surface, bottom, remap);
    }
}

/// Where a layer's box sits on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    /// Horizontally and vertically centered.
    #[default]
    Center,
    /// Centered, with the box's top at ~1/4 of the screen (palette position).
    TopThird,
    /// Horizontally centered, near the bottom (toast position, above the
    /// statusbar).
    Bottom,
}

/// A summoned layer: boxed, filled, optionally dimming the backdrop and
/// casting a shadow.
#[derive(Debug, Clone)]
pub struct LayerSpec {
    pub title: String,
    /// A key chip embedded in the top border's right corner (e.g. " t ").
    pub badge: Option<String>,
    /// Content size in cells; the box adds a 1-cell border + 1-cell pad each
    /// side horizontally and a 1-cell border vertically. Clamped to screen.
    pub cols: usize,
    pub rows: usize,
    pub anchor: Anchor,
    pub dim: bool,
    pub shadow: bool,
    /// Interior fill (default the panel surface — mockup b2).
    pub bg: Tok,
    /// Border color (hot layers pass `Tok::Slot(S::Accent)`).
    pub border: Tok,
}

impl Default for LayerSpec {
    fn default() -> Self {
        LayerSpec {
            title: String::new(),
            badge: None,
            cols: 40,
            rows: 10,
            anchor: Anchor::Center,
            dim: true,
            shadow: true,
            bg: Tok::Slot(S::Panel),
            border: Tok::Slot(S::Faint),
        }
    }
}

/// Compose a layer onto `surface`: dim → shadow → fill → rounded border +
/// title + badge. Returns the interior content rect for the caller's
/// `seg::draw_lines`. `None` when the screen is too small for any box.
pub fn open_layer(surface: &mut Surface, screen: Rect, spec: &LayerSpec) -> Option<Rect> {
    if screen.cols < 8 || screen.rows < 4 {
        return None;
    }
    let cols = spec.cols.min(screen.cols.saturating_sub(6));
    let rows = spec.rows.min(screen.rows.saturating_sub(3));
    let bw = cols + 4; // border + 1 pad each side
    let bh = rows + 2; // top/bottom border
    let bx = screen.x + (screen.cols - bw) / 2;
    let by = match spec.anchor {
        Anchor::Center => screen.y + (screen.rows - bh) / 2,
        Anchor::TopThird => screen.y + (screen.rows / 4).min(screen.rows - bh),
        // One row of breathing room above the bottom edge (the statusbar sits
        // outside `screen` for overlays, so this hugs the content's bottom).
        Anchor::Bottom => screen.y + screen.rows.saturating_sub(bh + 1),
    };
    let boxr = Rect {
        x: bx,
        y: by,
        cols: bw,
        rows: bh,
    };

    if spec.dim {
        dim_rect(surface, screen);
    }
    if spec.shadow {
        shadow_of(surface, boxr, screen);
    }

    let (border, bg, title_col) = chrome::with_palette(|p| {
        (
            spec.border.resolve(p),
            spec.bg.resolve(p),
            Tok::Slot(S::Text).resolve(p),
        )
    });
    chrome::fill(surface, boxr, bg);
    crate::borders::draw_card(
        surface,
        boxr,
        &spec.title,
        &crate::borders::CardStyle {
            border,
            title: title_col,
            bg,
        },
    );
    // Key-chip badge in the top border, right-aligned.
    if let Some(badge) = &spec.badge {
        let chip = Seg::key(badge.clone());
        let bl = badge.chars().count();
        if bw > bl + 4 {
            seg::draw_line(
                surface,
                bx + bw - bl - 3,
                by,
                bl,
                &crate::seg::Line::segs(vec![chip]),
                spec.bg,
            );
        }
    }
    Some(Rect {
        x: bx + 2,
        y: by + 1,
        cols,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seg::{Line, seg};

    fn surface_with_text(cols: usize, rows: usize, text: &str) -> Surface {
        let mut s = Surface::new(cols, rows);
        for y in 0..rows {
            seg::draw_line(
                &mut s,
                0,
                y,
                cols,
                &Line::segs(vec![seg(Tok::Slot(S::Text), text)]),
                Tok::Slot(S::Bg0),
            );
        }
        s
    }

    fn row_text(s: &mut Surface, y: usize) -> String {
        s.screen_cells()[y].iter().map(|c| c.str()).collect()
    }

    fn fg_at(s: &mut Surface, x: usize, y: usize) -> ColorAttribute {
        s.screen_cells()[y][x].attrs().foreground()
    }

    fn bg_at(s: &mut Surface, x: usize, y: usize) -> ColorAttribute {
        s.screen_cells()[y][x].attrs().background()
    }

    #[test]
    fn dim_keeps_glyphs_and_remaps_colors() {
        let mut s = surface_with_text(10, 2, "hello");
        let before_fg = fg_at(&mut s, 0, 0);
        dim_rect(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 10,
                rows: 2,
            },
        );
        assert_eq!(row_text(&mut s, 0), "hello     ");
        assert_ne!(fg_at(&mut s, 0, 0), before_fg, "fg must be remapped");
    }

    #[test]
    fn dim_is_deterministic_on_same_content() {
        let mut a = surface_with_text(8, 1, "x");
        let mut b = surface_with_text(8, 1, "x");
        let r = Rect {
            x: 0,
            y: 0,
            cols: 8,
            rows: 1,
        };
        dim_rect(&mut a, r);
        dim_rect(&mut b, r);
        assert_eq!(fg_at(&mut a, 0, 0), fg_at(&mut b, 0, 0));
        assert_eq!(bg_at(&mut a, 0, 0), bg_at(&mut b, 0, 0));
    }

    #[test]
    fn dim_collapses_default_colors_to_ramp() {
        // Untouched surface cells carry Default fg/bg — the remap is total.
        let mut s = Surface::new(4, 1);
        dim_rect(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 4,
                rows: 1,
            },
        );
        assert_ne!(fg_at(&mut s, 0, 0), ColorAttribute::Default);
        assert_ne!(bg_at(&mut s, 0, 0), ColorAttribute::Default);
    }

    #[test]
    fn shadow_paints_right_strip_and_bottom_row() {
        let mut s = surface_with_text(20, 8, "....................");
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 20,
            rows: 8,
        };
        let boxr = Rect {
            x: 2,
            y: 1,
            cols: 10,
            rows: 4,
        };
        let shadow_bg = chrome::with_palette(|p| chrome::theme_color(&p.shadow_bg));
        shadow_of(&mut s, boxr, screen);
        // Right strip: x 12..14, rows 2..=5.
        assert_eq!(bg_at(&mut s, 12, 2), shadow_bg);
        assert_eq!(bg_at(&mut s, 13, 5), shadow_bg);
        // Bottom strip: row 5, x 4..12.
        assert_eq!(bg_at(&mut s, 4, 5), shadow_bg);
        assert_eq!(bg_at(&mut s, 11, 5), shadow_bg);
        // Outside the shadow: untouched.
        assert_ne!(bg_at(&mut s, 0, 0), shadow_bg);
        assert_ne!(bg_at(&mut s, 12, 1), shadow_bg, "strip starts a row down");
        // Glyphs survive inside the shadow.
        assert_eq!(row_text(&mut s, 5).chars().nth(4), Some('.'));
    }

    #[test]
    fn shadow_clips_at_screen_edges() {
        let mut s = surface_with_text(10, 4, "..........");
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 10,
            rows: 4,
        };
        // Box flush against the right/bottom — shadow would fall off-screen.
        let boxr = Rect {
            x: 2,
            y: 1,
            cols: 8,
            rows: 3,
        };
        shadow_of(&mut s, boxr, screen); // must not panic
    }

    #[test]
    fn open_layer_returns_interior_and_draws_box() {
        let mut s = surface_with_text(40, 12, "censored backdrop content here....");
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 12,
        };
        let spec = LayerSpec {
            title: "jump".into(),
            badge: Some(" ⌘K ".into()),
            cols: 20,
            rows: 4,
            ..LayerSpec::default()
        };
        let inner = open_layer(&mut s, screen, &spec).expect("layer fits");
        assert_eq!(inner.cols, 20);
        assert_eq!(inner.rows, 4);
        // Border row carries the title.
        let top = row_text(&mut s, inner.y - 1);
        assert!(top.contains("jump"), "{top:?}");
        assert!(top.contains("⌘K"), "{top:?}");
        assert!(top.contains('╭') && top.contains('╮'), "{top:?}");
        // Interior is blank (filled).
        let mid: String = row_text(&mut s, inner.y)
            .chars()
            .skip(inner.x)
            .take(inner.cols)
            .collect();
        assert_eq!(mid.trim(), "");
    }

    #[test]
    fn open_layer_clamps_to_small_screens() {
        let mut s = Surface::new(30, 8);
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 30,
            rows: 8,
        };
        let spec = LayerSpec {
            cols: 100,
            rows: 50,
            ..LayerSpec::default()
        };
        let inner = open_layer(&mut s, screen, &spec).expect("clamped layer fits");
        assert!(inner.cols <= 24);
        assert!(inner.rows <= 5);
        // Tiny screens refuse politely.
        let mut tiny = Surface::new(6, 3);
        assert!(
            open_layer(
                &mut tiny,
                Rect {
                    x: 0,
                    y: 0,
                    cols: 6,
                    rows: 3
                },
                &spec
            )
            .is_none()
        );
    }
}
