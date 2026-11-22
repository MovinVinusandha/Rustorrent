//! Sanitized, collision-free mapping from torrent files to relative paths.
//!
//! This module performs no I/O. Every path it produces is relative and safe to
//! join below a download root.

// The sanitizer is exercised by tests in this commit; `FileMap` consumes it in
// the next commit.
#![allow(dead_code)]

use sha1::{Digest, Sha1};

/// Maximum size of one sanitized path component, in bytes.
const MAX_COMPONENT_BYTES: usize = 255;

/// Repairs one torrent path component so it is safe on every supported
/// platform.
///
/// The result is non-empty, at most [`MAX_COMPONENT_BYTES`] bytes, free of
/// path separators and Windows-invalid characters, not a reserved device name,
/// and does not end in a dot or space. The function is idempotent.
fn sanitize_component(input: &str) -> String {
    let repaired = repair_component(input);
    if repaired.len() <= MAX_COMPONENT_BYTES {
        return repaired;
    }
    truncate_with_hash(input, &repaired)
}

/// Applies the repair rules before truncation.
fn repair_component(input: &str) -> String {
    if input.is_empty() || input == "." || input == ".." {
        return "_".to_owned();
    }

    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        let replacement = match character {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            value if value.is_ascii_control() => '_',
            value => value,
        };
        output.push(replacement);
    }

    if is_reserved_name(&output) {
        output.insert(0, '_');
    }

    let trimmed_len = output.trim_end_matches(['.', ' ']).len();
    if trimmed_len < output.len() {
        output.truncate(trimmed_len);
        output.push('_');
    }

    output
}

/// Returns `true` for Windows reserved device names, with or without an
/// extension.
fn is_reserved_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let stem = stem.trim_end_matches([' ', '.']);
    let upper = stem.to_ascii_uppercase();

    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
}

/// Truncates a repaired component to the cap and appends a deterministic hash
/// suffix derived from the original input.
fn truncate_with_hash(original: &str, repaired: &str) -> String {
    let digest = Sha1::digest(original.as_bytes());
    let suffix = format!("~{}", hex8(&digest));
    let max_base = MAX_COMPONENT_BYTES.saturating_sub(suffix.len());
    let base = truncate_utf8(repaired, max_base);
    let mut output = String::with_capacity(base.len() + suffix.len());
    output.push_str(base);
    output.push_str(&suffix);
    output
}

/// Truncates `text` to at most `max` bytes without splitting a character.
fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }

    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Formats the first four bytes of a digest as eight lowercase hex characters.
fn hex8(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(8);
    for byte in bytes.iter().take(4) {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Builds the case-insensitive key used to detect collisions.
///
/// Components are sanitized first, so this only needs to fold case and join
/// with `/`. Unicode normalization is deliberately not applied in v0.1.
fn collision_key(components: &[String]) -> String {
    components.join("/").to_lowercase()
}

/// Adds a deterministic `~N` suffix to one component.
///
/// If the component has a dot at an index other than zero and an extension of
/// at most 10 bytes, the suffix goes before the last dot. Otherwise it is
/// appended. The result is truncated to [`MAX_COMPONENT_BYTES`] while keeping
/// the suffix.
fn suffix_component(component: &str, attempt: u32) -> String {
    let suffix = format!("~{attempt}");

    if let Some((stem, extension)) = split_extension(component) {
        let fixed = suffix.len() + 1 + extension.len();
        if fixed < MAX_COMPONENT_BYTES {
            let max_stem = MAX_COMPONENT_BYTES - fixed;
            let stem = truncate_utf8(stem, max_stem);
            let mut output = String::with_capacity(stem.len() + fixed);
            output.push_str(stem);
            output.push_str(&suffix);
            output.push('.');
            output.push_str(extension);
            return output;
        }
    }

    let max_base = MAX_COMPONENT_BYTES.saturating_sub(suffix.len());
    let base = truncate_utf8(component, max_base);
    let mut output = String::with_capacity(base.len() + suffix.len());
    output.push_str(base);
    output.push_str(&suffix);
    output
}

/// Splits a component into stem and extension when the extension rule applies.
fn split_extension(component: &str) -> Option<(&str, &str)> {
    let dot = component.rfind('.')?;
    if dot == 0 {
        return None;
    }

    let extension = &component[dot + 1..];
    if extension.is_empty() || extension.len() > 10 {
        return None;
    }

    Some((&component[..dot], extension))
}

#[cfg(test)]
mod tests;
