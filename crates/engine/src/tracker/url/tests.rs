use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::tracker::TrackerError;

use super::{Host, MAX_REDACTED_URL_LEN, MAX_URL_LEN, Scheme, TrackerUrl};

fn parse(input: &str) -> TrackerUrl {
    TrackerUrl::parse(input).expect("valid tracker URL")
}

fn assert_invalid(input: &str) {
    match TrackerUrl::parse(input) {
        Err(TrackerError::InvalidUrl) => {}
        Err(other) => panic!("expected InvalidUrl for {input:?}, got {other:?}"),
        Ok(_) => panic!("expected InvalidUrl for {input:?}, got a parsed URL"),
    }
}

fn assert_unsupported(input: &str) {
    match TrackerUrl::parse(input) {
        Err(TrackerError::UnsupportedScheme) => {}
        Err(other) => panic!("expected UnsupportedScheme for {input:?}, got {other:?}"),
        Ok(_) => panic!("expected UnsupportedScheme for {input:?}, got a parsed URL"),
    }
}

#[test]
fn parses_http_url_with_query_and_fragment() {
    let url = parse("http://tracker.example.com/announce?passkey=abc#frag");
    assert_eq!(url.scheme(), Scheme::Http);
    assert_eq!(url.host(), &Host::Name("tracker.example.com".into()));
    assert_eq!(url.port(), 80);
    assert_eq!(url.path(), "/announce");
    assert_eq!(url.query(), Some("passkey=abc"));
    assert_eq!(url.fragment(), Some("frag"));
    assert_eq!(url.request_target(), "/announce?passkey=abc");
}

#[test]
fn parses_https_url_with_non_default_port() {
    let url = parse("https://tracker.example.com:8443/a/b?x=1#frag");
    assert_eq!(url.scheme(), Scheme::Https);
    assert_eq!(url.host(), &Host::Name("tracker.example.com".into()));
    assert_eq!(url.port(), 8443);
    assert_eq!(url.path(), "/a/b");
    assert_eq!(url.query(), Some("x=1"));
    assert_eq!(url.fragment(), Some("frag"));
    assert_eq!(url.request_target(), "/a/b?x=1");
}

#[test]
fn parses_udp_url_and_requires_a_port() {
    let url = parse("udp://tracker.example.com:6969");
    assert_eq!(url.scheme(), Scheme::Udp);
    assert_eq!(url.host(), &Host::Name("tracker.example.com".into()));
    assert_eq!(url.port(), 6969);
    assert_eq!(url.path(), "/");
    assert_eq!(url.query(), None);
    assert_eq!(url.fragment(), None);

    assert_invalid("udp://tracker.example.com");
    assert_invalid("udp://tracker.example.com/");
    assert_eq!(parse("udp://[::1]:6969").port(), 6969);
}

#[test]
fn schemes_are_case_insensitive_and_restricted() {
    assert_eq!(parse("HTTP://example.com/").scheme(), Scheme::Http);
    assert_eq!(parse("HtTpS://example.com/").scheme(), Scheme::Https);
    assert_eq!(parse("UDP://example.com:6969/").scheme(), Scheme::Udp);

    for input in [
        "ftp://example.com/",
        "file://example.com/",
        "ws://example.com/",
    ] {
        assert_unsupported(input);
    }

    for input in [
        "example.com",
        "http:/example.com",
        "http//example.com/",
        "http://",
        "http:///path",
        "://example.com",
        "1http://example.com/",
    ] {
        assert_invalid(input);
    }
}

#[test]
fn canonical_ipv4_hosts_are_literals() {
    for (input, expected) in [
        ("http://127.0.0.1/", Ipv4Addr::LOCALHOST),
        ("http://0.0.0.0/", Ipv4Addr::UNSPECIFIED),
        ("http://255.255.255.255/", Ipv4Addr::BROADCAST),
    ] {
        let url = parse(input);
        assert_eq!(url.host().canonical_ip(), Some(IpAddr::V4(expected)));
        assert_eq!(url.port(), 80);
    }
}

#[test]
fn ipv4_literal_bypass_forms_are_rejected() {
    for host in [
        "127.1",
        "0",
        "2130706433",
        "0177.0.0.1",
        "08",
        "1.2.3.04",
        "1.2.3",
        "1.2.3.4.5",
        "1.2.3.256",
        "127.0.0.1.",
    ] {
        assert_invalid(&format!("http://{host}/"));
    }
}

#[test]
fn hex_host_forms_are_rejected() {
    for host in [
        "0x7f.0.0.1",
        "0X7F.0.0.1",
        "0x7f000001",
        "0X7F000001",
        "0x7f.0.0.1.",
        "1.2.3.0x4",
    ] {
        assert_invalid(&format!("http://{host}/"));
    }

    assert_invalid("http://0x7f.0.0.1:80/");
}

#[test]
fn ipv6_hosts_are_bracketed_and_canonicalized() {
    let url = parse("http://[0:0:0:0:0:0:0:1]:8080/announce");
    assert_eq!(url.host(), &Host::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    assert_eq!(url.port(), 8080);

    let default = parse("http://[::1]/");
    assert_eq!(default.port(), 80);
    assert_eq!(default.path(), "/");

    let explicit = parse("http://[::1]:80/");
    assert!(default == explicit);
}

#[test]
fn canonical_ip_maps_ipv4_mapped_ipv6() {
    let mapped = parse("http://[::ffff:127.0.0.1]/");
    assert_eq!(
        mapped.host().canonical_ip(),
        Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
    );

    let plain = Host::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
    assert_eq!(plain.canonical_ip(), Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));

    let name = Host::Name("example.com".into());
    assert_eq!(name.canonical_ip(), None);

    let nat64: Ipv6Addr = "64:ff9b::7f00:1".parse().expect("nat64 address");
    let transition = Host::Ip(IpAddr::V6(nat64));
    assert_eq!(transition.canonical_ip(), Some(IpAddr::V6(nat64)));
}

#[test]
fn malformed_ipv6_and_ports_are_rejected() {
    for input in [
        "http://[::1]:0/",
        "http://[::1]:65536/",
        "http://[::1/",
        "http://::1]/",
        "http://[::1]x/",
        "http://[]/",
        "http://[127.0.0.1]/",
        "http://[::1]:/",
        "http://[::1]:abc/",
        "http://[::1]:-1/",
        "http://[::1]:+80/",
        "http://[::1]:80:90/",
        "http://[::1%25eth0]/",
    ] {
        assert_invalid(input);
    }
}

#[test]
fn ports_are_validated() {
    assert_eq!(parse("http://example.com:65535/").port(), 65535);

    for input in [
        "http://example.com:0/",
        "http://example.com:65536/",
        "http://example.com:/",
        "http://example.com:abc/",
        "http://example.com:-1/",
        "http://example.com:+80/",
        "http://example.com:80:90/",
    ] {
        assert_invalid(input);
    }
}

#[test]
fn dns_names_are_lowercased_and_validated() {
    let url = parse("HTTP://Tracker.Example.COM./Announce");
    assert_eq!(url.scheme(), Scheme::Http);
    assert_eq!(url.host(), &Host::Name("tracker.example.com".into()));
    assert_eq!(url.path(), "/Announce");

    for input in [
        "http://example..com/",
        "http://.example.com/",
        "http://example.com../",
        "http://-example.com/",
        "http://example-.com/",
        "http://exa_mple.com/",
        "http://exa%6dple.com/",
        "http://exämple.com/",
    ] {
        assert_invalid(input);
    }
}

#[test]
fn host_labels_and_total_length_are_capped() {
    let label63 = "a".repeat(63);
    let host253 = format!("{label63}.{label63}.{label63}.{}", "a".repeat(61));
    assert_eq!(host253.len(), 253);
    let url = parse(&format!("http://{host253}/"));
    assert_eq!(url.host(), &Host::Name(host253));

    let label64 = "a".repeat(64);
    assert_invalid(&format!("http://{label64}.com/"));

    let host254 = format!("{label63}.{label63}.{label63}.{}", "a".repeat(62));
    assert_eq!(host254.len(), 254);
    assert_invalid(&format!("http://{host254}/"));
}

#[test]
fn path_query_and_fragment_are_ascii_and_percent_validated() {
    let url = parse("http://example.com/a+b%20c?x=%2f%2F&y=a+b#f%2F");
    assert_eq!(url.path(), "/a+b%20c");
    assert_eq!(url.query(), Some("x=%2f%2F&y=a+b"));
    assert_eq!(url.fragment(), Some("f%2F"));
    assert_eq!(url.request_target(), "/a+b%20c?x=%2f%2F&y=a+b");

    for input in [
        "http://example.com/a b",
        "http://example.com/a\tb",
        "http://example.com/a\\b",
        "http://example.com/é",
        "http://example.com/?q=é",
        "http://example.com/#é",
        "http://example.com/%",
        "http://example.com/%2",
        "http://example.com/%zz",
        "http://example.com/?x=%",
        "http://example.com/#%",
        "http://example.com/\u{7f}",
        "http://example.com/\u{0}",
    ] {
        assert_invalid(input);
    }
}

#[test]
fn empty_path_defaults_to_slash() {
    let url = parse("http://example.com");
    assert_eq!(url.path(), "/");
    assert_eq!(url.query(), None);
    assert_eq!(url.fragment(), None);
    assert_eq!(url.request_target(), "/");

    let query_only = parse("http://example.com?x=1");
    assert_eq!(query_only.path(), "/");
    assert_eq!(query_only.query(), Some("x=1"));
    assert_eq!(query_only.request_target(), "/?x=1");

    let fragment_only = parse("http://example.com#frag");
    assert_eq!(fragment_only.path(), "/");
    assert_eq!(fragment_only.fragment(), Some("frag"));
    assert_eq!(fragment_only.request_target(), "/");
}

#[test]
fn request_target_excludes_fragment_and_keeps_raw_bytes() {
    let url = parse("http://example.com/a?b=%2f#c");
    assert_eq!(url.request_target(), "/a?b=%2f");
    assert_eq!(url.path(), "/a");
    assert_eq!(url.query(), Some("b=%2f"));
    assert_eq!(url.fragment(), Some("c"));
}

#[test]
fn url_length_is_capped_at_2048_bytes() {
    let base = "http://example.com/";
    let mut url = String::from(base);
    url.push_str(&"a".repeat(MAX_URL_LEN - base.len()));
    assert_eq!(url.len(), MAX_URL_LEN);
    assert!(TrackerUrl::parse(&url).is_ok());

    url.push('a');
    assert_eq!(url.len(), MAX_URL_LEN + 1);
    assert_invalid(&url);
}

#[test]
fn normalized_urls_compare_and_hash_equally() {
    let mut set = HashSet::new();
    set.insert(parse("http://EXAMPLE.com:80/a?x=1#f"));
    assert!(set.contains(&parse("http://example.com/a?x=1#f")));

    let mut ipv6 = HashSet::new();
    ipv6.insert(parse("http://[0:0:0:0:0:0:0:1]/"));
    assert!(ipv6.contains(&parse("http://[::1]/")));

    let mut trailing_dot = HashSet::new();
    trailing_dot.insert(parse("http://example.com./"));
    assert!(trailing_dot.contains(&parse("http://example.com/")));

    assert!(parse("http://example.com/a") != parse("https://example.com/a"));
    assert!(parse("http://example.com/a") != parse("http://example.com:8080/a"));
    assert!(parse("http://example.com/a?x=1") != parse("http://example.com/a?x=2"));
    assert!(parse("http://example.com/a#f") != parse("http://example.com/a"));
}

#[test]
fn userinfo_is_rejected() {
    for input in [
        "http://user:pass@example.com/",
        "http://user@example.com/",
        "http://example.com:80@evil.com/",
    ] {
        assert_invalid(input);
    }
}

#[test]
fn host_percent_escapes_are_rejected() {
    for input in [
        "http://exa%6dple.com/",
        "http://%31%32%37.0.0.1/",
        "http://[::1%25eth0]/",
    ] {
        assert_invalid(input);
    }
}

#[test]
fn host_display_brackets_ipv6_only() {
    assert_eq!(
        Host::Name("tracker.example".into()).to_string(),
        "tracker.example"
    );
    assert_eq!(
        Host::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST)).to_string(),
        "127.0.0.1"
    );
    assert_eq!(
        Host::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST)).to_string(),
        "[::1]"
    );
}

#[test]
fn redaction_follows_the_documented_table() {
    for (input, expected) in [
        (
            "https://tracker.example/announce?passkey=SECRET",
            "https://tracker.example/…",
        ),
        (
            "http://tracker.example:8080/announce",
            "http://tracker.example:8080/…",
        ),
        ("udp://tracker.example:6969", "udp://tracker.example:6969"),
        ("http://tracker.example/", "http://tracker.example/"),
        ("http://tracker.example", "http://tracker.example/"),
        (
            "http://tracker.example:8080/",
            "http://tracker.example:8080/",
        ),
        ("http://tracker.example?x=1", "http://tracker.example/…"),
        ("http://tracker.example#frag", "http://tracker.example/…"),
        (
            "HTTPS://Tracker.Example/Announce",
            "https://tracker.example/…",
        ),
        ("http://[::1]/announce", "http://[::1]/…"),
        ("http://[::1]/", "http://[::1]/"),
        ("https://[::1]:8443/", "https://[::1]:8443/"),
        ("udp://[::1]:6969/announce", "udp://[::1]:6969"),
        ("http://127.0.0.1:80/announce", "http://127.0.0.1/…"),
    ] {
        let url = parse(input);
        assert_eq!(url.to_string(), expected, "Display for {input:?}");
        assert_eq!(format!("{url:?}"), format!("TrackerUrl({expected})"));
    }
}

#[test]
fn redaction_hides_secrets_in_every_component() {
    let cases = [
        (
            "https://tracker.example/announce/SECRET/path",
            "SECRET",
            "https://tracker.example/…",
        ),
        (
            "https://tracker.example/announce?passkey=SECRET",
            "SECRET",
            "https://tracker.example/…",
        ),
        (
            "https://tracker.example/announce?x=1#SECRET",
            "SECRET",
            "https://tracker.example/…",
        ),
        (
            "udp://tracker.example:6969/announce/SECRET",
            "SECRET",
            "udp://tracker.example:6969",
        ),
    ];

    for (input, secret, expected) in cases {
        let url = parse(input);
        let display = url.to_string();
        let debug = format!("{url:?}");
        assert_eq!(display, expected);
        assert_eq!(debug, format!("TrackerUrl({expected})"));
        assert!(!display.contains(secret));
        assert!(!debug.contains(secret));
        assert!(!display.contains('?'));
        assert!(!display.contains('#'));
        assert!(display.chars().all(|character| !character.is_control()));
    }

    let userinfo = TrackerUrl::parse("http://user:SECRET@tracker.example/announce");
    let error = userinfo.expect_err("userinfo is rejected");
    assert_eq!(error.to_string(), "<invalid tracker url>");
    assert!(!format!("{error:?}").contains("SECRET"));
    assert!(!error.to_string().contains("SECRET"));
}

#[test]
fn redacted_output_is_bounded_at_271_bytes() {
    let label = "a".repeat(63);
    let host = format!("{label}.{label}.{label}.{}", "a".repeat(61));
    assert_eq!(host.len(), 253);

    let prefix = format!("https://{host}:65535/");
    let url = format!("{prefix}{}", "a".repeat(MAX_URL_LEN - prefix.len()));
    assert_eq!(url.len(), MAX_URL_LEN);

    let redacted = parse(&url).to_string();
    assert_eq!(redacted, format!("https://{host}:65535/…"));
    assert_eq!(redacted.len(), MAX_REDACTED_URL_LEN);
    assert_eq!(MAX_REDACTED_URL_LEN, 271);

    let oversized = format!("{url}{}", "a".repeat(2048));
    assert_eq!(oversized.len(), 4096);
    assert_invalid(&oversized);
}
