//! Canonical bencode decoding.
//!
//! This module implements the subset of bencode needed by BitTorrent
//! metainfo. Decoding is strict by default: malformed or non-canonical input
//! is rejected instead of repaired.
//!
//! The decoder borrows byte strings from the input and records the byte range
//! of every decoded value. That range is what lets the metainfo parser hash
//! the original `info` bytes rather than a re-encoding.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use thiserror::Error;

/// Maximum total input size accepted by the decoder.
pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

/// Maximum size of one bencoded byte string.
pub const MAX_STRING_BYTES: usize = 16 * 1024 * 1024;

/// Maximum length of an integer literal, including its sign.
pub const MAX_INT_BYTES: usize = 20;

/// Maximum nesting depth of lists and dictionaries.
pub const MAX_DEPTH: usize = 64;

/// Maximum number of entries in one list or dictionary.
pub const MAX_CONTAINER_ITEMS: usize = 100_000;

/// Maximum number of decoded values in one input, including dictionary keys.
pub const MAX_NODES: usize = 1_000_000;

/// Maximum number of digits accepted in a string length prefix.
const MAX_LENGTH_DIGITS: usize = 20;

/// Resource limits for one decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum total input size in bytes.
    pub max_input_bytes: usize,
    /// Maximum size of one byte string in bytes.
    pub max_string_bytes: usize,
    /// Maximum integer literal length in bytes, including the sign.
    pub max_int_bytes: usize,
    /// Maximum number of nested containers, counting the top-level container.
    pub max_depth: usize,
    /// Maximum number of entries in one list or dictionary.
    pub max_container_items: usize,
    /// Maximum number of decoded values, including dictionary keys.
    pub max_nodes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: MAX_INPUT_BYTES,
            max_string_bytes: MAX_STRING_BYTES,
            max_int_bytes: MAX_INT_BYTES,
            max_depth: MAX_DEPTH,
            max_container_items: MAX_CONTAINER_ITEMS,
            max_nodes: MAX_NODES,
        }
    }
}

/// Dictionary key ordering accepted by the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOrder {
    /// Keys must be strictly increasing in raw byte order.
    ///
    /// This is the canonical form required for a byte-for-byte round-trip.
    Strict,
    /// Keys may appear in any order, but duplicates are still rejected.
    ///
    /// This is used for metainfo, where the info-hash is taken from the
    /// original byte range rather than from a re-encoding.
    Lenient,
}

/// Options for one decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeOptions {
    /// Resource limits.
    pub limits: Limits,
    /// Dictionary key ordering rule.
    pub key_order: KeyOrder,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            key_order: KeyOrder::Strict,
        }
    }
}

/// A decoded bencode value borrowed from the input.
///
/// Equality ignores the source span. Two values are equal when their kinds and
/// contents are equal, even if they came from different byte offsets.
#[derive(Debug, Clone)]
pub struct Value<'a> {
    kind: Kind<'a>,
    span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<Value<'a>>),
    Dict(BTreeMap<&'a [u8], Value<'a>>),
}

impl<'a> PartialEq for Value<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

impl<'a> Eq for Value<'a> {}

impl<'a> Value<'a> {
    /// Builds an integer value.
    pub fn int(value: i64) -> Self {
        Self {
            kind: Kind::Int(value),
            span: 0..0,
        }
    }

    /// Builds a byte-string value.
    pub fn bytes(value: &'a [u8]) -> Self {
        Self {
            kind: Kind::Bytes(value),
            span: 0..0,
        }
    }

    /// Builds a list value.
    pub fn list(values: Vec<Value<'a>>) -> Self {
        Self {
            kind: Kind::List(values),
            span: 0..0,
        }
    }

    /// Builds a dictionary value with canonical key order.
    ///
    /// Entries are sorted by raw key bytes before they are stored, so callers
    /// may pass keys in any order. Duplicate keys are rejected because a
    /// canonical dictionary cannot contain them; the returned error uses
    /// offset `0` because the value was built rather than decoded.
    pub fn dict(mut entries: Vec<(&'a [u8], Value<'a>)>) -> Result<Self, Error> {
        entries.sort_by(|left, right| left.0.cmp(right.0));

        for pair in entries.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(Error::DuplicateDictKey { offset: 0 });
            }
        }

        Ok(Self {
            kind: Kind::Dict(entries.into_iter().collect()),
            span: 0..0,
        })
    }

    /// Returns the integer value, or `None` for another value kind.
    pub fn as_int(&self) -> Option<i64> {
        match &self.kind {
            Kind::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the byte string, or `None` for another value kind.
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match &self.kind {
            Kind::Bytes(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the list entries, or `None` for another value kind.
    pub fn as_list(&self) -> Option<&[Value<'a>]> {
        match &self.kind {
            Kind::List(values) => Some(values),
            _ => None,
        }
    }

    /// Returns the dictionary entries, or `None` for another value kind.
    pub fn as_dict(&self) -> Option<&BTreeMap<&'a [u8], Value<'a>>> {
        match &self.kind {
            Kind::Dict(values) => Some(values),
            _ => None,
        }
    }

    /// Looks up a dictionary key, returning `None` for a missing key or a
    /// non-dictionary value.
    pub fn dict_get(&self, key: &[u8]) -> Option<&Value<'a>> {
        self.as_dict()?.get(key)
    }

    /// Returns the byte range occupied by this value in the original input.
    pub fn span(&self) -> Range<usize> {
        self.span.clone()
    }
}

/// Errors produced while decoding bencode.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// The input is larger than the configured limit.
    #[error("input is {len} bytes, limit is {max}")]
    InputTooLarge {
        /// Actual input size.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// The input ended before a value was complete.
    #[error("unexpected end of input at offset {offset}")]
    Truncated {
        /// Offset where more input was needed.
        offset: usize,
    },
    /// Bytes remain after the top-level value.
    #[error("trailing data at offset {offset}")]
    TrailingData {
        /// Offset of the first trailing byte.
        offset: usize,
    },
    /// A list or dictionary is nested deeper than the configured limit.
    #[error("nesting depth exceeds {max} at offset {offset}")]
    DepthLimit {
        /// Configured maximum depth.
        max: usize,
        /// Offset of the offending container.
        offset: usize,
    },
    /// A byte cannot start a bencode value.
    #[error("invalid token 0x{byte:02x} at offset {offset}")]
    InvalidToken {
        /// Offending byte.
        byte: u8,
        /// Offset of the byte.
        offset: usize,
    },
    /// An `e` appeared where a value was expected.
    #[error("unexpected end marker at offset {offset}")]
    UnexpectedEndMarker {
        /// Offset of the `e`.
        offset: usize,
    },
    /// An integer is empty or contains an invalid character.
    #[error("invalid integer at offset {offset}")]
    InvalidInteger {
        /// Offset of the integer.
        offset: usize,
    },
    /// An integer has a leading zero other than `i0e`.
    #[error("integer has a leading zero at offset {offset}")]
    IntegerLeadingZero {
        /// Offset of the integer.
        offset: usize,
    },
    /// An integer is negative zero.
    #[error("negative zero integer at offset {offset}")]
    IntegerNegativeZero {
        /// Offset of the integer.
        offset: usize,
    },
    /// An integer does not fit in `i64`.
    #[error("integer exceeds i64 range at offset {offset}")]
    IntegerOverflow {
        /// Offset of the integer.
        offset: usize,
    },
    /// An integer literal is longer than the configured limit.
    #[error("integer literal exceeds {max} bytes at offset {offset}")]
    IntegerTooLong {
        /// Configured maximum length.
        max: usize,
        /// Offset of the integer.
        offset: usize,
    },
    /// A string length prefix is malformed.
    #[error("invalid string length at offset {offset}")]
    InvalidLength {
        /// Offset of the length prefix.
        offset: usize,
    },
    /// A byte string is larger than the configured limit.
    #[error("string length {len} exceeds {max} at offset {offset}")]
    StringTooLong {
        /// Declared string length.
        len: usize,
        /// Configured maximum.
        max: usize,
        /// Offset of the length prefix.
        offset: usize,
    },
    /// A dictionary key is not a byte string.
    #[error("dictionary key is not a byte string at offset {offset}")]
    NonStringDictKey {
        /// Offset of the key.
        offset: usize,
    },
    /// Dictionary keys are not strictly sorted.
    #[error("dictionary keys are not strictly sorted at offset {offset}")]
    UnsortedDictKeys {
        /// Offset of the first out-of-order key.
        offset: usize,
    },
    /// A dictionary contains the same key twice.
    #[error("duplicate dictionary key at offset {offset}")]
    DuplicateDictKey {
        /// Offset of the duplicate key.
        offset: usize,
    },
    /// A list or dictionary has more entries than the configured limit.
    #[error("container has more than {max} items at offset {offset}")]
    TooManyItems {
        /// Configured maximum.
        max: usize,
        /// Offset of the offending entry.
        offset: usize,
    },
    /// The decoded value tree has more nodes than the configured limit.
    #[error("value tree has more than {max} nodes at offset {offset}")]
    TooManyNodes {
        /// Configured maximum.
        max: usize,
        /// Offset of the offending value.
        offset: usize,
    },
    /// Encoded output would exceed the configured limit.
    #[error("encoded output exceeds {max} bytes")]
    OutputTooLarge {
        /// Configured maximum.
        max: usize,
    },
    /// A value tree is nested deeper than the configured limit.
    #[error("value tree is deeper than {max} containers")]
    EncodeDepthLimit {
        /// Configured maximum.
        max: usize,
    },
    /// A byte string is larger than the configured limit.
    #[error("string length {len} exceeds {max}")]
    EncodeStringTooLong {
        /// Actual string length.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// A list or dictionary has more entries than the configured limit.
    #[error("container has more than {max} items")]
    EncodeTooManyItems {
        /// Configured maximum.
        max: usize,
    },
}

/// Decodes one canonical bencode value with default limits and strict key
/// order.
pub fn decode(input: &[u8]) -> Result<Value<'_>, Error> {
    decode_with_options(input, DecodeOptions::default())
}

/// Decodes one canonical bencode value with custom limits and strict key
/// order.
pub fn decode_with_limits(input: &[u8], limits: Limits) -> Result<Value<'_>, Error> {
    decode_with_options(
        input,
        DecodeOptions {
            limits,
            key_order: KeyOrder::Strict,
        },
    )
}

/// Decodes one bencode value with explicit options.
pub fn decode_with_options(input: &[u8], options: DecodeOptions) -> Result<Value<'_>, Error> {
    let mut decoder = Decoder::new(input, options)?;
    let value = decoder.parse_value(0)?;
    if decoder.pos != input.len() {
        return Err(Error::TrailingData {
            offset: decoder.pos,
        });
    }
    Ok(value)
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
    limits: Limits,
    key_order: KeyOrder,
    nodes: usize,
}

impl<'a> Decoder<'a> {
    fn new(input: &'a [u8], options: DecodeOptions) -> Result<Self, Error> {
        if input.len() > options.limits.max_input_bytes {
            return Err(Error::InputTooLarge {
                len: input.len(),
                max: options.limits.max_input_bytes,
            });
        }
        Ok(Self {
            input,
            pos: 0,
            limits: options.limits,
            key_order: options.key_order,
            nodes: 0,
        })
    }

    fn parse_value(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        let start = self.pos;
        self.bump_node(start)?;

        let kind = match self.peek()? {
            b'i' => self.parse_integer()?,
            b'0'..=b'9' => Kind::Bytes(self.parse_string_bytes()?),
            b'l' => {
                self.check_depth(depth, start)?;
                self.parse_list(depth)?
            }
            b'd' => {
                self.check_depth(depth, start)?;
                self.parse_dict(depth)?
            }
            b'e' => {
                return Err(Error::UnexpectedEndMarker { offset: start });
            }
            byte => {
                return Err(Error::InvalidToken {
                    byte,
                    offset: start,
                });
            }
        };

        Ok(Value {
            kind,
            span: start..self.pos,
        })
    }

    fn check_depth(&self, depth: usize, offset: usize) -> Result<(), Error> {
        if depth >= self.limits.max_depth {
            return Err(Error::DepthLimit {
                max: self.limits.max_depth,
                offset,
            });
        }
        Ok(())
    }

    fn bump_node(&mut self, offset: usize) -> Result<(), Error> {
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > self.limits.max_nodes {
            return Err(Error::TooManyNodes {
                max: self.limits.max_nodes,
                offset,
            });
        }
        Ok(())
    }

    fn peek(&self) -> Result<u8, Error> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or(Error::Truncated { offset: self.pos })
    }

    fn parse_integer(&mut self) -> Result<Kind<'a>, Error> {
        let start = self.pos;
        self.pos += 1;

        let digits_start = self.pos;
        let mut end = digits_start;
        loop {
            match self.input.get(end) {
                Some(b'e') => break,
                Some(_) => {
                    end += 1;
                    if end - digits_start > self.limits.max_int_bytes {
                        return Err(Error::IntegerTooLong {
                            max: self.limits.max_int_bytes,
                            offset: start,
                        });
                    }
                }
                None => return Err(Error::Truncated { offset: self.pos }),
            }
        }

        let bytes = &self.input[digits_start..end];
        if bytes.is_empty() {
            return Err(Error::InvalidInteger { offset: start });
        }

        let (negative, digits) = if bytes[0] == b'-' {
            (true, &bytes[1..])
        } else {
            (false, bytes)
        };
        if digits.is_empty() {
            return Err(Error::InvalidInteger { offset: start });
        }
        if digits.len() > 1 && digits[0] == b'0' {
            return Err(Error::IntegerLeadingZero { offset: start });
        }
        if negative && digits == b"0" {
            return Err(Error::IntegerNegativeZero { offset: start });
        }
        if !digits.iter().all(u8::is_ascii_digit) {
            return Err(Error::InvalidInteger { offset: start });
        }

        let text =
            std::str::from_utf8(bytes).map_err(|_| Error::InvalidInteger { offset: start })?;
        let value = text
            .parse::<i64>()
            .map_err(|_| Error::IntegerOverflow { offset: start })?;

        self.pos = end + 1;
        Ok(Kind::Int(value))
    }

    fn parse_string_bytes(&mut self) -> Result<&'a [u8], Error> {
        let start = self.pos;
        let mut end = start;
        let mut digits = 0;
        let mut len = 0usize;

        loop {
            match self.input.get(end) {
                Some(b':') => break,
                Some(&byte) if byte.is_ascii_digit() => {
                    digits += 1;
                    if digits > MAX_LENGTH_DIGITS {
                        return Err(Error::InvalidLength { offset: start });
                    }
                    len = len
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(usize::from(byte - b'0')))
                        .ok_or(Error::InvalidLength { offset: start })?;
                    end += 1;
                }
                Some(_) => return Err(Error::InvalidLength { offset: start }),
                None => return Err(Error::Truncated { offset: start }),
            }
        }

        if digits == 0 {
            return Err(Error::InvalidLength { offset: start });
        }
        if digits > 1 && self.input[start] == b'0' {
            return Err(Error::InvalidLength { offset: start });
        }
        if len > self.limits.max_string_bytes {
            return Err(Error::StringTooLong {
                len,
                max: self.limits.max_string_bytes,
                offset: start,
            });
        }

        let data_start = end + 1;
        let data_end = data_start
            .checked_add(len)
            .ok_or(Error::InvalidLength { offset: start })?;
        if data_end > self.input.len() {
            return Err(Error::Truncated { offset: data_start });
        }

        self.pos = data_end;
        Ok(&self.input[data_start..data_end])
    }

    fn parse_list(&mut self, depth: usize) -> Result<Kind<'a>, Error> {
        self.pos += 1;
        let mut items = Vec::new();

        loop {
            if self.peek()? == b'e' {
                self.pos += 1;
                break;
            }
            if items.len() >= self.limits.max_container_items {
                return Err(Error::TooManyItems {
                    max: self.limits.max_container_items,
                    offset: self.pos,
                });
            }
            items.push(self.parse_value(depth + 1)?);
        }

        Ok(Kind::List(items))
    }

    fn parse_dict(&mut self, depth: usize) -> Result<Kind<'a>, Error> {
        self.pos += 1;
        let mut entries = BTreeMap::new();
        let mut last_key: Option<&'a [u8]> = None;
        let mut seen = match self.key_order {
            KeyOrder::Strict => None,
            KeyOrder::Lenient => Some(BTreeSet::new()),
        };

        loop {
            if self.peek()? == b'e' {
                self.pos += 1;
                break;
            }
            if entries.len() >= self.limits.max_container_items {
                return Err(Error::TooManyItems {
                    max: self.limits.max_container_items,
                    offset: self.pos,
                });
            }

            let key_start = self.pos;
            self.bump_node(key_start)?;
            let key = match self.peek()? {
                b'0'..=b'9' => self.parse_string_bytes()?,
                _ => {
                    return Err(Error::NonStringDictKey { offset: key_start });
                }
            };

            match self.key_order {
                KeyOrder::Strict => {
                    if let Some(previous) = last_key {
                        match key.cmp(previous) {
                            std::cmp::Ordering::Less => {
                                return Err(Error::UnsortedDictKeys { offset: key_start });
                            }
                            std::cmp::Ordering::Equal => {
                                return Err(Error::DuplicateDictKey { offset: key_start });
                            }
                            std::cmp::Ordering::Greater => {}
                        }
                    }
                    last_key = Some(key);
                }
                KeyOrder::Lenient => {
                    if let Some(seen) = seen.as_mut()
                        && !seen.insert(key)
                    {
                        return Err(Error::DuplicateDictKey { offset: key_start });
                    }
                }
            }

            let value = self.parse_value(depth + 1)?;
            entries.insert(key, value);
        }

        Ok(Kind::Dict(entries))
    }
}

/// Encodes a value using canonical bencode rules and default limits.
pub fn encode(value: &Value<'_>) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    encode_into(value, &mut out)?;
    Ok(out)
}

/// Appends a canonically encoded value to `out` using default limits.
///
/// On error, `out` is restored to its original length.
pub fn encode_into(value: &Value<'_>, out: &mut Vec<u8>) -> Result<(), Error> {
    encode_with_limits(value, out, Limits::default())
}

fn encode_with_limits(value: &Value<'_>, out: &mut Vec<u8>, limits: Limits) -> Result<(), Error> {
    let start = out.len();
    let result = {
        let mut encoder = Encoder { out, limits };
        encoder.encode_value(value, 0)
    };
    if result.is_err() {
        out.truncate(start);
    }
    result
}

struct Encoder<'a> {
    out: &'a mut Vec<u8>,
    limits: Limits,
}

impl Encoder<'_> {
    fn encode_value(&mut self, value: &Value<'_>, depth: usize) -> Result<(), Error> {
        match &value.kind {
            Kind::Int(number) => {
                self.push(b"i")?;
                self.push(number.to_string().as_bytes())?;
                self.push(b"e")
            }
            Kind::Bytes(bytes) => self.encode_bytes(bytes),
            Kind::List(items) => {
                self.check_depth(depth)?;
                self.check_items(items.len())?;
                self.push(b"l")?;
                for item in items {
                    self.encode_value(item, depth + 1)?;
                }
                self.push(b"e")
            }
            Kind::Dict(entries) => {
                self.check_depth(depth)?;
                self.check_items(entries.len())?;
                self.push(b"d")?;
                for (key, value) in entries {
                    self.encode_bytes(key)?;
                    self.encode_value(value, depth + 1)?;
                }
                self.push(b"e")
            }
        }
    }

    fn check_depth(&self, depth: usize) -> Result<(), Error> {
        if depth >= self.limits.max_depth {
            return Err(Error::EncodeDepthLimit {
                max: self.limits.max_depth,
            });
        }
        Ok(())
    }

    fn check_items(&self, len: usize) -> Result<(), Error> {
        if len > self.limits.max_container_items {
            return Err(Error::EncodeTooManyItems {
                max: self.limits.max_container_items,
            });
        }
        Ok(())
    }

    fn encode_bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() > self.limits.max_string_bytes {
            return Err(Error::EncodeStringTooLong {
                len: bytes.len(),
                max: self.limits.max_string_bytes,
            });
        }

        let mut buffer = [0u8; MAX_LENGTH_DIGITS];
        let mut number = bytes.len();
        let mut index = buffer.len();
        loop {
            index -= 1;
            buffer[index] = b'0' + (number % 10) as u8;
            number /= 10;
            if number == 0 {
                break;
            }
        }

        self.push(&buffer[index..])?;
        self.push(b":")?;
        self.push(bytes)
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let new_len = self
            .out
            .len()
            .checked_add(bytes.len())
            .ok_or(Error::OutputTooLarge {
                max: self.limits.max_input_bytes,
            })?;
        if new_len > self.limits.max_input_bytes {
            return Err(Error::OutputTooLarge {
                max: self.limits.max_input_bytes,
            });
        }
        self.out.extend_from_slice(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
