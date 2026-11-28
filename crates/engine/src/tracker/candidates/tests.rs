use crate::metainfo::{FileInfo, InfoHash, MetaInfo, Mode};

use super::{
    CandidateWarnings, MAX_TIERS, MAX_TRACKER_URLS, MAX_URLS_PER_TIER, Shuffle, TrackerCandidates,
};
use crate::tracker::{TrackerError, TrackerUrl};

struct KeepOrder;

impl Shuffle for KeepOrder {
    fn shuffle(&mut self, _tier: &mut Vec<TrackerUrl>) {}
}

struct Reverse;

impl Shuffle for Reverse {
    fn shuffle(&mut self, tier: &mut Vec<TrackerUrl>) {
        tier.reverse();
    }
}

fn meta_owned(announce: Option<&str>, announce_list: Vec<Vec<String>>, private: bool) -> MetaInfo {
    MetaInfo {
        info_hash: InfoHash::from_bytes([0x11; 20]),
        name: "test".to_owned(),
        mode: Mode::Single,
        files: vec![FileInfo {
            path: vec!["test".to_owned()],
            length: 1,
        }],
        total_length: 1,
        piece_length: 16 * 1024,
        piece_hashes: vec![[0u8; 20]],
        announce: announce.map(str::to_owned),
        announce_list,
        private,
    }
}

fn meta(announce: Option<&str>, list: &[&[&str]], private: bool) -> MetaInfo {
    meta_owned(
        announce,
        list.iter()
            .map(|tier| tier.iter().map(|url| (*url).to_owned()).collect())
            .collect(),
        private,
    )
}

fn url(text: &str) -> TrackerUrl {
    TrackerUrl::parse(text).expect("valid tracker URL")
}

#[test]
fn a_single_tier_is_kept_in_order() {
    let meta = meta(
        Some("http://backup.example/announce"),
        &[&["http://a.example/announce", "udp://b.example:6969/announce"]],
        false,
    );

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(
        candidates.tiers(),
        &[vec![
            url("http://a.example/announce"),
            url("udp://b.example:6969/announce"),
        ]]
    );
    assert!(!candidates.is_private());
    assert_eq!(candidates.len(), 2);
}

#[test]
fn each_public_tier_is_shuffled_once() {
    let meta = meta(
        None,
        &[
            &["http://a.example/announce", "http://b.example/announce"],
            &["http://c.example/announce", "http://d.example/announce"],
        ],
        false,
    );

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut Reverse).expect("candidates");

    assert_eq!(
        candidates.tiers(),
        &[
            vec![
                url("http://b.example/announce"),
                url("http://a.example/announce"),
            ],
            vec![
                url("http://d.example/announce"),
                url("http://c.example/announce"),
            ],
        ]
    );
}

#[test]
fn an_invalid_only_list_falls_back_to_announce() {
    let meta = meta(
        Some("http://fallback.example/announce"),
        &[&["not a url", "ftp://bad.example/announce"]],
        false,
    );

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(
        candidates.tiers(),
        &[vec![url("http://fallback.example/announce")]]
    );
    assert_eq!(candidates.warnings().invalid_urls, 2);
}

#[test]
fn a_valid_list_ignores_a_different_valid_announce() {
    let meta = meta(
        Some("http://announce.example/announce"),
        &[&["http://list.example/announce"]],
        false,
    );

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(
        candidates.tiers(),
        &[vec![url("http://list.example/announce")]]
    );
    assert_eq!(candidates.len(), 1);
}

#[test]
fn duplicates_are_removed_after_normalization() {
    let meta = meta(
        None,
        &[&[
            "HTTP://Tracker.Example:80/announce",
            "http://tracker.example/announce",
            "http://tracker.example/announce?x=1",
        ]],
        false,
    );

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(
        candidates.tiers(),
        &[vec![
            url("http://tracker.example/announce"),
            url("http://tracker.example/announce?x=1"),
        ]]
    );
    assert_eq!(candidates.warnings().duplicate_urls, 1);
}

#[test]
fn invalid_schemes_and_ports_are_counted() {
    let meta = meta(
        Some("http://fallback.example/announce"),
        &[&[
            "ftp://bad.example/announce",
            "http://bad.example:99999/announce",
            "udp://bad.example/announce",
        ]],
        false,
    );

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(
        candidates.tiers(),
        &[vec![url("http://fallback.example/announce")]]
    );
    assert_eq!(candidates.warnings().invalid_urls, 3);
}

#[test]
fn the_url_cap_keeps_thirty_two_in_document_order() {
    let tiers: Vec<Vec<String>> = (0..9)
        .map(|tier| {
            (0..5)
                .map(|slot| format!("http://t{tier}u{slot}.example/announce"))
                .collect()
        })
        .collect();
    let meta = meta_owned(None, tiers, false);

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(candidates.len(), MAX_TRACKER_URLS);
    assert_eq!(candidates.tiers().len(), MAX_TIERS);
    for tier in candidates.tiers() {
        assert_eq!(tier.len(), MAX_URLS_PER_TIER);
    }
    assert_eq!(
        candidates.warnings(),
        CandidateWarnings {
            invalid_urls: 0,
            duplicate_urls: 0,
            dropped_urls: 13,
            dropped_tiers: 1,
        }
    );
}

#[test]
fn the_tier_cap_drops_extra_tiers() {
    let tiers: Vec<Vec<String>> = (0..9)
        .map(|tier| vec![format!("http://tier{tier}.example/announce")])
        .collect();
    let meta = meta_owned(None, tiers, false);

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(candidates.tiers().len(), MAX_TIERS);
    assert_eq!(candidates.len(), MAX_TIERS);
    assert_eq!(candidates.warnings().dropped_tiers, 1);
    assert_eq!(candidates.warnings().dropped_urls, 1);
}

#[test]
fn empty_tiers_are_removed_and_counted() {
    let meta = meta(None, &[&[], &["http://a.example/announce"], &[]], false);

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(
        candidates.tiers(),
        &[vec![url("http://a.example/announce")]]
    );
    assert_eq!(candidates.warnings().dropped_tiers, 2);
}

#[test]
fn a_private_torrent_flattens_and_shuffles_once() {
    let meta = meta(
        None,
        &[
            &["http://a.example/announce", "http://b.example/announce"],
            &["http://c.example/announce", "http://d.example/announce"],
        ],
        true,
    );

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut Reverse).expect("candidates");

    assert!(candidates.is_private());
    assert_eq!(candidates.tiers().len(), 1);
    assert_eq!(
        candidates.tiers()[0],
        vec![
            url("http://d.example/announce"),
            url("http://c.example/announce"),
            url("http://b.example/announce"),
            url("http://a.example/announce"),
        ]
    );
}

#[test]
fn a_valid_announce_is_used_when_the_list_is_absent() {
    let meta = meta(Some("http://announce.example/announce"), &[], false);

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");

    assert_eq!(
        candidates.tiers(),
        &[vec![url("http://announce.example/announce")]]
    );
    assert_eq!(candidates.warnings(), CandidateWarnings::default());
}

#[test]
fn no_usable_tracker_is_an_error() {
    for meta in [
        meta(None, &[], false),
        meta(Some(""), &[], false),
        meta(Some("ftp://bad.example/announce"), &[], false),
        meta(None, &[&["ftp://bad.example/announce"]], false),
        meta(Some("not a url"), &[&["also not a url"]], true),
    ] {
        assert_eq!(
            TrackerCandidates::from_metainfo(&meta, &mut KeepOrder),
            Err(TrackerError::NoUsableTracker)
        );
    }
}

#[test]
fn candidate_debug_does_not_leak_url_paths() {
    let meta = meta(
        None,
        &[&["http://tracker.example/announce/SECRET?passkey=TOPSECRET"]],
        false,
    );

    let candidates = TrackerCandidates::from_metainfo(&meta, &mut KeepOrder).expect("candidates");
    let text = format!("{candidates:?}");

    assert!(text.contains("TrackerUrl(http://tracker.example/…)"));
    assert!(!text.contains("SECRET"));
    assert!(!text.contains("TOPSECRET"));
}
