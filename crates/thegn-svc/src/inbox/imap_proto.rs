//! Pure IMAP4rev1 wire handling for the read-only inbox client (b163,
//! THE-2216). No I/O: [`super::imap`] owns the socket and feeds bytes here.
//!
//! Everything the server sends is hostile until parsed. Every bound is a
//! const below: a response line segment, a `{n}` literal, the whole reply to
//! one command, the list nesting depth, and the UIDs one FETCH may name.
//!
//! The command builders are the only producers of client bytes. None of them
//! can say `SELECT`, `STORE`, `EXPUNGE` or a non-`.PEEK` `BODY[`: the client
//! only reads.

use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use std::fmt::Write as _;
use zeroize::Zeroizing;

/// The longest response line segment (text between CRLFs, literals excluded).
/// A `* SEARCH` line naming a large unread set must fit; [`MAX_RESPONSE`]
/// still bounds memory per command.
pub const MAX_LINE: usize = 1024 * 1024;
/// The largest `{n}` literal accepted.
pub const MAX_LITERAL: usize = 64 * 1024;
/// The most bytes the whole reply to one command may take (untagged
/// responses plus the tagged completion).
pub const MAX_RESPONSE: usize = 2 * 1024 * 1024;
/// The most UIDs one `UID FETCH` names.
pub const MAX_UIDS: usize = 200;
/// The most untagged responses one command's reply may carry.
pub const MAX_UNTAGGED: usize = 4 * MAX_UIDS + 64;
/// The longest encoded-word scan (charset, and encoded text) before giving up.
const MAX_ENCODED_WORD: usize = 256;
/// The deepest parenthesized list nesting accepted.
pub const MAX_DEPTH: usize = 8;
/// The most digits in a `{n}` literal length.
const MAX_LITERAL_DIGITS: usize = 10;
/// The longest tag accepted from the server.
const MAX_TAG: usize = 32;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Why bytes could not be parsed or a command could not be built. Never
/// carries server or credential bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// A size bound was exceeded (`line`, `literal`, `response`, `uids`).
    BodyLimit(&'static str),
    /// The server sent something that is not IMAP.
    Malformed(&'static str),
    /// A string handed to a command builder contains CR, LF or NUL.
    Unquotable,
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::BodyLimit(what) => write!(f, "server response exceeds the {what} limit"),
            ProtoError::Malformed(what) => write!(f, "malformed server response: {what}"),
            ProtoError::Unquotable => write!(f, "value contains CR, LF or NUL"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// Who a response line is addressed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    /// `* ...`
    Untagged,
    /// `+ ...` (a continuation request)
    Continuation,
    /// `<tag> ...`
    Tagged(String),
}

/// One complete server response: the tag and everything after it, up to the
/// final CRLF. Literals stay inline in their wire form (`{n}\r\n<n bytes>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub tag: Tag,
    pub body: Vec<u8>,
}

fn find_crlf(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w == b"\r\n")
}

/// If `segment` ends with a literal announcement `{n}` (or `{n+}`), its
/// length. `Ok(None)` when it does not end with one.
fn trailing_literal(segment: &[u8]) -> Result<Option<usize>, ProtoError> {
    let Some(inner) = segment.strip_suffix(b"}") else {
        return Ok(None);
    };
    let Some(open) = inner.iter().rposition(|&b| b == b'{') else {
        return Ok(None);
    };
    let digits = &inner[open + 1..];
    let digits = digits.strip_suffix(b"+").unwrap_or(digits);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Ok(None);
    }
    literal_len(digits).map(Some)
}

fn literal_len(digits: &[u8]) -> Result<usize, ProtoError> {
    if digits.len() > MAX_LITERAL_DIGITS {
        return Err(ProtoError::BodyLimit("literal"));
    }
    let mut n: u64 = 0;
    for &d in digits {
        n = n * 10 + u64::from(d - b'0');
    }
    if n > MAX_LITERAL as u64 {
        return Err(ProtoError::BodyLimit("literal"));
    }
    Ok(n as usize)
}

/// Read one complete response from the front of `buf`. `Ok(None)` when more
/// bytes are needed; on success, the response and the bytes it used.
///
/// Errors as soon as a bound is crossed, before the oversize part is
/// buffered: a segment longer than [`MAX_LINE`] with no CRLF yet, a literal
/// announced larger than [`MAX_LITERAL`], or a response past [`MAX_RESPONSE`].
pub fn parse_response(buf: &[u8]) -> Result<Option<(Response, usize)>, ProtoError> {
    let mut pos = 0usize;
    loop {
        let Some(rel) = find_crlf(&buf[pos..]) else {
            if buf.len() - pos > MAX_LINE {
                return Err(ProtoError::BodyLimit("line"));
            }
            return Ok(None);
        };
        if rel > MAX_LINE {
            return Err(ProtoError::BodyLimit("line"));
        }
        let seg_end = pos + rel;
        if seg_end + 2 > MAX_RESPONSE {
            return Err(ProtoError::BodyLimit("response"));
        }
        match trailing_literal(&buf[pos..seg_end])? {
            Some(n) => {
                let next = seg_end + 2 + n;
                if next > MAX_RESPONSE {
                    return Err(ProtoError::BodyLimit("response"));
                }
                if buf.len() < next {
                    return Ok(None);
                }
                pos = next;
            }
            None => {
                let response = split_tag(&buf[..seg_end])?;
                return Ok(Some((response, seg_end + 2)));
            }
        }
    }
}

fn split_tag(line: &[u8]) -> Result<Response, ProtoError> {
    if let Some(rest) = line.strip_prefix(b"*") {
        let body = rest
            .strip_prefix(b" ")
            .ok_or(ProtoError::Malformed("untagged response without a space"))?;
        return Ok(Response {
            tag: Tag::Untagged,
            body: body.to_vec(),
        });
    }
    if let Some(rest) = line.strip_prefix(b"+") {
        return Ok(Response {
            tag: Tag::Continuation,
            body: rest.strip_prefix(b" ").unwrap_or(rest).to_vec(),
        });
    }
    let space = line
        .iter()
        .position(|&b| b == b' ')
        .ok_or(ProtoError::Malformed("tagged response without a space"))?;
    let tag = &line[..space];
    if tag.is_empty() || tag.len() > MAX_TAG || !tag.iter().all(|&b| is_atom_char(b)) {
        return Err(ProtoError::Malformed("bad tag"));
    }
    Ok(Response {
        tag: Tag::Tagged(String::from_utf8_lossy(tag).into_owned()),
        body: line[space + 1..].to_vec(),
    })
}

fn is_atom_char(b: u8) -> bool {
    b > 0x20 && b < 0x7f && !matches!(b, b'(' | b')' | b'{' | b'"' | b'\\' | b'%' | b'*' | b']')
}

/// A parsed IMAP value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// An atom or number; a `BODY[...]` section stays inside the atom.
    Atom(String),
    /// A quoted string or a literal.
    Str(Vec<u8>),
    Nil,
    List(Vec<Value>),
}

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn skip_spaces(&mut self) {
        while self.peek() == Some(b' ') {
            self.i += 1;
        }
    }

    fn seq(&mut self, depth: usize, close: Option<u8>) -> Result<Vec<Value>, ProtoError> {
        let mut out = Vec::new();
        loop {
            self.skip_spaces();
            match self.peek() {
                None if close.is_none() => return Ok(out),
                None => return Err(ProtoError::Malformed("unterminated list")),
                Some(b) if Some(b) == close => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b')') => return Err(ProtoError::Malformed("unbalanced ')'")),
                Some(_) => out.push(self.value(depth)?),
            }
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ProtoError> {
        match self.peek() {
            Some(b'(') => {
                if depth >= MAX_DEPTH {
                    return Err(ProtoError::Malformed("lists nested too deep"));
                }
                self.i += 1;
                Ok(Value::List(self.seq(depth + 1, Some(b')'))?))
            }
            Some(b'"') => self.quoted(),
            Some(b'{') => self.literal(),
            _ => self.atom(),
        }
    }

    fn quoted(&mut self) -> Result<Value, ProtoError> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None | Some(b'\r') | Some(b'\n') => {
                    return Err(ProtoError::Malformed("unterminated quoted string"));
                }
                Some(b'"') => {
                    self.i += 1;
                    return Ok(Value::Str(out));
                }
                Some(b'\\') => {
                    let escaped = self
                        .b
                        .get(self.i + 1)
                        .copied()
                        .ok_or(ProtoError::Malformed("dangling escape"))?;
                    out.push(escaped);
                    self.i += 2;
                }
                Some(b) => {
                    out.push(b);
                    self.i += 1;
                }
            }
        }
    }

    fn literal(&mut self) -> Result<Value, ProtoError> {
        let rest = &self.b[self.i..];
        let close = rest
            .iter()
            .position(|&b| b == b'}')
            .ok_or(ProtoError::Malformed("unterminated literal length"))?;
        let digits = &rest[1..close];
        let digits = digits.strip_suffix(b"+").unwrap_or(digits);
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Err(ProtoError::Malformed("bad literal length"));
        }
        let n = literal_len(digits)?;
        if rest.get(close + 1..close + 3) != Some(b"\r\n".as_slice()) {
            return Err(ProtoError::Malformed("literal without CRLF"));
        }
        let start = close + 3;
        let bytes = rest
            .get(start..start + n)
            .ok_or(ProtoError::Malformed("truncated literal"))?;
        self.i += start + n;
        Ok(Value::Str(bytes.to_vec()))
    }

    fn atom(&mut self) -> Result<Value, ProtoError> {
        let start = self.i;
        while let Some(b) = self.peek() {
            match b {
                b'[' => {
                    // A section (`BODY[HEADER.FIELDS (FROM)]`) holds spaces
                    // and parens; it ends at the next ']'.
                    let close = self.b[self.i..]
                        .iter()
                        .position(|&c| c == b']')
                        .ok_or(ProtoError::Malformed("unterminated section"))?;
                    if self.b[self.i..self.i + close]
                        .iter()
                        .any(|&c| c == b'\r' || c == b'\n')
                    {
                        return Err(ProtoError::Malformed("section spans lines"));
                    }
                    self.i += close + 1;
                }
                b' ' | b'(' | b')' | b'"' | b'\r' | b'\n' => break,
                b if b < 0x20 || b == 0x7f => break,
                _ => self.i += 1,
            }
        }
        if self.i == start {
            return Err(ProtoError::Malformed("unexpected character"));
        }
        let text = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
        if text.eq_ignore_ascii_case("NIL") {
            Ok(Value::Nil)
        } else {
            Ok(Value::Atom(text))
        }
    }
}

/// Tokenize a response body into values.
pub fn parse_values(body: &[u8]) -> Result<Vec<Value>, ProtoError> {
    Lexer { b: body, i: 0 }.seq(0, None)
}

/// A status response's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Ok,
    No,
    Bad,
    Bye,
    Preauth,
}

/// A status response (`OK`/`NO`/`BAD`/`BYE`/`PREAUTH`) and its response code
/// (the text inside `[...]`), if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub kind: StatusKind,
    pub code: Option<String>,
}

impl Status {
    /// The response code's first word, uppercased and reduced to
    /// `[A-Z0-9-]` (at most 32 chars): safe to put in an error message.
    pub fn code_word(&self) -> Option<String> {
        let code = self.code.as_deref()?;
        let word: String = code
            .split(' ')
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(32)
            .collect::<String>()
            .to_ascii_uppercase();
        (!word.is_empty()).then_some(word)
    }
}

fn first_word(body: &[u8]) -> (&[u8], &[u8]) {
    match body.iter().position(|&b| b == b' ') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None => (body, &[]),
    }
}

/// The status of a tagged or untagged status response.
pub fn status(response: &Response) -> Option<Status> {
    if response.tag == Tag::Continuation {
        return None;
    }
    let (word, rest) = first_word(&response.body);
    let kind = match word.to_ascii_uppercase().as_slice() {
        b"OK" => StatusKind::Ok,
        b"NO" => StatusKind::No,
        b"BAD" => StatusKind::Bad,
        b"BYE" => StatusKind::Bye,
        b"PREAUTH" => StatusKind::Preauth,
        _ => return None,
    };
    let code = rest.strip_prefix(b"[").and_then(|inner| {
        let close = inner.iter().position(|&b| b == b']')?;
        Some(String::from_utf8_lossy(&inner[..close]).into_owned())
    });
    Some(Status { kind, code })
}

/// `* OK [UIDVALIDITY n]` → `n` (non-zero).
pub fn uidvalidity(response: &Response) -> Option<u32> {
    if response.tag != Tag::Untagged {
        return None;
    }
    let st = status(response)?;
    if st.kind != StatusKind::Ok {
        return None;
    }
    let code = st.code?;
    let (word, value) = code.split_once(' ')?;
    if !word.eq_ignore_ascii_case("UIDVALIDITY") {
        return None;
    }
    value.trim().parse::<u32>().ok().filter(|v| *v != 0)
}

/// `* CAPABILITY ...` or `* OK [CAPABILITY ...]` → the capabilities,
/// uppercased.
pub fn capabilities(response: &Response) -> Option<Vec<String>> {
    if response.tag != Tag::Untagged {
        return None;
    }
    let (word, rest) = first_word(&response.body);
    let list = if word.eq_ignore_ascii_case(b"CAPABILITY") {
        String::from_utf8_lossy(rest).into_owned()
    } else {
        let code = status(response)?.code?;
        let (word, list) = code.split_once(' ')?;
        if !word.eq_ignore_ascii_case("CAPABILITY") {
            return None;
        }
        list.to_owned()
    };
    Some(
        list.split_ascii_whitespace()
            .map(str::to_ascii_uppercase)
            .collect(),
    )
}

/// `* SEARCH 1 2 3` → the numbers (each non-zero).
pub fn search(response: &Response) -> Option<Result<Vec<u32>, ProtoError>> {
    if response.tag != Tag::Untagged {
        return None;
    }
    let (word, rest) = first_word(&response.body);
    if !word.eq_ignore_ascii_case(b"SEARCH") {
        return None;
    }
    let parsed: Result<Vec<u32>, ProtoError> = rest
        .split(|&b| b == b' ')
        .filter(|w| !w.is_empty())
        .map(|w| parse_nz_number(w).ok_or(ProtoError::Malformed("bad SEARCH number")))
        .collect();
    Some(parsed)
}

fn parse_nz_number(w: &[u8]) -> Option<u32> {
    if w.is_empty() || w.len() > 10 || !w.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(w)
        .ok()?
        .parse::<u32>()
        .ok()
        .filter(|n| *n != 0)
}

/// One `* n FETCH (...)` response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetch {
    pub seq: u32,
    pub uid: Option<u32>,
    pub flags: Vec<String>,
    /// INTERNALDATE as Unix seconds.
    pub internal_date: Option<i64>,
    /// The `BODY[HEADER.FIELDS (...)]` bytes.
    pub header: Option<Vec<u8>>,
}

/// Parse a `* n FETCH (...)` response; `None` for any other response.
pub fn fetch(response: &Response) -> Option<Result<Fetch, ProtoError>> {
    if response.tag != Tag::Untagged {
        return None;
    }
    let (seq, rest) = first_word(&response.body);
    let (word, _) = first_word(rest);
    if !word.eq_ignore_ascii_case(b"FETCH") {
        return None;
    }
    let Some(seq) = parse_nz_number(seq) else {
        return Some(Err(ProtoError::Malformed("bad FETCH sequence number")));
    };
    Some(parse_fetch(seq, &response.body))
}

fn parse_fetch(seq: u32, body: &[u8]) -> Result<Fetch, ProtoError> {
    let values = parse_values(body)?;
    let items = match values.as_slice() {
        [Value::Atom(_), Value::Atom(_), Value::List(items)] => items,
        _ => return Err(ProtoError::Malformed("FETCH without one attribute list")),
    };
    if items.len() % 2 != 0 {
        return Err(ProtoError::Malformed("FETCH attribute without a value"));
    }
    let mut out = Fetch {
        seq,
        ..Fetch::default()
    };
    for pair in items.chunks(2) {
        let Value::Atom(key) = &pair[0] else {
            return Err(ProtoError::Malformed("FETCH attribute is not an atom"));
        };
        let key = key.to_ascii_uppercase();
        match (key.as_str(), &pair[1]) {
            ("UID", Value::Atom(n)) => {
                out.uid =
                    Some(parse_nz_number(n.as_bytes()).ok_or(ProtoError::Malformed("bad UID"))?);
            }
            ("UID", _) => return Err(ProtoError::Malformed("bad UID")),
            ("FLAGS", Value::List(flags)) => {
                out.flags = flags
                    .iter()
                    .filter_map(|f| match f {
                        Value::Atom(a) => Some(a.clone()),
                        _ => None,
                    })
                    .collect();
            }
            ("INTERNALDATE", Value::Str(s)) => {
                out.internal_date = parse_internal_date(&String::from_utf8_lossy(s));
            }
            (k, Value::Str(bytes)) if k.starts_with("BODY[") => out.header = Some(bytes.clone()),
            (k, Value::Nil) if k.starts_with("BODY[") => out.header = Some(Vec::new()),
            _ => {}
        }
    }
    Ok(out)
}

/// `"17-Jul-1996 02:44:25 -0700"` (day may be space-padded) → Unix seconds.
pub fn parse_internal_date(s: &str) -> Option<i64> {
    let mut parts = s.split_ascii_whitespace();
    let (date, time, zone) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let mut d = date.split('-');
    let day: u32 = d.next()?.parse().ok()?;
    let month = d.next()?;
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(month))? as u32 + 1;
    let year: i32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let (h, m, sec): (u32, u32, u32) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    let zb = zone.as_bytes();
    if zb.len() != 5 || !matches!(zb[0], b'+' | b'-') || !zb[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let zh: i64 = zone[1..3].parse().ok()?;
    let zm: i64 = zone[3..5].parse().ok()?;
    let offset = (zh * 3600 + zm * 60) * if zb[0] == b'-' { -1 } else { 1 };
    let naive = chrono::NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(h, m, sec)?;
    Some(naive.and_utc().timestamp() - offset)
}

/// The header fields the client asks for, unfolded and still encoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    pub from: Option<String>,
    pub subject: Option<String>,
    /// Without the angle brackets.
    pub message_id: Option<String>,
}

/// Parse a header block: unfold continuation lines and keep the first
/// From, Subject and Message-ID. Stops at the first empty line.
pub fn parse_headers(raw: &[u8]) -> Headers {
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in raw.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let text = String::from_utf8_lossy(line);
        if line[0] == b' ' || line[0] == b'\t' {
            if let Some((_, value)) = fields.last_mut() {
                value.push_str(&text);
            }
            continue;
        }
        if let Some((name, value)) = text.split_once(':') {
            fields.push((name.trim().to_ascii_lowercase(), value.to_owned()));
        }
    }
    let mut out = Headers::default();
    for (name, value) in fields {
        let slot = match name.as_str() {
            "from" => &mut out.from,
            "subject" => &mut out.subject,
            "message-id" => &mut out.message_id,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(value.trim().to_owned());
        }
    }
    out.message_id = out.message_id.map(|id| {
        let id = id.trim();
        match (id.find('<'), id.find('>')) {
            (Some(open), Some(close)) if open < close => id[open + 1..close].trim().to_owned(),
            _ => id.to_owned(),
        }
    });
    out
}

/// Decode RFC 2047 encoded words (`=?charset?B|Q?text?=`). UTF-8, US-ASCII
/// and ISO-8859-1 are decoded; any other charset, or a word that does not
/// decode, is left raw. Whitespace between two adjacent encoded words is
/// dropped, as the RFC requires.
pub fn decode_words(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    let mut after_word = false;
    while !rest.is_empty() {
        if after_word {
            let trimmed = rest.trim_start_matches([' ', '\t']);
            if trimmed.len() < rest.len()
                && let Some((decoded, used)) = encoded_word(trimmed)
            {
                out.push_str(&decoded);
                rest = &trimmed[used..];
                continue;
            }
        }
        if let Some((decoded, used)) = encoded_word(rest) {
            out.push_str(&decoded);
            rest = &rest[used..];
            after_word = true;
            continue;
        }
        let Some(ch) = rest.chars().next() else {
            break;
        };
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
        after_word = false;
    }
    out
}

/// One encoded word at the start of `s`: the decoded text and the bytes used.
fn encoded_word(s: &str) -> Option<(String, usize)> {
    let body = s.strip_prefix("=?")?;
    let q1 = body
        .as_bytes()
        .iter()
        .take(MAX_ENCODED_WORD)
        .position(|&b| b == b'?')?;
    let charset = &body[..q1];
    let after = &body[q1 + 1..];
    let enc = *after.as_bytes().first()?;
    if !matches!(enc, b'B' | b'b' | b'Q' | b'q') || after.as_bytes().get(1) != Some(&b'?') {
        return None;
    }
    let text_all = &after[2..];
    let end = text_all
        .as_bytes()
        .windows(2)
        .take(MAX_ENCODED_WORD)
        .position(|w| w == b"?=")?;
    let text = &text_all[..end];
    if charset.is_empty()
        || charset.contains(char::is_whitespace)
        || text.contains(char::is_whitespace)
    {
        return None;
    }
    let used = 2 + q1 + 1 + 2 + end + 2;
    let bytes = if matches!(enc, b'B' | b'b') {
        b_decode(text)?
    } else {
        q_decode(text)?
    };
    let charset = charset
        .split('*')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let decoded = match charset.as_str() {
        "utf-8" | "utf8" => String::from_utf8(bytes).ok()?,
        "us-ascii" => {
            if !bytes.is_ascii() {
                return None;
            }
            String::from_utf8(bytes).ok()?
        }
        "iso-8859-1" | "iso_8859-1" | "latin1" => bytes.iter().map(|&b| char::from(b)).collect(),
        _ => return None,
    };
    Some((decoded, used))
}

fn b_decode(text: &str) -> Option<Vec<u8>> {
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    LENIENT.decode(text).ok()
}

fn q_decode(text: &str) -> Option<Vec<u8>> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' => {
                let hex = b.get(i + 1..i + 3)?;
                let hex = std::str::from_utf8(hex).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// The sender's display name (decoded, unquoted), or else the address.
pub fn sender_display(raw_from: &str) -> String {
    let v = raw_from.trim();
    if let Some(lt) = v.rfind('<')
        && let Some(gt) = v[lt..].find('>')
    {
        let addr = v[lt + 1..lt + gt].trim();
        let name = unquote(decode_words(v[..lt].trim()).trim());
        return if name.is_empty() {
            addr.to_owned()
        } else {
            name
        };
    }
    let addr = match v.find('(') {
        Some(p) => v[..p].trim(),
        None => v,
    };
    decode_words(addr)
}

fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_owned();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_owned()
}

/// Percent-encode everything outside RFC 3986 unreserved characters and `@`.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'@') {
            out.push(char::from(b));
        } else {
            write!(out, "%{b:02X}").expect("writing to String cannot fail");
        }
    }
    out
}

/// An IMAP quoted string. Refuses CR, LF and NUL (they cannot be quoted and
/// would end the command line).
pub fn quote(s: &str) -> Result<String, ProtoError> {
    if s.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(ProtoError::Unquotable);
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

/// A command line (`<tag> <rest>\r\n`) in a buffer zeroed on drop.
fn line(tag: &str, rest: &str) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(tag.len() + rest.len() + 3));
    out.extend_from_slice(tag.as_bytes());
    out.push(b' ');
    out.extend_from_slice(rest.as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

pub fn cmd_capability(tag: &str) -> Zeroizing<Vec<u8>> {
    line(tag, "CAPABILITY")
}

/// `LOGIN "user" "pass"`.
pub fn cmd_login(tag: &str, user: &str, pass: &str) -> Result<Zeroizing<Vec<u8>>, ProtoError> {
    let user = Zeroizing::new(quote(user)?);
    let pass = Zeroizing::new(quote(pass)?);
    let rest = Zeroizing::new(format!("LOGIN {} {}", user.as_str(), pass.as_str()));
    Ok(line(tag, &rest))
}

/// `AUTHENTICATE PLAIN <base64(\0user\0pass)>` (SASL-IR, RFC 4959).
pub fn cmd_authenticate_plain(
    tag: &str,
    user: &str,
    pass: &str,
) -> Result<Zeroizing<Vec<u8>>, ProtoError> {
    if [user, pass]
        .iter()
        .any(|s| s.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)))
    {
        return Err(ProtoError::Unquotable);
    }
    let mut raw = Zeroizing::new(Vec::with_capacity(user.len() + pass.len() + 2));
    raw.push(0);
    raw.extend_from_slice(user.as_bytes());
    raw.push(0);
    raw.extend_from_slice(pass.as_bytes());
    let encoded = Zeroizing::new(STANDARD.encode(raw.as_slice()));
    let rest = Zeroizing::new(format!("AUTHENTICATE PLAIN {}", encoded.as_str()));
    Ok(line(tag, &rest))
}

/// `EXAMINE "<mailbox>"`: read-only open. There is no `SELECT` builder.
pub fn cmd_examine(tag: &str, mailbox: &str) -> Result<Zeroizing<Vec<u8>>, ProtoError> {
    Ok(line(tag, &format!("EXAMINE {}", quote(mailbox)?)))
}

pub fn cmd_uid_search_unseen(tag: &str) -> Zeroizing<Vec<u8>> {
    line(tag, "UID SEARCH UNSEEN")
}

/// `UID SEARCH SINCE 11-Nov-2026`.
pub fn cmd_uid_search_since(tag: &str, since: chrono::NaiveDate) -> Zeroizing<Vec<u8>> {
    use chrono::Datelike as _;
    let month = MONTHS[since.month0() as usize];
    line(
        tag,
        &format!("UID SEARCH SINCE {}-{month}-{}", since.day(), since.year()),
    )
}

/// The only FETCH the client sends: headers via `BODY.PEEK`, which never sets
/// `\Seen`.
pub const FETCH_ITEMS: &str =
    "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT MESSAGE-ID)])";

/// `UID FETCH <uids> (UID FLAGS INTERNALDATE BODY.PEEK[...])`; 1 to
/// [`MAX_UIDS`] UIDs.
pub fn cmd_uid_fetch(tag: &str, uids: &[u32]) -> Result<Zeroizing<Vec<u8>>, ProtoError> {
    if uids.is_empty() || uids.len() > MAX_UIDS {
        return Err(ProtoError::BodyLimit("uids"));
    }
    if uids.contains(&0) {
        return Err(ProtoError::Malformed("UID 0"));
    }
    let set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    Ok(line(tag, &format!("UID FETCH {set} {FETCH_ITEMS}")))
}

pub fn cmd_logout(tag: &str) -> Zeroizing<Vec<u8>> {
    line(tag, "LOGOUT")
}

#[cfg(test)]
#[path = "imap_proto_tests.rs"]
mod tests;
