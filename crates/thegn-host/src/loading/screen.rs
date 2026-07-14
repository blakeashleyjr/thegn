//! The loading-screen BODY layout: gauge rule, timeline-rail step rows with a
//! right-aligned elapsed column, floating progress/hint/error sub-lines, and
//! the context block — everything below the wordmark while `load_steps` is
//! non-empty. Pure surface painting: a function of (rect, steps, context,
//! caps, ambient clock); the splash-scoped ticker merely causes repaints.
//!
//! Layout contract (the no-bounce guarantee, locked by tests): for a given
//! plan the block height is CONSTANT — `steps.len()` rows plus a fixed
//! [`SUBLINE_RESERVE`] that floats under the cursor step (progress bar,
//! hint, or wrapped error) and renders blank otherwise. State transitions can
//! never change the height, so the wordmark above never re-centers.
//!
//! ```text
//! ─────────────── 3/5 · 0:41 ───────────────      gauge
//! ✓ connect podman                     0.3s
//! ✓ resolve image                      0.8s
//! ◐ pull ghcr.io/blake/dev:latest       38s       ← cursor
//! │   ▓▓▓▓▓▓▓░░░░░░░░░░  142 MB / 380 MB          ← floating sub-lines
//! │   network-bound — a cold pull can …
//! ◇ create container
//! ◇ shell
//!
//!   env        gpu-sprite                          context block
//!   placement  provider:sprites
//! ```

use termwiz::color::ColorAttribute;
use termwiz::surface::Surface;
use unicode_width::UnicodeWidthStr;

use crate::chrome::{self, LoadStep, S, StepState, col};
use crate::compositor::Rect;
use crate::loading::{catalog, plan};

/// Rows always reserved for the cursor step's floating sub-lines.
pub(crate) const SUBLINE_RESERVE: usize = 2;
/// Fixed width of the right-aligned elapsed column (fits `12m34s`).
const ELAPSED_W: usize = 6;
/// Content-height floor: absorbs the generic materialize seed (3 steps) being
/// replaced by a longer backend-aware plan without the wordmark jumping.
const RESERVE_MIN: usize = 12;
/// Progress-bar cell budget.
const BAR_W: usize = 20;

/// Total body rows the loading layout will occupy for this plan — the value
/// the splash reserves so its vertical anchor is a pure function of the plan
/// (not of tick-by-tick state).
pub(crate) fn reserved_rows(steps: &[LoadStep], ctx: &[(String, String)]) -> usize {
    let ctx_rows = if ctx.is_empty() { 0 } else { 1 + ctx.len() };
    (1 + steps.len() + SUBLINE_RESERVE + ctx_rows).max(RESERVE_MIN)
}

/// The cursor step: the running one, else the failure, else none. Sub-lines
/// float under it.
fn cursor(steps: &[LoadStep]) -> Option<usize> {
    steps
        .iter()
        .position(|s| s.state == StepState::Active)
        .or_else(|| steps.iter().position(|s| s.state == StepState::Failed))
}

/// One floating sub-line: colored segments drawn after the rail glyph.
struct SubLine {
    segs: Vec<(String, ColorAttribute)>,
}

/// The (up to [`SUBLINE_RESERVE`]) sub-lines for the cursor step: a failed
/// step wraps its error in red; an active step shows its progress bar and its
/// detail (falling back to the catalog's slow-step hint past the threshold).
fn sublines(step: &LoadStep, accent: ColorAttribute, width: usize) -> Vec<SubLine> {
    let mut out = Vec::new();
    if step.state == StepState::Failed {
        let err = step.detail.clone().unwrap_or_else(|| "failed".into());
        let red = chrome::theme_color(thegn_core::theme::RED);
        for line in wrap(&err, width, SUBLINE_RESERVE) {
            out.push(SubLine {
                segs: vec![(line, red)],
            });
        }
        return out;
    }
    if step.state != StepState::Active {
        return out;
    }
    if let Some((done, total)) = step.progress {
        let bytes = match total {
            Some(t) => format!("  {} / {}", plan::fmt_bytes(done), plan::fmt_bytes(t)),
            None => format!("  {}", plan::fmt_bytes(done)),
        };
        let bar_w = BAR_W.min(width.saturating_sub(UnicodeWidthStr::width(bytes.as_str())));
        let frac = total.filter(|t| *t > 0).map(|t| done as f64 / t as f64);
        let (fill, empty) = plan::bar(bar_w, frac);
        out.push(SubLine {
            segs: vec![(fill, accent), (empty, col(S::Ghost)), (bytes, col(S::Dim))],
        });
    }
    // Detail (live status) wins over the generic slow hint; either renders
    // dim. The hint appears once the step's elapsed crosses its threshold —
    // sampled at draw time, so the ticker surfaces it with no extra state.
    let text = match &step.detail {
        // The byte progress already renders on the bar line — don't repeat it.
        Some(d) if step.progress.is_none() => Some(d.clone()),
        _ => step
            .started_at
            .and_then(|t| catalog::slow_hint(step.kind, t.elapsed()))
            .map(str::to_string),
    };
    if let Some(t) = text
        && out.len() < SUBLINE_RESERVE
    {
        let mut line = t;
        if UnicodeWidthStr::width(line.as_str()) > width {
            line = truncate(&line, width);
        }
        out.push(SubLine {
            segs: vec![(line, col(S::Faint))],
        });
    }
    out.truncate(SUBLINE_RESERVE);
    out
}

/// The elapsed-column text for a step: live for the running one, the frozen
/// duration for finished ones, blank for pending.
fn elapsed_text(step: &LoadStep) -> Option<String> {
    match step.state {
        StepState::Active => step.started_at.map(|t| plan::fmt_elapsed(t.elapsed())),
        StepState::Done | StepState::Failed => step.took.map(plan::fmt_elapsed),
        StepState::Pending => None,
    }
}

/// Draw the loading body into `rect` starting at row `y0`. Returns nothing;
/// clamps to the rect. `y0` comes from the splash's centering math (which
/// used [`reserved_rows`], so the body always fits its reservation).
pub(crate) fn draw_body(
    surface: &mut Surface,
    rect: Rect,
    y0: usize,
    steps: &[LoadStep],
    ctx: &[(String, String)],
    accent: ColorAttribute,
    bg: ColorAttribute,
) {
    if steps.is_empty() {
        return;
    }
    let g = crate::caps::active_glyphs();
    let bottom = rect.y + rect.rows;
    let max_label = steps
        .iter()
        .map(|s| UnicodeWidthStr::width(s.label.as_str()))
        .max()
        .unwrap_or(0);
    // glyph(1) + space(1) + label + gap(2) + elapsed column.
    let needed = (2 + max_label + 2 + ELAPSED_W).max(26);
    let cap = rect.cols.saturating_sub(4).clamp(12, 56);
    let block_w = needed.min(cap);
    let bx = rect.x + rect.cols.saturating_sub(block_w) / 2;
    let label_budget = block_w.saturating_sub(2 + 2 + ELAPSED_W);

    // ── gauge ────────────────────────────────────────────────────────────
    let done = steps.iter().filter(|s| s.state == StepState::Done).count();
    let failed = steps.iter().any(|s| s.state == StepState::Failed);
    let total_elapsed = steps
        .iter()
        .filter_map(|s| s.started_at)
        .map(|t| t.elapsed())
        .max()
        .map(plan::fmt_elapsed);
    let mid = match (failed, &total_elapsed) {
        (true, Some(e)) => format!(" {} failed · {} ", g.cross, e),
        (true, None) => format!(" {} failed ", g.cross),
        (false, Some(e)) => format!(" {}/{} · {} ", done, steps.len(), e),
        (false, None) => format!(" {}/{} ", done, steps.len()),
    };
    let mid_fg = if failed {
        chrome::theme_color(thegn_core::theme::RED)
    } else {
        col(S::Dim)
    };
    if y0 < bottom {
        let mid_w = UnicodeWidthStr::width(mid.as_str()).min(block_w);
        let side = block_w.saturating_sub(mid_w);
        let (l, r) = (side / 2, side - side / 2);
        chrome::draw_text(surface, bx, y0, &g.box_h.repeat(l), col(S::Ghost), bg, l);
        chrome::draw_text(surface, bx + l, y0, &mid, mid_fg, bg, mid_w);
        chrome::draw_text(
            surface,
            bx + l + mid_w,
            y0,
            &g.box_h.repeat(r),
            col(S::Ghost),
            bg,
            r,
        );
    }

    // ── step rows + floating sub-lines ──────────────────────────────────
    let cur = cursor(steps);
    let mut y = y0 + 1;
    for (i, step) in steps.iter().enumerate() {
        if y >= bottom {
            break;
        }
        let (glyph, glyph_fg) = plan::visual_glyph_live(step.state, accent, true);
        chrome::draw_text(surface, bx, y, glyph, glyph_fg, bg, 1);
        let label = truncate(&step.label, label_budget);
        chrome::draw_text(
            surface,
            bx + 2,
            y,
            &label,
            plan::label_color(step.state),
            bg,
            label_budget,
        );
        if let Some(e) = elapsed_text(step) {
            let w = UnicodeWidthStr::width(e.as_str()).min(ELAPSED_W);
            let fg = if step.state == StepState::Active {
                col(S::Dim)
            } else {
                col(S::Ghost)
            };
            chrome::draw_text(surface, bx + block_w - w, y, &e, fg, bg, w);
        }
        y += 1;
        if cur == Some(i) {
            let lines = sublines(step, accent, block_w.saturating_sub(4));
            for j in 0..SUBLINE_RESERVE {
                if y >= bottom {
                    break;
                }
                if let Some(line) = lines.get(j) {
                    chrome::draw_text(surface, bx, y, g.box_v, col(S::Ghost), bg, 1);
                    let mut x = bx + 4;
                    for (text, fg) in &line.segs {
                        chrome::draw_text(
                            surface,
                            x,
                            y,
                            text,
                            *fg,
                            bg,
                            (bx + block_w).saturating_sub(x),
                        );
                        x += UnicodeWidthStr::width(text.as_str());
                    }
                }
                y += 1;
            }
        }
    }
    // No cursor (all pending / all done): the reserve rows sit blank at the
    // block's end — same height either way.
    if cur.is_none() {
        y += SUBLINE_RESERVE;
    }

    // ── context block ────────────────────────────────────────────────────
    if ctx.is_empty() {
        return;
    }
    y += 1; // gap
    let key_w = ctx
        .iter()
        .map(|(k, _)| UnicodeWidthStr::width(k.as_str()))
        .max()
        .unwrap_or(0);
    for (k, v) in ctx {
        if y >= bottom {
            break;
        }
        let kx = bx + 2;
        chrome::draw_text(surface, kx, y, k, col(S::Ghost), bg, key_w);
        let vx = kx + key_w + 2;
        chrome::draw_text(
            surface,
            vx,
            y,
            v,
            col(S::Dim),
            bg,
            (rect.x + rect.cols).saturating_sub(vx),
        );
        y += 1;
    }
}

/// The one-line status for the Small/Text splash variants:
/// `◐ 3/5 pull image · 37% · 38s`, parts dropped right-to-left elsewhere by
/// clipping. Returns colored segments for `centered_parts`-style rendering.
pub(crate) fn compact_line(
    steps: &[LoadStep],
    accent: ColorAttribute,
) -> Vec<(String, ColorAttribute)> {
    let Some(step) = cursor(steps).map(|i| &steps[i]).or_else(|| steps.last()) else {
        return Vec::new();
    };
    let done = steps.iter().filter(|s| s.state == StepState::Done).count();
    let (glyph, glyph_fg) = plan::visual_glyph_live(step.state, accent, true);
    let mut parts = vec![
        (format!("{glyph} "), glyph_fg),
        (format!("{}/{} ", done, steps.len()), col(S::Dim)),
        (step.label.clone(), plan::label_color(step.state)),
    ];
    let middot = crate::caps::active_glyphs().middot;
    if step.state == StepState::Failed {
        parts.push((
            format!(" {middot} failed"),
            chrome::theme_color(thegn_core::theme::RED),
        ));
        return parts;
    }
    if let Some((d, Some(t))) = step.progress
        && t > 0
    {
        parts.push((
            format!(
                " {middot} {}%",
                (d as f64 / t as f64 * 100.0).round() as u64
            ),
            col(S::Dim),
        ));
    }
    if let Some(e) = elapsed_text(step) {
        parts.push((format!(" {middot} {e}"), col(S::Ghost)));
    }
    parts
}

/// Truncate `s` to `width` display columns, ellipsized via the caps glyph.
fn truncate(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    let ell = crate::caps::active_glyphs().ellipsis;
    let ell_w = UnicodeWidthStr::width(ell);
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = UnicodeWidthStr::width(c.to_string().as_str());
        if w + cw > width.saturating_sub(ell_w) {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push_str(ell);
    out
}

/// Greedy word-wrap into at most `max_lines` lines of `width` columns; the
/// last line is truncated if the text keeps going.
fn wrap(s: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let need = if cur.is_empty() { 0 } else { 1 } + UnicodeWidthStr::width(word);
        if !cur.is_empty() && UnicodeWidthStr::width(cur.as_str()) + need > width {
            lines.push(std::mem::take(&mut cur));
            if lines.len() == max_lines {
                break;
            }
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() && lines.len() < max_lines {
        lines.push(cur);
    }
    if let Some(last) = lines.last_mut()
        && UnicodeWidthStr::width(last.as_str()) > width
    {
        *last = truncate(last, width);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::StepKind;

    fn lines(s: &mut Surface) -> Vec<String> {
        s.screen_cells()
            .iter()
            .map(|row| row.iter().map(|c| c.str()).collect::<String>())
            .collect()
    }

    fn rect(cols: usize, rows: usize) -> Rect {
        Rect {
            x: 0,
            y: 0,
            cols,
            rows,
        }
    }

    fn plan_steps(cursor: usize, failed: bool) -> Vec<LoadStep> {
        let labels = ["connect", "image", "container", "shell"];
        crate::loading::plan::LoadPlan::from_cursor(&labels, cursor, failed).into_steps()
    }

    fn draw(steps: &[LoadStep], ctx: &[(String, String)]) -> Vec<String> {
        let mut s = Surface::new(60, 20);
        draw_body(
            &mut s,
            rect(60, 20),
            0,
            steps,
            ctx,
            ColorAttribute::Default,
            ColorAttribute::Default,
        );
        lines(&mut s)
    }

    #[test]
    fn reserve_is_constant_across_cursor_and_failure() {
        // The no-bounce contract: same plan, any state ⇒ same reservation.
        let ctx = vec![("env".to_string(), "local".to_string())];
        let r0 = reserved_rows(&plan_steps(0, false), &ctx);
        let rmid = reserved_rows(&plan_steps(2, false), &ctx);
        let rdone = reserved_rows(&plan_steps(9, false), &ctx);
        let rfail = reserved_rows(&plan_steps(1, true), &ctx);
        assert!(r0 == rmid && rmid == rdone && rdone == rfail, "constant");
        // Floor absorbs the short generic seed.
        assert!(reserved_rows(&plan_steps(0, false), &[]) >= RESERVE_MIN);
    }

    #[test]
    fn gauge_shows_progress_and_elapsed() {
        let steps = plan_steps(2, false);
        let all = draw(&steps, &[]).join("\n");
        assert!(all.contains("2/4"), "done/total on the gauge: {all}");
    }

    #[test]
    fn gauge_flips_to_failed() {
        let steps = plan_steps(1, true);
        let all = draw(&steps, &[]).join("\n");
        assert!(all.contains("failed"), "{all}");
    }

    #[test]
    fn progress_bar_and_bytes_render_under_the_active_step() {
        let mut steps = plan_steps(1, false);
        steps[1].progress = Some((142_000_000, Some(380_000_000)));
        let l = draw(&steps, &[]);
        // Active step on row 2 (gauge row 0, step rows from 1); bar on row 3.
        let g = crate::caps::active_glyphs();
        assert!(l[3].contains(g.bar_fill), "bar fill: {:?}", l[3]);
        assert!(l[3].contains(g.bar_empty), "bar empty: {:?}", l[3]);
        assert!(l[3].contains("142 MB / 380 MB"), "bytes: {:?}", l[3]);
        assert!(l[3].trim_start().starts_with(g.box_v), "rail: {:?}", l[3]);
    }

    #[test]
    fn failure_wraps_error_into_the_reserve() {
        let mut steps = plan_steps(1, true);
        steps[1] = steps[1]
            .clone()
            .with_detail("manifest unknown: tag not found in registry (ghcr.io 404)");
        let l = draw(&steps, &[]);
        // Wrapped into the 2 reserve rows directly under the failed step
        // (rows 3+4); overflow past the reserve is dropped, never a third row.
        assert!(l[3].contains("manifest"), "{:?}", l[3]);
        assert!(!l[3].contains("registry"), "wraps, not one long line");
        assert!(l[4].contains("not found"), "{:?}", l[4]);
        assert!(!l[5].contains("ghcr"), "no third error row: {:?}", l[5]);
    }

    #[test]
    fn rows_below_the_cursor_hold_position_as_it_advances() {
        // The sub-line reserve floats DOWN with the cursor, so a step that is
        // still ahead of the cursor sits at the same row across ticks — the
        // list reads as a stable timeline, not a shuffling one.
        let find_step_row = |steps: &[LoadStep], label: &str| {
            draw(steps, &[])
                .iter()
                .position(|l| l.contains(label))
                .unwrap()
        };
        assert_eq!(
            find_step_row(&plan_steps(0, false), "shell"),
            find_step_row(&plan_steps(2, false), "shell"),
            "a pending step's row is cursor-independent while the cursor is above it"
        );
        // And the failure state keeps the same geometry as the running state.
        assert_eq!(
            find_step_row(&plan_steps(1, false), "shell"),
            find_step_row(&plan_steps(1, true), "shell"),
        );
    }

    #[test]
    fn context_block_renders_below_the_reserve() {
        let ctx = vec![
            ("env".to_string(), "gpu-sprite".to_string()),
            ("placement".to_string(), "provider:sprites".to_string()),
        ];
        let steps = plan_steps(1, false);
        let l = draw(&steps, &ctx);
        // gauge(1) + steps(4) + reserve(2) + gap(1) = context at row 8.
        assert!(l[8].contains("env"), "{:?}", l[8]);
        assert!(l[8].contains("gpu-sprite"));
        assert!(l[9].contains("placement"));
    }

    #[test]
    fn slow_hint_appears_past_threshold() {
        let mut steps = plan_steps(1, false);
        steps[1].kind = StepKind::Image;
        steps[1].started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(20));
        let all = draw(&steps, &[]).join("\n");
        assert!(all.contains("network-bound"), "hint after 20s: {all}");
        // Elapsed column shows the running time.
        assert!(all.contains("20s"), "{all}");
    }

    #[test]
    fn detail_wins_over_hint_and_elapsed_freezes_for_done() {
        let mut steps = plan_steps(1, false);
        steps[1].kind = StepKind::Image;
        steps[1].started_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(20));
        steps[1].detail = Some("layer 3/7".into());
        let all = draw(&steps, &[]).join("\n");
        assert!(all.contains("layer 3/7"));
        assert!(!all.contains("network-bound"), "detail wins");
        // A done step shows its frozen `took`, not a live counter.
        let mut done = plan_steps(9, false);
        done[0].took = Some(std::time::Duration::from_millis(300));
        let all = draw(&done, &[]).join("\n");
        assert!(all.contains("0.3s"), "{all}");
    }

    #[test]
    fn ascii_terminal_renders_the_full_layout() {
        use thegn_core::termcaps::UnicodeLevel;
        crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
            let mut steps = plan_steps(1, false);
            steps[1].progress = Some((10, Some(100)));
            let l = draw(&steps, &[]);
            let all = l.join("\n");
            // The information-loss regression: steps stay visible on ASCII.
            assert!(all.contains("image"), "step labels visible: {all}");
            assert!(l[3].contains('='), "ascii bar fill: {:?}", l[3]);
            assert!(l[3].contains('-'), "ascii bar empty");
            assert!(!all.contains('▓') && !all.contains('│'), "no unicode leaks");
        });
    }

    #[test]
    fn compact_line_shape() {
        let mut steps = plan_steps(1, false);
        steps[1].progress = Some((37, Some(100)));
        let parts = compact_line(&steps, ColorAttribute::Default);
        let text: String = parts.iter().map(|(t, _)| t.as_str()).collect();
        assert!(text.contains("1/4"), "{text}");
        assert!(text.contains("image"));
        assert!(text.contains("37%"));
        // Failed variant says so.
        let parts = compact_line(&plan_steps(1, true), ColorAttribute::Default);
        let text: String = parts.iter().map(|(t, _)| t.as_str()).collect();
        assert!(text.contains("failed"), "{text}");
    }

    #[test]
    fn wrap_and_truncate_are_width_disciplined() {
        assert_eq!(wrap("a b c", 10, 2), vec!["a b c"]);
        let w = wrap("manifest unknown: tag not found in registry", 20, 2);
        assert_eq!(w.len(), 2);
        assert!(w.iter().all(|l| UnicodeWidthStr::width(l.as_str()) <= 20));
        let t = truncate("a-very-long-label-that-overflows", 10);
        assert!(UnicodeWidthStr::width(t.as_str()) <= 10);
        assert!(t.ends_with(crate::caps::active_glyphs().ellipsis));
    }
}
