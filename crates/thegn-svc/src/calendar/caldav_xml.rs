//! Bounded, namespace-aware reader for the small DAV multistatus subset used
//! by CalDAV. The transport caps the body; each admitted response is also
//! charged to the account admission meter.

use std::{borrow::Cow, collections::BTreeMap};

use super::CalendarError;

pub(super) const DAV_NS: &str = "DAV:";
pub(super) const CALDAV_NS: &str = "urn:ietf:params:xml:ns:caldav";

/// CalDAV's structural path is five elements deep; this leaves room for
/// vendor properties while refusing pathological nesting.
const MAX_XML_DEPTH: usize = 32;
/// Qualified names are tiny in real DAV documents; this bounds name storage.
const MAX_XML_NAME_BYTES: usize = 256;
/// A DAV element has only a few namespace declarations and attributes.
const MAX_XML_ATTRIBUTES: usize = 64;

#[derive(Debug)]
struct Element<'a> {
    ns: String,
    local: &'a str,
    inner: &'a str,
    text: String,
    children: Vec<Element<'a>>,
}

fn parse_error(offset: usize) -> CalendarError {
    CalendarError::Parse(format!("invalid CalDAV XML at byte {offset}"))
}

struct Reader<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Reader<'a> {
    fn fail<T>(&self) -> Result<T, CalendarError> {
        Err(parse_error(self.pos))
    }

    fn ws(&mut self) {
        while let Some(ch) = self.input[self.pos..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            self.pos += ch.len_utf8();
        }
    }

    fn skip_misc(&mut self) -> Result<(), CalendarError> {
        loop {
            self.ws();
            if self.input[self.pos..].starts_with("<!--") {
                let Some(end) = self.input[self.pos + 4..].find("-->") else {
                    return self.fail();
                };
                if self.input[self.pos + 4..self.pos + 4 + end].contains("--") {
                    return self.fail();
                }
                self.pos += 4 + end + 3;
            } else if self.input[self.pos..].starts_with("<?") {
                let Some(end) = self.input[self.pos + 2..].find("?>") else {
                    return self.fail();
                };
                self.pos += 2 + end + 2;
            } else {
                return Ok(());
            }
        }
    }

    fn name(&mut self) -> Result<&'a str, CalendarError> {
        let start = self.pos;
        while let Some(ch) = self.input[self.pos..].chars().next() {
            if ch.is_whitespace() || matches!(ch, '/' | '>' | '=' | '?' | '<') {
                break;
            }
            self.pos += ch.len_utf8();
        }
        let name = &self.input[start..self.pos];
        let mut chars = name.chars();
        let valid_start = chars
            .next()
            .is_some_and(|ch| ch.is_alphabetic() || matches!(ch, '_' | ':'));
        if name.is_empty()
            || name.len() > MAX_XML_NAME_BYTES
            || !valid_start
            || chars.any(|ch| !(ch.is_alphanumeric() || matches!(ch, '_' | ':' | '-' | '.')))
        {
            return self.fail();
        }
        Ok(name)
    }

    fn element(
        &mut self,
        inherited: &BTreeMap<String, String>,
        depth: usize,
        root_phase: &mut u8,
        visit_response: &mut impl FnMut(&Element<'a>) -> Result<(), CalendarError>,
    ) -> Result<Element<'a>, CalendarError> {
        if depth > MAX_XML_DEPTH || !self.input[self.pos..].starts_with('<') {
            return self.fail();
        }
        self.pos += 1;
        let qname = self.name()?;
        let mut attrs = Vec::new();
        let mut self_closing = false;
        loop {
            let before_space = self.pos;
            self.ws();
            let separated = self.pos > before_space;
            if self.input[self.pos..].starts_with("/>") {
                self.pos += 2;
                self_closing = true;
                break;
            }
            if self.input[self.pos..].starts_with('>') {
                self.pos += 1;
                break;
            }
            if !separated {
                return self.fail();
            }
            if attrs.len() >= MAX_XML_ATTRIBUTES {
                return self.fail();
            }
            let key = self.name()?;
            if attrs.iter().any(|(existing, _)| existing == &key) {
                return self.fail();
            }
            self.ws();
            if !self.input[self.pos..].starts_with('=') {
                return self.fail();
            }
            self.pos += 1;
            self.ws();
            let Some(quote) = self.input[self.pos..].chars().next() else {
                return self.fail();
            };
            if quote != '\'' && quote != '"' {
                return self.fail();
            }
            self.pos += 1;
            let value_start = self.pos;
            let Some(end) = self.input[self.pos..].find(quote) else {
                return self.fail();
            };
            self.pos += end;
            let value = &self.input[value_start..self.pos];
            if value.contains('<') {
                return self.fail();
            }
            validate_xml_text(value)?;
            attrs.push((key, value));
            self.pos += quote.len_utf8();
        }
        let has_namespace_decl = attrs
            .iter()
            .any(|(key, _)| key == "xmlns" || key.starts_with("xmlns:"));
        let mut owned_scope = None;
        for (key, value) in &attrs {
            if has_namespace_decl && (key == "xmlns" || key.starts_with("xmlns:")) {
                let scope = owned_scope.get_or_insert_with(|| inherited.clone());
                if key == "xmlns" {
                    scope.insert(String::new(), decode_text(value)?.into_owned());
                } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                    scope.insert(prefix.to_string(), decode_text(value)?.into_owned());
                }
            }
        }
        let scope = owned_scope.as_ref().unwrap_or(inherited);
        if qname.matches(':').count() > 1 {
            return self.fail();
        }
        let (prefix, local) = qname
            .split_once(':')
            .map_or((None, qname.as_str()), |(p, l)| (Some(p), l));
        if prefix.is_some_and(str::is_empty) || local.is_empty() || local.len() > MAX_XML_NAME_BYTES
        {
            return self.fail();
        }
        let ns = match prefix {
            Some(prefix) => match scope.get(prefix) {
                Some(ns) => ns.clone(),
                None => return self.fail(),
            },
            None => scope.get("").cloned().unwrap_or_default(),
        };
        if depth == 1 && (ns != DAV_NS || local != "multistatus") {
            return self.fail();
        }
        if depth == 2 && ns == DAV_NS {
            match local {
                "response" if *root_phase > 0 => return self.fail(),
                "responsedescription" if *root_phase > 0 => return self.fail(),
                "responsedescription" => *root_phase = 1,
                "sync-token" if *root_phase > 1 => return self.fail(),
                "sync-token" => *root_phase = 2,
                _ => {}
            }
        }
        let collect_text =
            ns == DAV_NS && ["multistatus", "response", "propstat", "prop"].contains(&local);
        let content_start = self.pos;
        let mut children = Vec::new();
        let mut text = String::new();
        if !self_closing {
            loop {
                if self.pos >= self.input.len() {
                    return self.fail();
                }
                if self.input[self.pos..].starts_with("</") {
                    let close_start = self.pos;
                    self.pos += 2;
                    let close = self.name()?;
                    self.ws();
                    if close != qname || !self.input[self.pos..].starts_with('>') {
                        return self.fail();
                    }
                    let content_end = close_start;
                    self.pos += 1;
                    let element = Element {
                        ns,
                        local,
                        inner: &self.input[content_start..content_end],
                        text,
                        children,
                    };
                    if depth == 2 && is(&element, DAV_NS, "response") {
                        visit_response(&element)?;
                    }
                    return Ok(element);
                }
                if self.input[self.pos..].starts_with("<!DOCTYPE") {
                    return self.fail();
                }
                if self.input[self.pos..].starts_with("<![CDATA[") {
                    self.pos += 9;
                    let Some(end) = self.input[self.pos..].find("]]>") else {
                        return self.fail();
                    };
                    if self.input[self.pos..self.pos + end]
                        .chars()
                        .any(|ch| !valid_xml_char(ch))
                    {
                        return self.fail();
                    }
                    if collect_text {
                        text.push_str(&self.input[self.pos..self.pos + end]);
                    }
                    self.pos += end + 3;
                } else if self.input[self.pos..].starts_with("<!--")
                    || self.input[self.pos..].starts_with("<?")
                {
                    self.skip_misc()?;
                } else if self.input[self.pos..].starts_with('<') {
                    let child = self.element(&scope, depth + 1, root_phase, visit_response)?;
                    if !(depth == 1 && is(&child, DAV_NS, "response")) {
                        children.push(child);
                    }
                } else {
                    let Some(end) = self.input[self.pos..].find('<') else {
                        return self.fail();
                    };
                    let raw = &self.input[self.pos..self.pos + end];
                    validate_xml_text(raw)?;
                    if collect_text {
                        text.push_str(&decode_text(raw)?);
                    }
                    self.pos += end;
                }
            }
        }
        let element = Element {
            ns,
            local,
            inner: &self.input[self.pos..self.pos],
            text,
            children,
        };
        if depth == 2 && is(&element, DAV_NS, "response") {
            visit_response(&element)?;
        }
        Ok(element)
    }
}

fn is(element: &Element<'_>, ns: &str, local: &str) -> bool {
    element.ns == ns && element.local == local
}

fn structural(local: &str) -> bool {
    [
        "multistatus",
        "response",
        "propstat",
        "prop",
        "href",
        "status",
        "sync-token",
    ]
    .contains(&local)
}

fn status_code(element: &Element<'_>) -> Result<u16, CalendarError> {
    if element.inner.len() > 1024 {
        return Err(CalendarError::Parse(
            "CalDAV HTTP status exceeds limit".into(),
        ));
    }
    let text = decode_text(element.inner.trim())?;
    let mut fields = text.split_whitespace();
    let version = fields.next().unwrap_or("");
    let code = fields.next().unwrap_or("");
    if !version
        .strip_prefix("HTTP/")
        .is_some_and(|version| !version.is_empty())
        || code.len() != 3
        || !code.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(CalendarError::Parse("invalid CalDAV HTTP status".into()));
    }
    let code = code
        .parse::<u16>()
        .map_err(|_| CalendarError::Parse("invalid CalDAV HTTP status".into()))?;
    if !(100..600).contains(&code) {
        return Err(CalendarError::Parse("invalid CalDAV HTTP status".into()));
    }
    Ok(code)
}

pub(super) struct DavResource<'a> {
    pub(super) href: String,
    pub(super) ics_raw: &'a str,
    pub(super) deleted: bool,
}

pub(super) fn walk_multistatus<'a>(
    input: &'a str,
    mut visit: impl FnMut(DavResource<'a>) -> Result<(), CalendarError>,
) -> Result<String, CalendarError> {
    let mut reader = Reader {
        input,
        pos: if input.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        },
    };
    let mut root_phase = 0;
    reader.skip_misc()?;
    let mut namespaces = BTreeMap::new();
    namespaces.insert("xml".into(), "http://www.w3.org/XML/1998/namespace".into());
    let root = reader.element(&namespaces, 1, &mut root_phase, &mut |response| {
        process_response(response, &mut visit)
    })?;
    reader.skip_misc()?;
    if reader.pos != input.len() {
        return Err(parse_error(reader.pos));
    }
    let mut token = String::new();
    let mut saw_token = false;
    let mut saw_description = false;
    let mut phase = 0_u8;
    for child in &root.children {
        if is(child, DAV_NS, "sync-token") {
            phase = 2;
            if saw_token {
                return Err(CalendarError::Parse("duplicate CalDAV sync-token".into()));
            }
            saw_token = true;
            if child.inner.trim().len() > crate::http::MAX_REQUEST_BYTES {
                return Err(CalendarError::BodyLimit(
                    "calendar sync token exceeds limit",
                ));
            }
            if !child.children.is_empty() {
                return Err(CalendarError::Parse(
                    "nested CalDAV sync-token content".into(),
                ));
            }
            token = decode_text(child.inner.trim())?.trim().to_string();
            if token.len() > crate::http::MAX_REQUEST_BYTES {
                return Err(CalendarError::BodyLimit(
                    "calendar sync token exceeds limit",
                ));
            }
        } else if child.ns == DAV_NS && child.local == "responsedescription" {
            if saw_description || phase > 1 {
                return Err(CalendarError::Parse(
                    "duplicate or misplaced CalDAV responsedescription".into(),
                ));
            }
            phase = 1;
            saw_description = true;
        } else if structural(&child.local) {
            return Err(parse_error(0));
        } else if child.ns == DAV_NS {
            return Err(CalendarError::Parse(
                "unexpected DAV multistatus child".into(),
            ));
        }
    }
    if !root.text.trim().is_empty() {
        return Err(parse_error(0));
    }
    Ok(token)
}

fn process_response<'a>(
    response: &Element<'a>,
    visit: &mut impl FnMut(DavResource<'a>) -> Result<(), CalendarError>,
) -> Result<(), CalendarError> {
    if !response.text.trim().is_empty() {
        return Err(CalendarError::Parse(
            "unexpected CalDAV response text".into(),
        ));
    }
    if response
        .children
        .iter()
        .any(|e| structural(&e.local) && e.ns != DAV_NS)
    {
        return Err(CalendarError::Parse(
            "wrong namespace for CalDAV response element".into(),
        ));
    }
    if response
        .children
        .iter()
        .any(|e| is(e, DAV_NS, "sync-token"))
    {
        return Err(CalendarError::Parse(
            "sync-token must be a multistatus child".into(),
        ));
    }
    if response.children.iter().any(|e| is(e, DAV_NS, "error")) {
        return Err(CalendarError::Api(
            "CalDAV resource reported a DAV error".into(),
        ));
    }
    if response.children.iter().any(|e| {
        e.ns == DAV_NS
            && ![
                "href",
                "status",
                "propstat",
                "responsedescription",
                "location",
                "error",
            ]
            .contains(&e.local.as_str())
    }) {
        return Err(CalendarError::Parse("unexpected DAV response child".into()));
    }
    if !response
        .children
        .first()
        .is_some_and(|e| is(e, DAV_NS, "href"))
    {
        return Err(CalendarError::Parse(
            "CalDAV href must lead its response".into(),
        ));
    }
    let hrefs: Vec<_> = response
        .children
        .iter()
        .filter(|e| is(e, DAV_NS, "href"))
        .collect();
    if hrefs.len() != 1 || !hrefs[0].children.is_empty() {
        return Err(CalendarError::Parse(
            "CalDAV response requires one href".into(),
        ));
    }
    let href_raw = hrefs[0].inner.trim();
    if href_raw.len() > crate::http::MAX_REQUEST_BYTES {
        return Err(CalendarError::BodyLimit(
            "calendar resource href exceeds limit",
        ));
    }
    let href = decode_text(href_raw)?.trim().to_string();
    if href.is_empty() {
        return Err(CalendarError::Parse(
            "CalDAV response requires one href".into(),
        ));
    }
    if href.len() > crate::http::MAX_REQUEST_BYTES {
        return Err(CalendarError::BodyLimit(
            "calendar resource href exceeds limit",
        ));
    }
    let statuses: Vec<_> = response
        .children
        .iter()
        .filter(|e| is(e, DAV_NS, "status"))
        .collect();
    let propstats: Vec<_> = response
        .children
        .iter()
        .filter(|e| is(e, DAV_NS, "propstat"))
        .collect();
    if !statuses.is_empty() && propstats.is_empty() {
        if statuses.len() != 1 {
            return Err(CalendarError::Parse(
                "duplicate CalDAV response status".into(),
            ));
        }
        let code = status_code(statuses[0])?;
        if !statuses[0].children.is_empty() {
            return Err(CalendarError::Parse("nested CalDAV status".into()));
        }
        if code == 404 {
            return visit(DavResource {
                href,
                ics_raw: "",
                deleted: true,
            });
        }
        return Err(CalendarError::Api(format!(
            "CalDAV resource status HTTP {code}"
        )));
    }
    if !statuses.is_empty() || propstats.is_empty() {
        return Err(CalendarError::Parse(
            "invalid CalDAV response status shape".into(),
        ));
    }
    let mut calendar: Option<(&str, u16)> = None;
    let mut saw_etag = false;
    for propstat in propstats {
        if !propstat.text.trim().is_empty() {
            return Err(CalendarError::Parse(
                "unexpected CalDAV propstat text".into(),
            ));
        }
        if propstat
            .children
            .iter()
            .any(|e| structural(&e.local) && e.ns != DAV_NS)
        {
            return Err(CalendarError::Parse(
                "wrong namespace for CalDAV propstat element".into(),
            ));
        }
        if propstat.children.iter().any(|e| {
            e.ns == DAV_NS
                && !["prop", "status", "responsedescription", "error"].contains(&e.local.as_str())
        }) {
            return Err(CalendarError::Parse("unexpected DAV propstat child".into()));
        }
        let props: Vec<_> = propstat
            .children
            .iter()
            .filter(|e| is(e, DAV_NS, "prop"))
            .collect();
        let statuses: Vec<_> = propstat
            .children
            .iter()
            .filter(|e| is(e, DAV_NS, "status"))
            .collect();
        if props.len() != 1 || statuses.len() != 1 {
            return Err(CalendarError::Parse(
                "CalDAV propstat requires prop and status".into(),
            ));
        }
        let code = status_code(statuses[0])?;
        if !statuses[0].children.is_empty() {
            return Err(CalendarError::Parse("nested CalDAV propstat status".into()));
        }
        if !props[0].text.trim().is_empty() {
            return Err(CalendarError::Parse("unexpected CalDAV prop text".into()));
        }
        for property in &props[0].children {
            if is(property, CALDAV_NS, "calendar-data") {
                if calendar.replace((property.inner, code)).is_some() {
                    return Err(CalendarError::Parse(
                        "duplicate CalDAV calendar-data".into(),
                    ));
                }
                if !property.children.is_empty() {
                    return Err(CalendarError::Parse("nested CalDAV calendar-data".into()));
                }
            }
            if is(property, DAV_NS, "getetag") {
                if saw_etag {
                    return Err(CalendarError::Parse("duplicate CalDAV getetag".into()));
                }
                saw_etag = true;
            }
        }
    }
    let Some((ics_raw, code)) = calendar else {
        return Err(CalendarError::Api(
            "CalDAV resource has no calendar-data".into(),
        ));
    };
    if !(200..300).contains(&code) {
        return Err(CalendarError::Api(format!(
            "CalDAV calendar-data unavailable: HTTP {code}"
        )));
    }
    if ics_raw.trim().is_empty() {
        return Err(CalendarError::Parse("CalDAV calendar-data is empty".into()));
    }
    visit(DavResource {
        href,
        ics_raw,
        deleted: false,
    })
}

fn valid_xml_char(ch: char) -> bool {
    matches!(ch as u32, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

fn scan_entity(raw: &str) -> Result<(char, usize), CalendarError> {
    let Some(after_ampersand) = raw.strip_prefix('&') else {
        return Err(CalendarError::Parse("invalid CalDAV entity".into()));
    };
    let Some(end) = after_ampersand.find(';') else {
        return Err(CalendarError::Parse("unterminated CalDAV entity".into()));
    };
    let entity = &after_ampersand[..end];
    let ch = match entity {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "quot" => '"',
        "apos" => '\'',
        _ if entity.starts_with("#x") || entity.starts_with("#X") => {
            let digits = &entity[2..];
            if !digits.as_bytes().first().is_some_and(u8::is_ascii_hexdigit) {
                return Err(CalendarError::Parse(
                    "invalid CalDAV character reference".into(),
                ));
            }
            let value = u32::from_str_radix(digits, 16)
                .ok()
                .and_then(char::from_u32);
            match value {
                Some(ch) if valid_xml_char(ch) => ch,
                _ => {
                    return Err(CalendarError::Parse(
                        "invalid CalDAV character reference".into(),
                    ));
                }
            }
        }
        _ if entity.starts_with('#') => {
            let digits = &entity[1..];
            if !digits.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                return Err(CalendarError::Parse(
                    "invalid CalDAV character reference".into(),
                ));
            }
            let value = digits.parse::<u32>().ok().and_then(char::from_u32);
            match value {
                Some(ch) if valid_xml_char(ch) => ch,
                _ => {
                    return Err(CalendarError::Parse(
                        "invalid CalDAV character reference".into(),
                    ));
                }
            }
        }
        _ => return Err(CalendarError::Parse("unknown CalDAV entity".into())),
    };
    Ok((ch, end + 2))
}

fn validate_xml_text(raw: &str) -> Result<(), CalendarError> {
    let mut rest = raw;
    while !rest.is_empty() {
        if rest.starts_with('&') {
            let (_, len) = scan_entity(rest)?;
            rest = &rest[len..];
        } else {
            let end = rest.find('&').unwrap_or(rest.len());
            if rest[..end]
                .chars()
                .any(|ch| ch == '<' || !valid_xml_char(ch))
            {
                return Err(CalendarError::Parse("invalid CalDAV XML text".into()));
            }
            rest = &rest[end..];
        }
    }
    Ok(())
}

pub(super) fn decode_text(raw: &str) -> Result<Cow<'_, str>, CalendarError> {
    if !raw.contains('&')
        && !raw.contains("<![CDATA[")
        && !raw.contains("<!--")
        && !raw.contains("<?")
    {
        if raw.contains('<') {
            return Err(CalendarError::Parse("invalid CalDAV XML text".into()));
        }
        if raw.chars().any(|ch| !valid_xml_char(ch)) {
            return Err(CalendarError::Parse("invalid CalDAV XML character".into()));
        }
        return Ok(Cow::Borrowed(raw));
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let Some(end) = cdata.find("]]>") else {
                return Err(CalendarError::Parse("unterminated CalDAV CDATA".into()));
            };
            out.push_str(&cdata[..end]);
            rest = &cdata[end + 3..];
        } else if let Some(comment) = rest.strip_prefix("<!--") {
            let Some(end) = comment.find("-->") else {
                return Err(CalendarError::Parse("unterminated CalDAV comment".into()));
            };
            if comment[..end].contains("--") {
                return Err(CalendarError::Parse("invalid CalDAV comment".into()));
            }
            rest = &comment[end + 3..];
        } else if let Some(pi) = rest.strip_prefix("<?") {
            let Some(end) = pi.find("?>") else {
                return Err(CalendarError::Parse(
                    "unterminated CalDAV processing instruction".into(),
                ));
            };
            rest = &pi[end + 2..];
        } else if rest.starts_with('&') {
            let (ch, len) = scan_entity(rest)?;
            out.push(ch);
            rest = &rest[len..];
        } else {
            let end = rest.find(['&', '<']).unwrap_or(rest.len());
            if end == 0 {
                return Err(CalendarError::Parse("invalid CalDAV XML text".into()));
            }
            if rest[..end].chars().any(|ch| !valid_xml_char(ch)) {
                return Err(CalendarError::Parse("invalid CalDAV XML character".into()));
            }
            out.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    Ok(Cow::Owned(out))
}

#[cfg(test)]
#[path = "caldav_xml_tests.rs"]
mod tests;
