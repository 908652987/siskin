//! Builds outgoing MIME messages: a base64 UTF-8 text body, optional
//! attachments, and an envelope recipient list that also covers Bcc.
//!
//! Header values are validated here because they reach the wire verbatim: a
//! line break in a subject, address or filename would inject extra headers.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Utc;
use thiserror::Error;
use uuid::Uuid;

/// Upper bound on the combined attachment payload of one message.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_FILENAME_CHARS: usize = 255;
const FALLBACK_MEDIA_TYPE: &str = "application/octet-stream";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub filename: String,
    /// Declared media type; invalid values fall back to `application/octet-stream`.
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub attachments: Vec<OutgoingAttachment>,
}

/// One message ready for SMTP: the raw bytes carry `To`/`Cc` (never `Bcc`),
/// while `envelope` lists every address the server must deliver to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedMessage {
    pub envelope: Vec<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ComposeError {
    #[error("no recipients")]
    NoRecipients,
    #[error("invalid address: {0:?}")]
    InvalidAddress(String),
    #[error("{field} contains a control character")]
    UnsafeHeader { field: &'static str },
    #[error("attachment {0:?} has an unusable filename")]
    InvalidFilename(String),
    #[error("attachments total {bytes} bytes, over the {limit} byte limit")]
    AttachmentsTooLarge { bytes: usize, limit: usize },
}

/// Renders `message` as RFC 5322 bytes and returns the SMTP envelope.
pub fn compose(from: &str, message: &OutgoingMessage) -> Result<ComposedMessage, ComposeError> {
    let from = normalize_address(from)?;
    let to = normalize_addresses(&message.to)?;
    let cc = normalize_addresses(&message.cc)?;
    let bcc = normalize_addresses(&message.bcc)?;
    let subject = encode_subject(&header_value("subject", &message.subject)?);

    let mut envelope: Vec<String> = Vec::new();
    for address in to.iter().chain(&cc).chain(&bcc) {
        if !envelope
            .iter()
            .any(|seen| seen.eq_ignore_ascii_case(address))
        {
            envelope.push(address.clone());
        }
    }
    if envelope.is_empty() {
        return Err(ComposeError::NoRecipients);
    }

    let attachment_bytes = message
        .attachments
        .iter()
        .try_fold(0usize, |total, attachment| {
            total
                .checked_add(attachment.data.len())
                .ok_or(ComposeError::AttachmentsTooLarge {
                    bytes: usize::MAX,
                    limit: MAX_ATTACHMENT_BYTES,
                })
        })?;
    if attachment_bytes > MAX_ATTACHMENT_BYTES {
        return Err(ComposeError::AttachmentsTooLarge {
            bytes: attachment_bytes,
            limit: MAX_ATTACHMENT_BYTES,
        });
    }

    let domain = from
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .unwrap_or("localhost");
    let mut out = String::with_capacity(message.body_text.len() * 2 + attachment_bytes * 2);
    out.push_str(&format!("From: {from}\r\n"));
    if !to.is_empty() {
        out.push_str(&format!("To: {}\r\n", to.join(", ")));
    }
    if !cc.is_empty() {
        out.push_str(&format!("Cc: {}\r\n", cc.join(", ")));
    }
    out.push_str(&format!(
        "Subject: {subject}\r\nDate: {}\r\nMessage-ID: <{}@{}>\r\nMIME-Version: 1.0\r\n",
        Utc::now().to_rfc2822(),
        Uuid::new_v4(),
        domain
    ));

    if message.attachments.is_empty() {
        out.push_str(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        );
        out.push_str(&wrap_base64(&STANDARD.encode(&message.body_text)));
        out.push_str("\r\n");
    } else {
        let boundary = format!("=_siskin_{}_", Uuid::new_v4().simple());
        out.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n"
        ));
        out.push_str(&format!("--{boundary}\r\n"));
        out.push_str(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        );
        out.push_str(&wrap_base64(&STANDARD.encode(&message.body_text)));
        out.push_str("\r\n");
        for attachment in &message.attachments {
            let filename = normalize_filename(&attachment.filename)?;
            let media_type = media_type(attachment.content_type.as_deref());
            out.push_str(&format!("--{boundary}\r\n"));
            out.push_str(&format!("Content-Type: {media_type}\r\n"));
            out.push_str("Content-Transfer-Encoding: base64\r\n");
            out.push_str(&format!(
                "Content-Disposition: attachment; filename={}\r\n\r\n",
                encode_parameter(&filename)
            ));
            out.push_str(&wrap_base64(&STANDARD.encode(&attachment.data)));
            out.push_str("\r\n");
        }
        out.push_str(&format!("--{boundary}--\r\n"));
    }

    Ok(ComposedMessage {
        envelope,
        bytes: out.into_bytes(),
    })
}

fn normalize_addresses(raw: &[String]) -> Result<Vec<String>, ComposeError> {
    raw.iter()
        .map(|address| normalize_address(address))
        .collect()
}

/// Accepts only a plain `local@domain` address: display names, comments and
/// non-ASCII (SMTPUTF8) addresses are rejected rather than mis-encoded.
fn normalize_address(raw: &str) -> Result<String, ComposeError> {
    let address = raw.trim();
    let invalid = || ComposeError::InvalidAddress(raw.to_string());
    if address.is_empty() || !address.is_ascii() {
        return Err(invalid());
    }
    if address
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace() || "<>,;:\\\"()[]".contains(ch))
    {
        return Err(invalid());
    }
    let Some((local, domain)) = address.split_once('@') else {
        return Err(invalid());
    };
    let local_ok = !local.is_empty()
        && !local.starts_with('.')
        && !local.ends_with('.')
        && local
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~.".contains(ch));
    let domain_ok = !domain.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && domain
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.');
    if !local_ok || !domain_ok {
        return Err(invalid());
    }
    Ok(address.to_string())
}

fn header_value(field: &'static str, raw: &str) -> Result<String, ComposeError> {
    let value = raw.trim();
    if value.chars().any(char::is_control) {
        return Err(ComposeError::UnsafeHeader { field });
    }
    Ok(value.to_string())
}

/// ASCII subjects pass through; anything else becomes folded encoded words so
/// no single line approaches the 998-octet limit.
fn encode_subject(subject: &str) -> String {
    if subject.is_ascii() {
        return subject.to_string();
    }
    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for ch in subject.chars() {
        if chunk.len() + ch.len_utf8() > 45 {
            words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(chunk.as_bytes())));
    }
    words.join("\r\n ")
}

fn normalize_filename(raw: &str) -> Result<String, ComposeError> {
    let name = raw.trim();
    let invalid = || ComposeError::InvalidFilename(raw.to_string());
    if name.is_empty() || name.chars().count() > MAX_FILENAME_CHARS {
        return Err(invalid());
    }
    if name
        .chars()
        .any(|ch| ch.is_control() || ch == '/' || ch == '\\')
    {
        return Err(invalid());
    }
    Ok(name.to_string())
}

/// Quoted `filename` parameter; non-ASCII names use an RFC 2047 encoded word,
/// which is what `mail-parse` (and most clients) decode.
fn encode_parameter(name: &str) -> String {
    if name.is_ascii() {
        format!("\"{}\"", name.replace('"', "\\\""))
    } else {
        format!("\"=?UTF-8?B?{}?=\"", STANDARD.encode(name))
    }
}

fn media_type(raw: Option<&str>) -> String {
    let Some(value) = raw.map(str::trim) else {
        return FALLBACK_MEDIA_TYPE.to_string();
    };
    let valid = value.split_once('/').is_some_and(|(kind, subtype)| {
        !kind.is_empty()
            && !subtype.is_empty()
            && kind.chars().all(is_token_char)
            && subtype.chars().all(is_token_char)
    });
    if valid {
        value.to_string()
    } else {
        FALLBACK_MEDIA_TYPE.to_string()
    }
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(ch)
}

fn wrap_base64(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2);
    for (index, chunk) in encoded.as_bytes().chunks(76).enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        out.push_str(&String::from_utf8_lossy(chunk));
    }
    out
}

/// Byte offset where the header section ends, and the separator length.
fn find_header_end(raw: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index < raw.len() {
        let rest = &raw[index..];
        if rest.starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if rest.starts_with(b"\n\n") {
            return Some((index, 2));
        }
        index += 1;
    }
    None
}

/// Derives an SMTP envelope from an already composed message. Used when a
/// JMAP client omits `envelope` on `EmailSubmission/set` (RFC 8621 §7.1):
/// recipients come from `To`, `Cc` and `Bcc`, since Bcc addresses exist only
/// in the header until the server strips it.
pub fn derive_envelope(raw: &[u8]) -> Result<Vec<String>, ComposeError> {
    let mut envelope: Vec<String> = Vec::new();
    for field in ["to", "cc", "bcc"] {
        let Some(value) = mail_parse::raw_header_value(raw, field) else {
            continue;
        };
        let decoded = mail_parse::decode_encoded_words(&value);
        for token in split_address_list(&decoded) {
            let Ok(address) = normalize_address(&token) else {
                continue;
            };
            if !envelope
                .iter()
                .any(|seen| seen.eq_ignore_ascii_case(&address))
            {
                envelope.push(address);
            }
        }
    }
    if envelope.is_empty() {
        return Err(ComposeError::NoRecipients);
    }
    Ok(envelope)
}

/// Removes every `Bcc` header field (with its folded continuation lines) from
/// the header section, leaving the body untouched. RFC 8621 §7.5 requires the
/// server to do this during delivery.
pub fn strip_bcc_header(raw: &[u8]) -> Vec<u8> {
    let (header_end, _) = match find_header_end(raw) {
        Some(bounds) => bounds,
        None => (raw.len(), 0),
    };
    let header = &raw[..header_end];
    let mut out = Vec::with_capacity(raw.len());
    let mut skip = false;
    for line in header.split_inclusive(|byte| *byte == b'\n') {
        let end = line
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(line.len());
        let mut content = &line[..end];
        if content.last() == Some(&b'\r') {
            content = &content[..content.len() - 1];
        }
        let continuation = matches!(content.first(), Some(b' ') | Some(b'\t'));
        if continuation && skip {
            continue;
        }
        skip = false;
        let is_bcc =
            content.len() >= 4 && content[..3].eq_ignore_ascii_case(b"bcc") && content[3] == b':';
        if is_bcc {
            skip = true;
            continue;
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(&raw[header_end..]);
    out
}

/// Splits a header value into addr-spec tokens, honouring quoted strings,
/// comments and angle addresses. Display names are dropped.
fn split_address_list(header: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut in_quote = false;
    for ch in header.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                current.push(ch);
            }
            '<' | '(' if !in_quote => {
                depth += 1;
                current.push(ch);
            }
            '>' | ')' if !in_quote => {
                depth -= 1;
                current.push(ch);
            }
            ',' if !in_quote && depth == 0 => {
                tokens.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    tokens.push(current);
    tokens
        .iter()
        .map(|token| addr_spec(token))
        .filter(|token| !token.is_empty())
        .collect()
}

fn addr_spec(token: &str) -> String {
    let token = token.trim();
    if let (Some(open), Some(close)) = (token.rfind('<'), token.rfind('>')) {
        if open < close {
            return token[open + 1..close].trim().to_string();
        }
    }
    let mut out = String::new();
    let mut depth = 0i32;
    for ch in token.chars() {
        match ch {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parse::parse_message;

    fn attachment(name: &str, content_type: &str, data: &[u8]) -> OutgoingAttachment {
        OutgoingAttachment {
            filename: name.to_string(),
            content_type: Some(content_type.to_string()),
            data: data.to_vec(),
        }
    }

    #[test]
    fn encodes_subject_and_body() {
        let message = OutgoingMessage {
            to: vec!["peer@example.com".into()],
            subject: "你好".into(),
            body_text: "hello".into(),
            ..OutgoingMessage::default()
        };
        let composed = compose("sender@example.com", &message).expect("compose");
        let raw = String::from_utf8(composed.bytes).expect("ascii");
        assert!(raw.contains("To: peer@example.com\r\n"));
        assert!(raw.contains("Subject: =?UTF-8?B?"));
        assert!(raw.contains("Message-ID: <"));
        assert!(raw.contains("@example.com>"));
        assert!(raw.contains("aGVsbG8="));
        assert!(raw.ends_with("\r\n"));
        assert_eq!(composed.envelope, vec!["peer@example.com".to_string()]);
    }

    #[test]
    fn bcc_reaches_the_envelope_but_not_the_headers() {
        let message = OutgoingMessage {
            to: vec!["a@example.com".into()],
            cc: vec!["b@example.com".into()],
            bcc: vec!["c@example.com".into()],
            subject: "hi".into(),
            body_text: "body".into(),
            ..OutgoingMessage::default()
        };
        let composed = compose("me@example.com", &message).expect("compose");
        let raw = String::from_utf8(composed.bytes).expect("ascii");
        assert!(raw.contains("To: a@example.com\r\n"));
        assert!(raw.contains("Cc: b@example.com\r\n"));
        assert!(!raw.contains("Bcc:"));
        assert!(!raw.contains("c@example.com"));
        assert_eq!(
            composed.envelope,
            vec![
                "a@example.com".to_string(),
                "b@example.com".to_string(),
                "c@example.com".to_string()
            ]
        );
    }

    #[test]
    fn duplicate_recipients_are_folded_case_insensitively() {
        let message = OutgoingMessage {
            to: vec!["Peer@Example.com".into(), "peer@example.com".into()],
            cc: vec!["PEER@example.com".into()],
            ..OutgoingMessage::default()
        };
        let composed = compose("me@example.com", &message).expect("compose");
        assert_eq!(composed.envelope, vec!["Peer@Example.com".to_string()]);
    }

    #[test]
    fn attachments_round_trip_through_the_parser() {
        let message = OutgoingMessage {
            to: vec!["peer@example.com".into()],
            subject: "files".into(),
            body_text: "see attached".into(),
            attachments: vec![
                attachment("note.txt", "text/plain", b"hello attachment"),
                attachment("报告.pdf", "application/pdf", b"%PDF-1.7"),
            ],
            ..OutgoingMessage::default()
        };
        let composed = compose("me@example.com", &message).expect("compose");
        let parsed = parse_message(&composed.bytes);
        assert_eq!(parsed.text.as_deref(), Some("see attached"));
        assert_eq!(parsed.attachments.len(), 2);
        let names: Vec<_> = parsed
            .attachments
            .iter()
            .filter_map(|part| part.filename.as_deref())
            .collect();
        assert!(names.contains(&"note.txt"));
        assert!(names.contains(&"报告.pdf"));
        let text = parsed
            .attachments
            .iter()
            .find(|part| part.filename.as_deref() == Some("note.txt"))
            .expect("text attachment");
        assert_eq!(text.bytes, b"hello attachment");
        assert_eq!(text.content_type, "text/plain");
    }

    #[test]
    fn rejects_header_injection_and_bad_addresses() {
        let injected = OutgoingMessage {
            to: vec!["peer@example.com\r\nBcc: victim@example.com".into()],
            ..OutgoingMessage::default()
        };
        assert_eq!(
            compose("me@example.com", &injected),
            Err(ComposeError::InvalidAddress(
                "peer@example.com\r\nBcc: victim@example.com".into()
            ))
        );

        let subject = OutgoingMessage {
            to: vec!["peer@example.com".into()],
            subject: "hi\r\nBcc: victim@example.com".into(),
            ..OutgoingMessage::default()
        };
        assert_eq!(
            compose("me@example.com", &subject),
            Err(ComposeError::UnsafeHeader { field: "subject" })
        );

        let filename = OutgoingMessage {
            to: vec!["peer@example.com".into()],
            attachments: vec![attachment("../escape", "text/plain", b"x")],
            ..OutgoingMessage::default()
        };
        assert_eq!(
            compose("me@example.com", &filename),
            Err(ComposeError::InvalidFilename("../escape".into()))
        );

        let no_at = OutgoingMessage {
            to: vec!["not-an-address".into()],
            ..OutgoingMessage::default()
        };
        assert!(matches!(
            compose("me@example.com", &no_at),
            Err(ComposeError::InvalidAddress(_))
        ));

        let display_name = OutgoingMessage {
            to: vec!["Peer <peer@example.com>".into()],
            ..OutgoingMessage::default()
        };
        assert!(matches!(
            compose("me@example.com", &display_name),
            Err(ComposeError::InvalidAddress(_))
        ));
    }

    #[test]
    fn requires_at_least_one_recipient() {
        let message = OutgoingMessage {
            subject: "hi".into(),
            body_text: "body".into(),
            ..OutgoingMessage::default()
        };
        assert_eq!(
            compose("me@example.com", &message),
            Err(ComposeError::NoRecipients)
        );
    }

    #[test]
    fn rejects_oversized_attachments() {
        let message = OutgoingMessage {
            to: vec!["peer@example.com".into()],
            attachments: vec![OutgoingAttachment {
                filename: "big.bin".into(),
                content_type: None,
                data: vec![0u8; MAX_ATTACHMENT_BYTES + 1],
            }],
            ..OutgoingMessage::default()
        };
        assert_eq!(
            compose("me@example.com", &message),
            Err(ComposeError::AttachmentsTooLarge {
                bytes: MAX_ATTACHMENT_BYTES + 1,
                limit: MAX_ATTACHMENT_BYTES,
            })
        );
    }

    #[test]
    fn derives_an_envelope_from_headers_without_duplicates() {
        let raw = b"From: me@example.com\r\n\
                    To: Peer <peer@example.com>, second@example.com\r\n\
                    Cc: =?utf-8?B?5Lit5paH?= <third@example.com>\r\n\
                    Bcc: hidden@example.com\r\n\
                    \r\nbody";
        let envelope = derive_envelope(raw).expect("envelope");
        assert_eq!(
            envelope,
            vec![
                "peer@example.com".to_string(),
                "second@example.com".to_string(),
                "third@example.com".to_string(),
                "hidden@example.com".to_string(),
            ]
        );

        let duplicate = b"To: A <a@example.com>, a@example.com\r\n\r\nbody";
        assert_eq!(
            derive_envelope(duplicate).expect("envelope"),
            vec!["a@example.com".to_string()]
        );

        assert_eq!(
            derive_envelope(b"Subject: none\r\n\r\nbody"),
            Err(ComposeError::NoRecipients)
        );
    }

    #[test]
    fn removes_bcc_headers_and_their_continuations_only() {
        let raw = b"To: peer@example.com\r\n\
                    Bcc: first@example.com,\r\n\
                    \tsecond@example.com\r\n\
                    Subject: hi\r\n\
                    \r\nBcc: not a header\r\n";
        let stripped = strip_bcc_header(raw);
        let text = String::from_utf8(stripped.clone()).expect("utf8");
        assert_eq!(
            text,
            "To: peer@example.com\r\nSubject: hi\r\n\r\nBcc: not a header\r\n"
        );
        // The body still contains the literal text; only the header section lost
        // its Bcc fields.
        assert!(stripped.ends_with(b"Bcc: not a header\r\n"));

        let without = strip_bcc_header(b"Subject: plain\r\n\r\nbody");
        assert_eq!(without, b"Subject: plain\r\n\r\nbody");
    }
}
