use super::{MAX_TRACKER_ID_LEN, TrackerId};

#[test]
fn accepts_up_to_the_cap_and_ignores_longer_values() {
    assert!(TrackerId::new(&[0u8; MAX_TRACKER_ID_LEN]).is_some());
    assert!(TrackerId::new(&[0u8; MAX_TRACKER_ID_LEN + 1]).is_none());
    assert!(TrackerId::new(b"").is_some_and(|id| id.is_empty()));
}

#[test]
fn diagnostics_show_only_the_length() {
    let id = TrackerId::new(b"SECRET-PASSKEY").expect("valid tracker id");

    assert_eq!(id.as_bytes(), b"SECRET-PASSKEY");
    assert_eq!(id.len(), 14);
    assert!(!id.is_empty());
    assert_eq!(format!("{id:?}"), "TrackerId(14 bytes)");
    assert_eq!(id.to_string(), "TrackerId(14 bytes)");
    assert!(!format!("{id:?}").contains("SECRET"));
    assert!(!id.to_string().contains("PASSKEY"));
}
