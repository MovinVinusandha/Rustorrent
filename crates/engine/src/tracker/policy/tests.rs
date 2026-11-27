use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::tracker::{TrackerError, TrackerUrl};

use super::TargetPolicy;

fn v4(text: &str) -> IpAddr {
    IpAddr::V4(text.parse().expect("valid IPv4 literal"))
}

fn v6(text: &str) -> IpAddr {
    IpAddr::V6(text.parse().expect("valid IPv6 literal"))
}

fn check(allow_private: bool, address: IpAddr) -> Result<(), TrackerError> {
    TargetPolicy::new(allow_private).check(address)
}

#[test]
fn always_blocked_ipv4_ranges_are_rejected() {
    for address in [
        "0.0.0.1",
        "0.255.255.255",
        "224.0.0.1",
        "239.255.255.255",
        "255.255.255.255",
        "192.0.0.1",
        "192.0.0.255",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "198.18.0.1",
        "198.19.255.255",
        "240.0.0.1",
        "250.1.2.3",
    ] {
        for allow_private in [false, true] {
            assert_eq!(
                check(allow_private, v4(address)),
                Err(TrackerError::BlockedTarget),
                "{address} with allow_private={allow_private}"
            );
        }
    }
}

#[test]
fn always_blocked_ipv6_ranges_are_rejected() {
    for address in [
        "::",
        "ff00::1",
        "::ffff:127.0.0.1",
        "2002::1",
        "64:ff9b::1",
        "2001::1",
        "64:ff9b:1::1",
        "2001:db8::1",
        "100::1",
    ] {
        for allow_private in [false, true] {
            assert_eq!(
                check(allow_private, v6(address)),
                Err(TrackerError::BlockedTarget),
                "{address} with allow_private={allow_private}"
            );
        }
    }
}

#[test]
fn private_ranges_require_the_flag() {
    let private = [
        "10.0.0.1",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.0.1",
        "100.64.0.1",
        "100.127.255.255",
        "127.0.0.1",
        "169.254.1.1",
    ];

    for address in private {
        assert_eq!(
            check(false, v4(address)),
            Err(TrackerError::BlockedTarget),
            "{address} without the flag"
        );
        assert_eq!(check(true, v4(address)), Ok(()), "{address} with the flag");
    }

    for address in ["::1", "fe80::1", "fc00::1", "fdff::1"] {
        assert_eq!(
            check(false, v6(address)),
            Err(TrackerError::BlockedTarget),
            "{address} without the flag"
        );
        assert_eq!(check(true, v6(address)), Ok(()), "{address} with the flag");
    }
}

#[test]
fn public_addresses_are_allowed() {
    for address in [
        "1.1.1.1",
        "8.8.8.8",
        "93.184.216.34",
        "172.15.255.255",
        "172.32.0.0",
        "100.63.255.255",
        "100.128.0.0",
        "192.167.255.255",
        "192.169.0.0",
    ] {
        assert_eq!(check(false, v4(address)), Ok(()), "{address}");
    }

    for address in ["2606:4700:4700::1111", "2001:4860:4860::8888"] {
        assert_eq!(check(false, v6(address)), Ok(()), "{address}");
    }
}

#[test]
fn ipv4_mapped_ipv6_is_canonicalized_before_the_policy() {
    let url = TrackerUrl::parse("http://[::ffff:127.0.0.1]/").expect("valid tracker URL");
    let address = url.host().canonical_ip().expect("IP literal");

    assert_eq!(address, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(
        check(false, address),
        Err(TrackerError::BlockedTarget),
        "canonical loopback is blocked"
    );

    // A mapped address returned by a resolver is always blocked, even when
    // private targets are enabled.
    let mapped = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001));
    assert_eq!(
        check(true, mapped),
        Err(TrackerError::BlockedTarget),
        "mapped form is blocked before any IPv4 translation"
    );
}

#[test]
fn the_policy_exposes_its_flag() {
    assert!(!TargetPolicy::new(false).allows_private_targets());
    assert!(TargetPolicy::new(true).allows_private_targets());
}
