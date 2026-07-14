//! Help-page rendering: the markdown AST flowed into styled [`Line`]s at a
//! target width, with a table of link positions (for the overlay's link
//! cursor) and heading positions (for search jump-to-section).
//!
//! Owns its own word-flow instead of `seg::wrap` because links must stay
//! addressable after wrapping: every link becomes a single unbreakable atom
//! (spaces in its label swap to NBSP), so one link maps to one seg on one
//! known line.

use unicode_width::UnicodeWidthStr;

use crate::chrome::S;
use crate::seg::{Line, Seg, Tok, Under, seg, sp};
use thegn_core::help::{Block, Inline, LinkTarget, markdown};

/// One rendered link: the physical line its seg starts on, in link order.
#[derive(Debug, Clone)]
pub struct LinkSpan {
    pub line: usize,
    pub target: LinkTarget,
    /// Kept for future status-line display; exercised by tests today.
    #[allow(dead_code)]
    pub label: String,
}

/// A page flowed at one width.
#[derive(Debug, Clone, Default)]
pub struct RenderedPage {
    pub lines: Vec<Line>,
    pub links: Vec<LinkSpan>,
    /// `(line, plain heading text)`, for search's jump-to-section.
    pub headings: Vec<(usize, String)>,
}

/// One unbreakable flow unit.
struct Atom {
    seg: Seg,
    link: Option<usize>,
    space: bool,
}

fn seg_like(base: &Seg, text: &str) -> Seg {
    let mut s = base.clone();
    s.text = text.to_string();
    s
}

/// Split plain text into alternating space / word atoms with `base`'s style.
fn split_runs(text: &str, base: &Seg, out: &mut Vec<Atom>) {
    let mut rest = text;
    while !rest.is_empty() {
        let is_space = rest.starts_with(' ');
        let end = rest
            .find(|c: char| (c == ' ') != is_space)
            .unwrap_or(rest.len());
        let (tok, r) = rest.split_at(end);
        out.push(Atom {
            seg: seg_like(base, tok),
            link: None,
            space: is_space,
        });
        rest = r;
    }
}

/// Convert a span run to atoms. `muted` restyles body text for quotes;
/// `next_link` numbers links page-globally; `selected` inverts that link.
fn atoms(
    spans: &[Inline],
    muted: bool,
    next_link: &mut usize,
    selected: Option<usize>,
    links: &mut Vec<(usize, LinkTarget, String)>,
    out: &mut Vec<Atom>,
) {
    let body_tok = if muted {
        Tok::Slot(S::Dim)
    } else {
        Tok::Slot(S::Text)
    };
    for span in spans {
        match span {
            Inline::Text(t) => {
                let base = if muted {
                    seg(body_tok, "").italic()
                } else {
                    seg(body_tok, "")
                };
                split_runs(t, &base, out);
            }
            Inline::Bold(t) => split_runs(t, &seg(body_tok, "").bold(), out),
            Inline::Italic(t) => split_runs(t, &seg(body_tok, "").italic(), out),
            // Inline code stays one atom so its raised chip never wraps
            // mid-token; spaces become NBSP (width 1, renders as a space).
            Inline::Code(t) => out.push(Atom {
                seg: seg(Tok::Slot(S::Text), t.replace(' ', "\u{a0}")).bg(Tok::Slot(S::Raise)),
                link: None,
                space: false,
            }),
            Inline::Link { target, label } => {
                let idx = *next_link;
                *next_link += 1;
                let styled = if selected == Some(idx) {
                    seg(Tok::Slot(S::Accent), label.replace(' ', "\u{a0}"))
                        .bg(Tok::SelAccent)
                        .bold()
                } else {
                    seg(Tok::Slot(S::Accent), label.replace(' ', "\u{a0}")).under(Under::Single)
                };
                links.push((idx, target.clone(), label.clone()));
                out.push(Atom {
                    seg: styled,
                    link: Some(idx),
                    space: false,
                });
            }
        }
    }
}

struct Flow {
    width: usize,
    lines: Vec<Line>,
    links: Vec<LinkSpan>,
    headings: Vec<(usize, String)>,
}

impl Flow {
    fn blank(&mut self) {
        if !self.lines.is_empty() && !matches!(self.lines.last(), Some(Line::Blank)) {
            self.lines.push(Line::Blank);
        }
    }

    /// Flow `atoms` into lines. `gutter` repeats on every physical line
    /// (quote bar); `lead` appears on the first line only, continuations get
    /// equal-width spaces (list bullet hang).
    fn flow(
        &mut self,
        atoms: Vec<Atom>,
        gutter: &[Seg],
        lead: &[Seg],
        link_meta: &[(usize, LinkTarget, String)],
    ) {
        let gutter_w: usize = gutter.iter().map(|s| s.text.width()).sum();
        let lead_w: usize = lead.iter().map(|s| s.text.width()).sum();
        let avail = self.width.saturating_sub(gutter_w + lead_w).max(1);

        let mut head: Vec<Seg> = gutter.iter().chain(lead.iter()).cloned().collect();
        let mut cur: Vec<Seg> = Vec::new();
        let mut cur_w = 0usize;
        let mut emitted_any = false;

        macro_rules! break_line {
            () => {{
                let mut segs = std::mem::take(&mut head);
                segs.append(&mut cur);
                self.lines.push(Line::segs(segs));
                emitted_any = true;
                head = gutter.to_vec();
                if lead_w > 0 {
                    head.push(sp(lead_w));
                }
                cur_w = 0;
            }};
        }

        for atom in atoms {
            if atom.space {
                if cur_w == 0 {
                    continue; // no leading spaces on any line
                }
                let room = avail - cur_w;
                let take = atom.seg.text.len().min(room);
                if take > 0 {
                    cur.push(seg_like(&atom.seg, &atom.seg.text[..take]));
                    cur_w += take;
                }
                continue;
            }
            let w = atom.seg.text.width();
            let note_link = |flow: &mut Flow, link: Option<usize>| {
                if let Some(idx) = link
                    && flow.links.len() == idx
                {
                    let (_, target, label) = &link_meta[link_meta
                        .iter()
                        .position(|(i, _, _)| *i == idx)
                        .expect("link meta recorded at atom build")];
                    flow.links.push(LinkSpan {
                        line: flow.lines.len(),
                        target: target.clone(),
                        label: label.clone(),
                    });
                }
            };
            if w <= avail - cur_w {
                note_link(self, atom.link);
                cur.push(atom.seg);
                cur_w += w;
                continue;
            }
            if w <= avail && cur_w > 0 {
                break_line!();
                note_link(self, atom.link);
                cur.push(atom.seg);
                cur_w += w;
                continue;
            }
            // Wider than a whole line: hard-split on display cells.
            let mut rest = atom.seg.text.as_str().to_string();
            while !rest.is_empty() {
                let room = avail - cur_w;
                let chunk = crate::seg::take_cols(&rest, room);
                if chunk.is_empty() {
                    break_line!();
                    continue;
                }
                note_link(self, atom.link);
                let chunk_len = chunk.len();
                cur.push(seg_like(&atom.seg, chunk));
                cur_w += cur.last().map(|s| s.text.width()).unwrap_or(0);
                rest = rest[chunk_len..].to_string();
                if !rest.is_empty() {
                    break_line!();
                }
            }
        }
        if !cur.is_empty() || !emitted_any {
            let mut segs = head;
            segs.append(&mut cur);
            self.lines.push(Line::segs(segs));
        }
    }
}

/// Flow a page's blocks at `width`. `selected_link` renders that link (by
/// page-global index) inverted for the overlay's link cursor.
pub fn render_page(blocks: &[Block], width: usize, selected_link: Option<usize>) -> RenderedPage {
    let width = width.max(8);
    let g = crate::caps::active_glyphs();
    let mut f = Flow {
        width,
        lines: Vec::new(),
        links: Vec::new(),
        headings: Vec::new(),
    };
    let mut next_link = 0usize;
    let mut meta: Vec<(usize, LinkTarget, String)> = Vec::new();

    for block in blocks {
        match block {
            Block::Heading { level, spans } => {
                f.blank();
                let text = markdown::plain(spans);
                f.headings.push((f.lines.len(), text.clone()));
                // Headings never wrap; clip to the width (draw_line would
                // truncate anyway — this keeps the invariant testable).
                let text = crate::seg::take_cols(&text, width).to_string();
                let styled = match level {
                    1 => seg(Tok::Slot(S::Text), text).bold(),
                    2 => seg(Tok::Slot(S::Accent), text).bold(),
                    _ => seg(Tok::Slot(S::Ghost2), text).bold(),
                };
                f.lines.push(Line::segs(vec![styled]));
                if *level == 1 {
                    f.lines.push(Line::Fill {
                        ch: g.box_h.chars().next().unwrap_or('-'),
                        fg: Tok::Slot(S::Ghost3),
                    });
                }
            }
            Block::Para(spans) => {
                f.blank();
                let mut a = Vec::new();
                atoms(
                    spans,
                    false,
                    &mut next_link,
                    selected_link,
                    &mut meta,
                    &mut a,
                );
                f.flow(a, &[], &[], &meta);
            }
            Block::Code { text, .. } => {
                f.blank();
                let lines: Vec<&str> = if text.is_empty() {
                    vec![""]
                } else {
                    text.lines().collect()
                };
                for line in lines {
                    let clipped = crate::seg::take_cols(line, width.saturating_sub(2));
                    let pad = width.saturating_sub(clipped.width() + 1);
                    f.lines.push(Line::segs(vec![
                        seg(Tok::Slot(S::Dim), format!(" {clipped}{}", " ".repeat(pad)))
                            .bg(Tok::Slot(S::Raise)),
                    ]));
                }
            }
            Block::List(items) => {
                f.blank();
                let mut counters = [0usize; 2];
                for item in items {
                    let depth = usize::from(item.depth.min(1));
                    if depth == 0 {
                        counters[1] = 0;
                    }
                    let marker = if item.ordered {
                        counters[depth] += 1;
                        format!("{}. ", counters[depth])
                    } else {
                        format!("{} ", g.middot)
                    };
                    let mut lead = Vec::new();
                    if depth > 0 {
                        lead.push(sp(2));
                    }
                    lead.push(seg(Tok::Slot(S::Ghost2), marker));
                    let mut a = Vec::new();
                    atoms(
                        &item.spans,
                        false,
                        &mut next_link,
                        selected_link,
                        &mut meta,
                        &mut a,
                    );
                    f.flow(a, &[], &lead, &meta);
                }
            }
            Block::Quote(spans) => {
                f.blank();
                let gutter = vec![seg(Tok::Slot(S::Ghost2), format!("{} ", g.box_v))];
                let mut a = Vec::new();
                atoms(
                    spans,
                    true,
                    &mut next_link,
                    selected_link,
                    &mut meta,
                    &mut a,
                );
                f.flow(a, &gutter, &[], &meta);
            }
            Block::Rule => {
                f.blank();
                f.lines.push(Line::Fill {
                    ch: g.box_h.chars().next().unwrap_or('-'),
                    fg: Tok::Slot(S::Ghost3),
                });
            }
        }
    }
    while matches!(f.lines.last(), Some(Line::Blank)) {
        f.lines.pop();
    }
    RenderedPage {
        lines: f.lines,
        links: f.links,
        headings: f.headings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Vec<Block> {
        markdown::parse(src)
    }

    fn line_text(l: &Line) -> String {
        match l {
            Line::Blank => String::new(),
            Line::Segs(segs) => segs.iter().map(|s| s.text.as_str()).collect(),
            Line::Split { l, r } => {
                let mut t: String = l.iter().map(|s| s.text.as_str()).collect();
                t.push_str(&r.iter().map(|s| s.text.as_str()).collect::<String>());
                t
            }
            Line::Fill { ch, .. } => ch.to_string(),
        }
    }

    #[test]
    fn paragraph_wraps_at_width() {
        let page = render_page(&parse("alpha beta gamma delta epsilon\n"), 12, None);
        for l in &page.lines {
            assert!(
                line_text(l).width() <= 12,
                "line too wide: {:?}",
                line_text(l)
            );
        }
        assert!(page.lines.len() >= 3, "{:#?}", page.lines);
    }

    #[test]
    fn heading_is_recorded_and_h1_gets_a_rule() {
        let page = render_page(&parse("# Title\n\nbody\n"), 40, None);
        assert_eq!(page.headings, vec![(0, "Title".to_string())]);
        assert!(matches!(page.lines[1], Line::Fill { .. }));
    }

    #[test]
    fn links_map_to_lines_in_order() {
        let src = "intro [[alpha]] middle words that push the second link down \
                   further and further [[beta|the beta page]] end\n";
        let page = render_page(&parse(src), 20, None);
        assert_eq!(page.links.len(), 2);
        assert_eq!(page.links[0].label, "alpha");
        assert!(matches!(&page.links[0].target, LinkTarget::Page(p) if p == "alpha"));
        assert_eq!(page.links[1].label, "the beta page");
        assert!(
            page.links[1].line > page.links[0].line,
            "second link wrapped to a later line: {:?}",
            page.links
        );
        // The multi-word label is one seg on that line (NBSP-joined).
        let l = &page.lines[page.links[1].line];
        assert!(line_text(l).contains("the\u{a0}beta\u{a0}page"));
    }

    #[test]
    fn selected_link_is_inverted() {
        let page = render_page(&parse("see [[alpha]]\n"), 40, Some(0));
        let Line::Segs(segs) = &page.lines[page.links[0].line] else {
            panic!()
        };
        let link_seg = segs.iter().find(|s| s.text == "alpha").unwrap();
        assert!(link_seg.bold);
        assert_eq!(link_seg.bg, Some(Tok::SelAccent));
        // Unselected renders underlined instead.
        let page = render_page(&parse("see [[alpha]]\n"), 40, None);
        let Line::Segs(segs) = &page.lines[page.links[0].line] else {
            panic!()
        };
        let link_seg = segs.iter().find(|s| s.text == "alpha").unwrap();
        assert_eq!(link_seg.under, Under::Single);
    }

    #[test]
    fn code_block_lines_fill_the_width() {
        let page = render_page(&parse("```sh\nls -la\n```\n"), 20, None);
        let code: Vec<&Line> = page
            .lines
            .iter()
            .filter(|l| matches!(l, Line::Segs(s) if s.iter().any(|x| x.bg == Some(Tok::Slot(S::Raise)))))
            .collect();
        assert_eq!(code.len(), 1);
        assert_eq!(line_text(code[0]).width(), 20, "padded to full width");
        assert!(line_text(code[0]).contains("ls -la"));
    }

    #[test]
    fn lists_number_and_hang_indent() {
        let src = "1. first\n1. second item that definitely wraps around\n- plain\n  - nested\n";
        let page = render_page(&parse(src), 18, None);
        let texts: Vec<String> = page.lines.iter().map(line_text).collect();
        assert!(texts.iter().any(|t| t.starts_with("1. first")), "{texts:?}");
        assert!(
            texts.iter().any(|t| t.starts_with("2. second")),
            "renumbered: {texts:?}"
        );
        // Wrapped continuation hangs under the text, not the marker.
        let cont = texts
            .iter()
            .find(|t| t.trim_start().starts_with("wraps") || t.trim_start().starts_with("around"))
            .unwrap();
        assert!(cont.starts_with("   "), "hanging indent: {cont:?}");
        assert!(
            texts.iter().any(|t| t.starts_with("  ")),
            "nested item indented"
        );
    }

    #[test]
    fn quote_gutter_repeats_on_wrapped_lines() {
        let src = "> a quoted tip that is long enough to wrap at this width\n";
        let page = render_page(&parse(src), 16, None);
        let quoted: Vec<String> = page
            .lines
            .iter()
            .map(line_text)
            .filter(|t| !t.is_empty())
            .collect();
        assert!(quoted.len() >= 2);
        for l in &quoted {
            assert!(l.starts_with('│') || l.starts_with('|'), "{l:?}");
        }
    }

    #[test]
    fn oversized_token_hard_splits() {
        let src = "path/that/is/way/too/long/to/fit/on/one/line/of/this/width\n";
        let page = render_page(&parse(src), 10, None);
        assert!(page.lines.len() >= 2);
        for l in &page.lines {
            assert!(line_text(l).width() <= 10);
        }
    }

    #[test]
    fn tiny_width_never_panics() {
        let src = "# H\n\ntext [[index]] `code` **bold**\n\n```\nx\n```\n";
        for w in 0..=9 {
            let _ = render_page(&parse(src), w, Some(0));
        }
    }

    #[test]
    fn full_shipped_pages_render_at_common_widths() {
        for src in crate::help::pages::SOURCES {
            let (_, body) = thegn_core::help::frontmatter::parse(src).unwrap();
            let blocks = markdown::parse(body);
            for w in [20, 40, 80, 120] {
                let page = render_page(&blocks, w, None);
                for l in &page.lines {
                    assert!(line_text(l).width() <= w, "overflow at width {w}");
                }
            }
        }
    }
}
