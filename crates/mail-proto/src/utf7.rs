//! IMAP mailbox names use modified UTF-7 (RFC 3501 section 5.1.3): printable
//! US-ASCII passes through, `&` becomes `&-`, and runs of other characters are
//! base64 (UTF-16BE, `/` replaced by `,`) wrapped in `&` and `-`.
//!
//! The public API of this crate speaks UTF-8; encoding happens only here.

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine as _;

/// Decodes a wire name; `None` means the server sent malformed modified UTF-7.
pub(crate) fn decode(input: &str) -> Option<String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        if chars.as_str().starts_with('-') {
            chars.next();
            out.push('&');
            continue;
        }
        let mut encoded = String::new();
        let mut closed = false;
        for ch in chars.by_ref() {
            if ch == '-' {
                closed = true;
                break;
            }
            encoded.push(if ch == ',' { '/' } else { ch });
        }
        if !closed || encoded.is_empty() {
            return None;
        }
        let encoded = encoded.trim_end_matches('=');
        let bytes = STANDARD_NO_PAD.decode(encoded).ok()?;
        let (pairs, remainder) = bytes.as_chunks::<2>();
        if !remainder.is_empty() {
            return None;
        }
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_be_bytes(*pair)).collect();
        out.push_str(&String::from_utf16(&units).ok()?);
    }
    Some(out)
}

/// Encodes a UTF-8 mailbox name for the wire.
pub(crate) fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending: Vec<u16> = Vec::new();

    fn flush(pending: &mut Vec<u16>, out: &mut String) {
        if pending.is_empty() {
            return;
        }
        let bytes: Vec<u8> = pending.iter().flat_map(|unit| unit.to_be_bytes()).collect();
        let encoded = STANDARD_NO_PAD.encode(bytes).replace('/', ",");
        out.push('&');
        out.push_str(&encoded);
        out.push('-');
        pending.clear();
    }

    for ch in input.chars() {
        match ch {
            '&' => {
                flush(&mut pending, &mut out);
                out.push_str("&-");
            }
            ' '..='~' => {
                flush(&mut pending, &mut out);
                out.push(ch);
            }
            _ => {
                let mut buf = [0u16; 2];
                pending.extend_from_slice(ch.encode_utf16(&mut buf));
            }
        }
    }
    flush(&mut pending, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_rfc_3501_example() {
        let wire = "~peter/mail/&U,BTFw-/&ZeVnLIqe-";
        let decoded = decode(wire).expect("valid");
        assert_eq!(decoded, "~peter/mail/台北/日本語");
        assert_eq!(encode(&decoded), wire);
    }

    #[test]
    fn escapes_ampersands_and_leaves_ascii_alone() {
        assert_eq!(encode("A&B"), "A&-B");
        assert_eq!(decode("A&-B").as_deref(), Some("A&B"));
        assert_eq!(encode("INBOX/Sent"), "INBOX/Sent");
    }

    #[test]
    fn rejects_malformed_sequences() {
        assert!(decode("&").is_none());
        assert!(decode("&abc").is_none());
        assert!(decode("&!!!!-").is_none());
        assert!(
            decode("&A-").is_none(),
            "base64 must hold whole UTF-16 units"
        );
    }
}
