//! One-row mini-core cell: `label sparkline  pct`, degrading at narrow widths.
use super::{GraphOut, spark};
use crate::chrome::S;
use crate::seg::{self, Line, Seg, Tok};
use termwiz::color::ColorAttribute;
use thegn_core::numfmt::{Unit, fmt};
use thegn_core::termcaps::ColorDepth;
use thegn_core::theme::{Palette, RampId};

/// Width of the right-aligned percent column.
const PCT_W: usize = 4;

/// Longest prefix of `label` that fits in `max` cells.
fn clip_label(label: &str, max: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for (index, c) in label.char_indices() {
        let next = used + seg::cells(c.encode_utf8(&mut [0; 4]));
        if next > max {
            break;
        }
        used = next;
        end = index + c.len_utf8();
    }
    &label[..end]
}

fn tok(depth: ColorDepth, slot: S) -> Tok {
    if depth == ColorDepth::None {
        Tok::Attr(ColorAttribute::Default)
    } else {
        Tok::Slot(slot)
    }
}

fn text_seg(out: &mut GraphOut, fg: Tok, parts: &[&str], pad: usize) -> Seg {
    let mut text = out.take_string(pad + parts.iter().map(|p| p.len()).sum::<usize>());
    text.clear();
    text.extend(std::iter::repeat_n(' ', pad));
    for part in parts {
        text.push_str(part);
    }
    seg::seg(fg, text)
}

/// Paint `label sparkline pct` into one row, never wider than `w` cells.
///
/// The label takes at most `w / 3` cells. When the row is too narrow the
/// sparkline is dropped first, then the label; `w == 0` paints an empty row.
pub(crate) fn paint_minicore(
    out: &mut GraphOut,
    label: &str,
    vals: &[f32],
    pct: f64,
    w: usize,
    ramp: RampId,
    palette: &Palette,
) {
    let pct = if pct.is_nan() {
        0.0
    } else {
        pct.clamp(0.0, 100.0)
    };
    let depth = crate::caps::color_depth();
    let label = clip_label(label, w / 3);
    let label_w = seg::cells(label);
    let lead = if label_w > 0 { label_w + 1 } else { 0 };
    let pct_owned = fmt(pct, Unit::Percent, w.min(PCT_W));
    let pct_text = pct_owned.as_str();
    let label_tok = tok(depth, S::Dim);
    let pct_tok = tok(depth, S::Text);

    if w > lead + PCT_W + 1 {
        // label, space, sparkline (>= 1 cell), space, percent.
        let spark_w = w - lead - PCT_W - 1;
        spark::paint_spark(out, vals, spark_w, ramp, palette);
        let head = (lead > 0).then(|| text_seg(out, label_tok, &[label, " "], 0));
        let tail = text_seg(out, pct_tok, &[" ", pct_text], 0);
        if let Some(Line::Segs(segs)) = out.lines.first_mut() {
            if let Some(head) = head {
                segs.insert(0, head);
            }
            segs.push(tail);
        }
        return;
    }

    // No room for a sparkline: reuse the row, label + gap + percent if it
    // fits, else the percent alone.
    out.cells.reset(0, 1);
    out.lines.resize_with(1, Line::default);
    if !matches!(out.lines[0], Line::Segs(_)) {
        out.lines[0] = Line::Segs(Vec::new());
    }
    let mut fresh = Vec::new();
    if w > 0 {
        if label_w > 0 && w > label_w + PCT_W {
            fresh.push(text_seg(out, label_tok, &[label], 0));
            fresh.push(text_seg(
                out,
                pct_tok,
                &[pct_text],
                w - label_w - seg::cells(pct_text),
            ));
        } else {
            fresh.push(text_seg(
                out,
                pct_tok,
                &[pct_text],
                w.saturating_sub(seg::cells(pct_text)),
            ));
        }
    }
    let spare = &mut out.spare_strings;
    if let Line::Segs(segs) = &mut out.lines[0] {
        spare.extend(segs.drain(..).map(|stale| stale.text));
        segs.extend(fresh);
    }
}
