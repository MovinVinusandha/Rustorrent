//! Strict tracker URL parsing.
//!
//! The parser is deliberately smaller than a general URL crate: it accepts
//! only `http`, `https` and `udp` URLs, rejects userinfo and non-ASCII input,
//! and classifies every numeric-looking host before a resolver can see it.
//!
//! The original string is not stored. The scheme, host and port are
//! normalized, while the path, query and fragment are kept byte for byte
//! after validation.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::TrackerError;

/// Maximum accepted tracker URL length in bytes.
pub const MAX_URL_LEN: usize = 2048;

/// Maximum accepted DNS host length in bytes, excluding a trailing dot.
pub const MAX_HOST_LEN: usize = 253;

/// Maximum accepted DNS label length in bytes.
pub const MAX_LABEL_LEN: usize = 63;

/// Tracker URL scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// Plain HTTP.
    Http,
    /// HTTP over TLS.
    Https,
    /// The UDP tracker protocol.
    Udp,
}

impl Scheme {
    /// Returns the scheme's default port, or `None` when a port is required.
    pub const fn default_port(self) -> Option<u16> {
        match self {
            Self::Http => Some(80),
            Self::Https => Some(443),
            Self::Udp => None,
        }
    }

    /// Returns the lowercase scheme name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Udp => "udp",
        }
    }
}

/// A tracker host: a validated DNS name or an IP literal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    /// A lowercased DNS name with any trailing dot removed.
    Name(String),
    /// An IPv4 or IPv6 literal.
    Ip(IpAddr),
}

impl Host {
    /// Returns the IP literal, or `None` for a DNS name.
    ///
    /// An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) yields the embedded
    /// IPv4 address, so the target policy has one place to check. Any other
    /// IPv6 address is returned unchanged.
    pub fn canonical_ip(&self) -> Option<IpAddr> {
        match self {
            Self::Name(_) => None,
            Self::Ip(IpAddr::V4(address)) => Some(IpAddr::V4(*address)),
            Self::Ip(IpAddr::V6(address)) => Some(match address.to_ipv4_mapped() {
                Some(mapped) => IpAddr::V4(mapped),
                None => IpAddr::V6(*address),
            }),
        }
    }
}

/// A parsed tracker URL.
///
/// The scheme, host and port are normalized; the path, query and fragment are
/// kept byte for byte after validation. The original string is not stored.
/// Equality and hashing use the normalized components only.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TrackerUrl {
    scheme: Scheme,
    host: Host,
    port: u16,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}

impl TrackerUrl {
    /// Parses a tracker URL.
    ///
    /// Returns [`TrackerError::InvalidUrl`] for malformed input and
    /// [`TrackerError::UnsupportedScheme`] for a well-formed but unsupported
    /// scheme.
    pub fn parse(input: &str) -> Result<Self, TrackerError> {
        if input.len() > MAX_URL_LEN || !input.is_ascii() {
            return Err(TrackerError::InvalidUrl);
        }
        if input.bytes().any(is_forbidden_byte) {
            return Err(TrackerError::InvalidUrl);
        }

        let (scheme_text, rest) = input.split_once("://").ok_or(TrackerError::InvalidUrl)?;
        let scheme = parse_scheme(scheme_text)?;

        let (authority, tail) = split_authority(rest);
        if authority.is_empty() || authority.contains('@') {
            return Err(TrackerError::InvalidUrl);
        }

        let (host, port) = parse_authority(authority, scheme)?;
        let (path, query, fragment) = parse_tail(tail)?;

        Ok(Self {
            scheme,
            host,
            port,
            path,
            query,
            fragment,
        })
    }

    /// Returns the normalized scheme.
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Returns the normalized host.
    pub const fn host(&self) -> &Host {
        &self.host
    }

    /// Returns the effective port, including the scheme default.
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Returns the validated path, which always starts with `/`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the raw query without the leading `?`.
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// Returns the raw fragment without the leading `#`.
    pub fn fragment(&self) -> Option<&str> {
        self.fragment.as_deref()
    }

    /// Returns the HTTP request target: the path and query, never the
    /// fragment.
    pub fn request_target(&self) -> String {
        let query_len = self.query.as_ref().map_or(0, |query| query.len() + 1);
        let mut target = String::with_capacity(self.path.len() + query_len);
        target.push_str(&self.path);
        if let Some(query) = &self.query {
            target.push('?');
            target.push_str(query);
        }
        target
    }
}

fn is_forbidden_byte(byte: u8) -> bool {
    byte <= b' ' || byte == 0x7f || byte == b'\\'
}

fn split_authority(rest: &str) -> (&str, &str) {
    match rest.find(['/', '?', '#']) {
        Some(index) => {
            let authority = rest.get(..index).unwrap_or_default();
            let tail = rest.get(index..).unwrap_or_default();
            (authority, tail)
        }
        None => (rest, ""),
    }
}

fn parse_scheme(text: &str) -> Result<Scheme, TrackerError> {
    let mut bytes = text.bytes();
    let Some(first) = bytes.next() else {
        return Err(TrackerError::InvalidUrl);
    };
    if !first.is_ascii_alphabetic() {
        return Err(TrackerError::InvalidUrl);
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')) {
        return Err(TrackerError::InvalidUrl);
    }

    if text.eq_ignore_ascii_case("http") {
        Ok(Scheme::Http)
    } else if text.eq_ignore_ascii_case("https") {
        Ok(Scheme::Https)
    } else if text.eq_ignore_ascii_case("udp") {
        Ok(Scheme::Udp)
    } else {
        Err(TrackerError::UnsupportedScheme)
    }
}

fn parse_authority(authority: &str, scheme: Scheme) -> Result<(Host, u16), TrackerError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (inside, after) = rest.split_once(']').ok_or(TrackerError::InvalidUrl)?;
        if inside.is_empty() || inside.contains('%') {
            return Err(TrackerError::InvalidUrl);
        }
        let address: Ipv6Addr = inside.parse().map_err(|_| TrackerError::InvalidUrl)?;
        let port_text = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or(TrackerError::InvalidUrl)?),
        };
        let port = effective_port(port_text, scheme)?;
        return Ok((Host::Ip(IpAddr::V6(address)), port));
    }

    let (host_text, port_text) = match authority.split_once(':') {
        Some((host, port)) => {
            if port.contains(':') {
                return Err(TrackerError::InvalidUrl);
            }
            (host, Some(port))
        }
        None => (authority, None),
    };
    if host_text.is_empty() {
        return Err(TrackerError::InvalidUrl);
    }

    let host = parse_host(&host_text.to_ascii_lowercase())?;
    let port = effective_port(port_text, scheme)?;
    Ok((host, port))
}

fn effective_port(text: Option<&str>, scheme: Scheme) -> Result<u16, TrackerError> {
    match text {
        Some(text) => parse_port(text),
        None => scheme.default_port().ok_or(TrackerError::InvalidUrl),
    }
}

fn parse_port(text: &str) -> Result<u16, TrackerError> {
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TrackerError::InvalidUrl);
    }
    let port: u16 = text.parse().map_err(|_| TrackerError::InvalidUrl)?;
    if port == 0 {
        return Err(TrackerError::InvalidUrl);
    }
    Ok(port)
}

fn parse_host(text: &str) -> Result<Host, TrackerError> {
    if text.contains('%') {
        return Err(TrackerError::InvalidUrl);
    }

    let name = text.strip_suffix('.').unwrap_or(text);
    if name.is_empty() {
        return Err(TrackerError::InvalidUrl);
    }

    if name
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        if name.len() != text.len() {
            return Err(TrackerError::InvalidUrl);
        }
        return parse_ipv4(name).map(|address| Host::Ip(IpAddr::V4(address)));
    }

    if looks_like_inet_aton(name) {
        return Err(TrackerError::InvalidUrl);
    }

    parse_name(name).map(Host::Name)
}

fn parse_ipv4(text: &str) -> Result<Ipv4Addr, TrackerError> {
    let mut parts = text.split('.');
    let mut octets = [0u8; 4];
    for octet in &mut octets {
        let part = parts.next().ok_or(TrackerError::InvalidUrl)?;
        *octet = parse_ipv4_part(part)?;
    }
    if parts.next().is_some() {
        return Err(TrackerError::InvalidUrl);
    }
    Ok(Ipv4Addr::from(octets))
}

fn parse_ipv4_part(part: &str) -> Result<u8, TrackerError> {
    if part.is_empty() || part.len() > 3 {
        return Err(TrackerError::InvalidUrl);
    }
    if part.len() > 1 && part.starts_with('0') {
        return Err(TrackerError::InvalidUrl);
    }
    if !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TrackerError::InvalidUrl);
    }
    part.parse().map_err(|_| TrackerError::InvalidUrl)
}

fn looks_like_inet_aton(text: &str) -> bool {
    let mut parts = 0;
    for part in text.split('.') {
        parts += 1;
        if parts > 4 || !is_numeric_part(part) {
            return false;
        }
    }
    parts != 0
}

fn is_numeric_part(part: &str) -> bool {
    if let Some(hex) = part.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

fn parse_name(name: &str) -> Result<String, TrackerError> {
    if name.is_empty() || name.len() > MAX_HOST_LEN || name.ends_with('.') {
        return Err(TrackerError::InvalidUrl);
    }

    for label in name.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(TrackerError::InvalidUrl);
        }
        let Some((first, rest)) = label.as_bytes().split_first() else {
            return Err(TrackerError::InvalidUrl);
        };
        if !first.is_ascii_alphanumeric() {
            return Err(TrackerError::InvalidUrl);
        }
        if !rest
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return Err(TrackerError::InvalidUrl);
        }
        if label.ends_with('-') {
            return Err(TrackerError::InvalidUrl);
        }
    }

    Ok(name.to_string())
}

fn parse_tail(tail: &str) -> Result<(String, Option<String>, Option<String>), TrackerError> {
    let (before_fragment, fragment) = match tail.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (tail, None),
    };
    let (path, query) = match before_fragment.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (before_fragment, None),
    };

    let path = if path.is_empty() { "/" } else { path };
    if !path.starts_with('/') {
        return Err(TrackerError::InvalidUrl);
    }

    validate_component(path)?;
    if let Some(query) = query {
        validate_component(query)?;
    }
    if let Some(fragment) = fragment {
        validate_component(fragment)?;
    }

    Ok((
        path.to_string(),
        query.map(|value| value.to_string()),
        fragment.map(|value| value.to_string()),
    ))
}

fn validate_component(text: &str) -> Result<(), TrackerError> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let high = bytes
                .get(index + 1)
                .copied()
                .ok_or(TrackerError::InvalidUrl)?;
            let low = bytes
                .get(index + 2)
                .copied()
                .ok_or(TrackerError::InvalidUrl)?;
            if !high.is_ascii_hexdigit() || !low.is_ascii_hexdigit() {
                return Err(TrackerError::InvalidUrl);
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
