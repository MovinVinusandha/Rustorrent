//! Tracker target address policy.
//!
//! Pure address classification for the SSRF guard. No I/O, no DNS and no
//! clock. The policy is applied to every resolved address and to every
//! IP-literal URL before a socket is created.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::TrackerError;

/// Address policy for tracker targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetPolicy {
    allow_private_targets: bool,
}

impl TargetPolicy {
    /// Builds a policy; private targets are blocked unless the flag is set.
    pub fn new(allow_private_targets: bool) -> Self {
        Self {
            allow_private_targets,
        }
    }

    /// Returns `true` when private targets are allowed.
    pub fn allows_private_targets(&self) -> bool {
        self.allow_private_targets
    }

    /// Checks one address against the policy.
    pub fn check(&self, address: IpAddr) -> Result<(), TrackerError> {
        match address {
            IpAddr::V4(address) => self.check_v4(address),
            IpAddr::V6(address) => self.check_v6(address),
        }
    }

    fn check_v4(&self, address: Ipv4Addr) -> Result<(), TrackerError> {
        const ALWAYS_BLOCKED: &[(Ipv4Addr, u8)] = &[
            (Ipv4Addr::new(0, 0, 0, 0), 8),
            (Ipv4Addr::new(224, 0, 0, 0), 4),
            (Ipv4Addr::new(255, 255, 255, 255), 32),
            (Ipv4Addr::new(192, 0, 0, 0), 24),
            (Ipv4Addr::new(192, 0, 2, 0), 24),
            (Ipv4Addr::new(198, 51, 100, 0), 24),
            (Ipv4Addr::new(203, 0, 113, 0), 24),
            (Ipv4Addr::new(198, 18, 0, 0), 15),
            (Ipv4Addr::new(240, 0, 0, 0), 4),
        ];
        if in_v4_ranges(address, ALWAYS_BLOCKED) {
            return Err(TrackerError::BlockedTarget);
        }

        const PRIVATE: &[(Ipv4Addr, u8)] = &[
            (Ipv4Addr::new(10, 0, 0, 0), 8),
            (Ipv4Addr::new(172, 16, 0, 0), 12),
            (Ipv4Addr::new(192, 168, 0, 0), 16),
            (Ipv4Addr::new(100, 64, 0, 0), 10),
            (Ipv4Addr::new(127, 0, 0, 0), 8),
            (Ipv4Addr::new(169, 254, 0, 0), 16),
        ];
        if !self.allow_private_targets && in_v4_ranges(address, PRIVATE) {
            return Err(TrackerError::BlockedTarget);
        }

        Ok(())
    }

    fn check_v6(&self, address: Ipv6Addr) -> Result<(), TrackerError> {
        const ALWAYS_BLOCKED: &[(Ipv6Addr, u8)] = &[
            (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 128),
            (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
            (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
            (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
            (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
            (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32),
            (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
            (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
            (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
        ];
        if in_v6_ranges(address, ALWAYS_BLOCKED) {
            return Err(TrackerError::BlockedTarget);
        }

        const PRIVATE: &[(Ipv6Addr, u8)] = &[
            (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1), 128),
            (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
            (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
        ];
        if !self.allow_private_targets && in_v6_ranges(address, PRIVATE) {
            return Err(TrackerError::BlockedTarget);
        }

        Ok(())
    }
}

fn in_v4_ranges(address: Ipv4Addr, ranges: &[(Ipv4Addr, u8)]) -> bool {
    ranges
        .iter()
        .any(|(network, prefix)| v4_in(address, *network, *prefix))
}

fn in_v6_ranges(address: Ipv6Addr, ranges: &[(Ipv6Addr, u8)]) -> bool {
    ranges
        .iter()
        .any(|(network, prefix)| v6_in(address, *network, *prefix))
}

fn v4_in(address: Ipv4Addr, network: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    u32::from(address) & mask == u32::from(network) & mask
}

fn v6_in(address: Ipv6Addr, network: Ipv6Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix))
    };
    u128::from(address) & mask == u128::from(network) & mask
}

#[cfg(test)]
mod tests;
