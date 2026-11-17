//! Pure text extraction from a `.docx` `word/document.xml` (preview pane).
//!
//! A hand-rolled tag scanner over untrusted bytes: no XML crate, nothing is
//! executed, every slice is bounds-checked and output is capped by the caller.

/// Extracted paragraphs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DocxText {
    pub lines: Vec<String>,
    /// True when `max_lines` cut the document short.
    pub truncated: bool,
}

const MAX_ENTITY_LEN: usize = 12;

struct Out {
    lines: Vec<String>,
    cur: String,
    cur_chars: usize,
    max_lines: usize,
    max_line_chars: usize,
    truncated: bool,
}

impl Out {
    fn push_str(&mut self, s: &str) {
        for ch in s.chars() {
            if self.cur_chars >= self.max_line_chars {
                return;
            }
            self.cur.push(ch);
            self.cur_chars += 1;
        }
    }

    /// End the current line; false once the line budget is exhausted.
    fn end_line(&mut self) -> bool {
        if self.lines.len() >= self.max_lines {
            self.truncated = true;
            return false;
        }
        self.lines.push(std::mem::take(&mut self.cur));
        self.cur_chars = 0;
        true
    }
}

/// Decode one entity body (the text between `&` and `;`).
fn decode_entity(body: &str) -> Option<char> {
    match body {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let num = body.strip_prefix('#')?;
            let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                num.parse::<u32>().ok()
            };
            // Invalid numeric entities (surrogates, out of range, zero) are U+FFFD.
            Some(
                code.and_then(char::from_u32)
                    .filter(|c| *c != '\0')
                    .unwrap_or('\u{FFFD}'),
            )
        }
    }
}

fn push_text(out: &mut Out, text: &str) {
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        let (before, after) = rest.split_at(amp);
        out.push_str(before);
        let tail = after.get(1..).unwrap_or("");
        let window = tail
            .get(..tail.len().min(MAX_ENTITY_LEN))
            .unwrap_or_else(|| {
                // Window cut a multi-byte char: fall back to the ASCII prefix.
                let end = tail
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|i| *i <= MAX_ENTITY_LEN)
                    .last()
                    .unwrap_or(0);
                tail.get(..end).unwrap_or("")
            });
        match window
            .find(';')
            .and_then(|semi| decode_entity(window.get(..semi)?).map(|c| (c, semi)))
        {
            Some((c, semi)) => {
                let mut buf = [0u8; 4];
                out.push_str(c.encode_utf8(&mut buf));
                rest = tail.get(semi + 1..).unwrap_or("");
            }
            None => {
                out.push_str("&");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
}

/// Extract paragraph text from `word/document.xml` bytes.
///
/// `<w:t>` text is accumulated; `</w:p>`, `<w:br/>` and `<w:cr/>` end a line;
/// `<w:tab/>` adds four spaces. Non-UTF-8 input is decoded lossily.
pub fn document_xml_lines(
    xml: &[u8],
    max_lines: usize,
    max_line_chars: usize,
) -> Result<DocxText, String> {
    if max_lines == 0 {
        return Err("no line budget".into());
    }
    let text = String::from_utf8_lossy(xml);
    if !text.contains('<') {
        return Err("document.xml is not xml".into());
    }
    let mut out = Out {
        lines: Vec::new(),
        cur: String::new(),
        cur_chars: 0,
        max_lines,
        max_line_chars,
        truncated: false,
    };
    let mut in_t = false;
    let mut in_tabs = false;
    let mut rest: &str = &text;
    'scan: while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            if in_t {
                push_text(&mut out, rest);
            }
            break;
        };
        let (before, from_tag) = rest.split_at(lt);
        if in_t {
            push_text(&mut out, before);
        }
        let Some(gt) = from_tag.find('>') else {
            break; // unterminated tag: ignore the remainder
        };
        let tag = from_tag.get(1..gt).unwrap_or("");
        rest = from_tag.get(gt + 1..).unwrap_or("");
        let self_closing = tag.ends_with('/');
        let name = tag
            .split(|c: char| c.is_ascii_whitespace() || c == '/')
            .find(|s| !s.is_empty())
            .unwrap_or("");
        // `</w:t>` splits to "" then "w:t" on '/', so test the raw prefix.
        match (tag.starts_with('/'), name) {
            (false, "w:t") => in_t = !self_closing,
            (true, "w:t") => in_t = false,
            (true, "w:p") => {
                in_t = false;
                if !out.end_line() {
                    break 'scan;
                }
            }
            (false, "w:tabs") => in_tabs = !self_closing,
            (true, "w:tabs") => in_tabs = false,
            (false, "w:tab") if !in_tabs => out.push_str("    "),
            (false, "w:br" | "w:cr") => {
                if !out.end_line() {
                    break 'scan;
                }
            }
            _ => {}
        }
    }
    if !out.cur.is_empty() && !out.truncated {
        out.end_line();
    }
    Ok(DocxText {
        lines: out.lines,
        truncated: out.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(xml: &str) -> Vec<String> {
        document_xml_lines(xml.as_bytes(), 100, 1000).unwrap().lines
    }

    #[test]
    fn paragraphs_tabs_breaks() {
        let x = "<w:document><w:body><w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:tab/><w:t>World</w:t></w:r></w:p>\
                 <w:p><w:r><w:t>a</w:t><w:br/><w:t>b</w:t><w:cr/><w:t>c</w:t></w:r></w:p><w:p></w:p></w:body></w:document>";
        assert_eq!(lines(x), ["Hello    World", "a", "b", "c", ""]);
        assert_eq!(
            lines(
                "<w:p><w:pPr><w:tabs><w:tab w:val=\"left\" w:pos=\"720\"/></w:tabs></w:pPr><w:r><w:t>x</w:t></w:r></w:p>"
            ),
            ["x"]
        );
    }

    #[test]
    fn entities_numeric_and_invalid() {
        let x = "<w:p><w:t>a &amp; b &lt;&gt; &quot;&apos; &#65;&#x42; &#xD800; &#99999999; &#; &bogus &amp</w:t></w:p>";
        assert_eq!(
            lines(x),
            ["a & b <> \"' AB \u{FFFD} \u{FFFD} \u{FFFD} &bogus &amp"]
        );
    }

    #[test]
    fn attributes_nested_runs_and_non_text_ignored() {
        let x = "<w:p><w:pPr><w:t2>no</w:t2></w:pPr><w:r><w:t xml:space=\"preserve\"> keep </w:t></w:r>\
                 <w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:t/><w:instrText>skip</w:instrText>between</w:p>";
        assert_eq!(lines(x), [" keep bold"]);
    }

    #[test]
    fn non_utf8_is_lossy() {
        let mut x = b"<w:p><w:t>a".to_vec();
        x.push(0xFF);
        x.extend_from_slice(b"b</w:t></w:p>");
        let t = document_xml_lines(&x, 10, 100).unwrap();
        assert_eq!(t.lines, ["a\u{FFFD}b"]);
    }

    #[test]
    fn max_lines_truncates() {
        let x = "<w:p><w:t>1</w:t></w:p><w:p><w:t>2</w:t></w:p><w:p><w:t>3</w:t></w:p>";
        let t = document_xml_lines(x.as_bytes(), 2, 100).unwrap();
        assert_eq!(t.lines, ["1", "2"]);
        assert!(t.truncated);
        let t = document_xml_lines(x.as_bytes(), 3, 100).unwrap();
        assert!(!t.truncated);
        assert_eq!(t.lines.len(), 3);
    }

    #[test]
    fn line_chars_capped_and_trailing_text_flushed() {
        let t = document_xml_lines(b"<w:p><w:t>abcdefgh", 5, 3).unwrap();
        assert_eq!(t.lines, ["abc"]);
        assert!(document_xml_lines(b"<w:p><w:t>x", 0, 3).is_err());
        assert!(document_xml_lines(b"plain text", 3, 3).is_err());
    }

    #[test]
    fn unterminated_tag_and_multibyte_entity_window() {
        assert_eq!(lines("<w:p><w:t>a</w:t></w:p><w:p"), ["a"]);
        let t = "<w:p><w:t>&\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}</w:t></w:p>";
        assert_eq!(lines(t).len(), 1);
    }

    #[test]
    fn every_prefix_of_a_document_is_safe() {
        let x = "<?xml version=\"1.0\"?><w:document><w:body><w:p><w:r><w:t xml:space=\"preserve\">caf\u{e9} &amp; &#x1F600; \u{1F600}</w:t></w:r><w:tab/></w:p></w:body></w:document>";
        for n in 0..=x.len() {
            if let Ok(t) = document_xml_lines(&x.as_bytes()[..n], 10, 20) {
                assert!(t.lines.len() <= 10);
            }
        }
    }
}
