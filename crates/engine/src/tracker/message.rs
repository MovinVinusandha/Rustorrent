//! Tracker-supplied text with best-effort URL redaction.
//!
//! A tracker can echo the announce URL, including a passkey, in a `failure
//! reason` or `warning message`. [`TrackerMessage`] stores a sanitized copy
//! for the UI but never exposes it through `Debug` or `Display`; those print
//! only the length. The UI must call [`TrackerMessage::sanitized_text`]
//! explicitly.
//!
//! Sanitization is bounded: the URL is at most 2048 bytes and the message at
//! most 256 bytes, so the token scan is O(URL tokens x 256) and the output is
//! at most 640 bytes.

use std::fmt;

use super::TrackerUrl;

/// Maximum stored message length in bytes.
pub const MAX_MESSAGE_LEN: usize = 256;

/// Shortest token that is replaced with `<redacted>`.
const MIN_REDACTED_TOKEN_LEN: usize = 4;

/// Replacement text for a matched URL token.
const REDACTED: &str = "<redacted>";

/// A tracker-supplied `failure reason` or `warning message`.
///
/// Construction is lossy and bounded: invalid UTF-8 becomes U+FFFD, control
/// characters are removed, and the text is truncated to
/// [`MAX_MESSAGE_LEN`] bytes on a UTF-8 boundary.
#[derive(Clone, PartialEq, Eq)]
pub struct TrackerMessage(String);

impl TrackerMessage {
    /// Stores tracker bytes after lossy conversion, control stripping and the
    /// 256-byte cap.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let lossy = String::from_utf8_lossy(bytes);
        let mut text = String::with_capacity(lossy.len().min(MAX_MESSAGE_LEN));
        for character in lossy.chars() {
            if character.is_control() {
                continue;
            }
            if text.len() + character.len_utf8() > MAX_MESSAGE_LEN {
                break;
            }
            text.push(character);
        }
        Self(text)
    }

    /// Returns the message with every announce-URL token replaced.
    ///
    /// Tokens come from the path segments, query names, query values and
    /// fragment. A token is percent-decoded first and replaced when it is at
    /// least 4 bytes long; matching is ASCII case-insensitive. The result is
    /// for display only and must not be logged.
    pub fn sanitized_text(&self, url: &TrackerUrl) -> String {
        let tokens = url_tokens(url);
        if tokens.is_empty() {
            return self.0.clone();
        }

        let mut ranges = Vec::new();
        for token in &tokens {
            let needle = token.as_bytes();
            let mut start = 0;
            while let Some(offset) = find_ignore_ascii_case(&self.0.as_bytes()[start..], needle) {
                let begin = start + offset;
                let end = begin + needle.len();
                ranges.push((begin, end));
                start = end;
            }
        }
        if ranges.is_empty() {
            return self.0.clone();
        }

        ranges.sort_unstable();
        let mut output = String::with_capacity(self.0.len());
        let mut cursor = 0;
        for (begin, end) in ranges {
            if begin < cursor {
                continue;
            }
            output.push_str(&self.0[cursor..begin]);
            output.push_str(REDACTED);
            cursor = end;
        }
        output.push_str(&self.0[cursor..]);
        output
    }

    /// Returns the stored length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` when the message is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for TrackerMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TrackerMessage({} bytes)", self.0.len())
    }
}

impl fmt::Display for TrackerMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TrackerMessage({} bytes)", self.0.len())
    }
}

fn url_tokens(url: &TrackerUrl) -> Vec<String> {
    let mut tokens = Vec::new();

    for segment in url.path().split('/') {
        push_token(&mut tokens, segment);
    }

    if let Some(query) = url.query() {
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some((name, value)) => {
                    push_token(&mut tokens, name);
                    push_token(&mut tokens, value);
                }
                None => push_token(&mut tokens, pair),
            }
        }
    }

    if let Some(fragment) = url.fragment() {
        push_token(&mut tokens, fragment);
    }

    tokens.sort_unstable();
    tokens.dedup();
    tokens
}

fn push_token(tokens: &mut Vec<String>, encoded: &str) {
    let decoded = percent_decode(encoded);
    if decoded.len() < MIN_REDACTED_TOKEN_LEN {
        return;
    }
    tokens.push(String::from_utf8_lossy(&decoded).into_owned());
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let high = bytes.get(index + 1).and_then(|byte| hex_value(*byte));
            let low = bytes.get(index + 2).and_then(|byte| hex_value(*byte));
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        decoded.push(byte);
        index += 1;
    }

    decoded
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Finds `needle` in `haystack`, folding ASCII case.
///
/// `needle` comes from a lossy UTF-8 string, so it starts with an ASCII byte,
/// a UTF-8 leading byte, or the U+FFFD leading byte. It can therefore only
/// match at a character boundary, even though the search is over bytes.
fn find_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests;
