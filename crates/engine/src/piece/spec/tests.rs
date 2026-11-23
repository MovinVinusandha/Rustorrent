use super::*;
use crate::metainfo::{FileInfo, InfoHash, Mode};
use crate::piece::PeerKey;
use crate::storage::layout::FileMap;

const HASH_A: [u8; PIECE_HASH_LENGTH] = [0x11; PIECE_HASH_LENGTH];
const HASH_B: [u8; PIECE_HASH_LENGTH] = [0x22; PIECE_HASH_LENGTH];

fn meta(piece_length: u64, total_length: u64, hashes: Vec<[u8; PIECE_HASH_LENGTH]>) -> MetaInfo {
    MetaInfo {
        info_hash: InfoHash::from_bytes([0; PIECE_HASH_LENGTH]),
        name: "file.bin".to_owned(),
        mode: Mode::Single,
        files: vec![FileInfo {
            path: vec!["file.bin".to_owned()],
            length: total_length,
        }],
        total_length,
        piece_length,
        piece_hashes: hashes,
        announce: None,
        announce_list: Vec::new(),
        private: false,
    }
}

#[test]
fn peer_key_roundtrips_and_orders() {
    let key = PeerKey::from_raw(7);
    assert_eq!(key.raw(), 7);
    assert_eq!(key, PeerKey::from_raw(7));
    assert_ne!(key, PeerKey::from_raw(8));

    let copy = key;
    assert_eq!(copy, key);

    let mut keys = [
        PeerKey::from_raw(30),
        PeerKey::from_raw(10),
        PeerKey::from_raw(20),
    ];
    keys.sort();
    assert_eq!(
        keys.map(PeerKey::raw),
        [10, 20, 30],
        "peer keys must be stable for deterministic blame lists"
    );
}

#[test]
fn from_metainfo_uses_the_actual_last_piece_length() {
    let meta = meta(4096, 5000, vec![HASH_A, HASH_B]);
    let map = FileMap::build(&meta).expect("file map");

    let first = PieceSpec::from_metainfo(&map, &meta, 0).expect("first piece");
    assert_eq!(first.index(), 0);
    assert_eq!(first.offset(), 0);
    assert_eq!(first.end_offset(), 4096);
    assert_eq!(first.length(), 4096);
    assert_eq!(first.hash(), HASH_A);

    let last = PieceSpec::from_metainfo(&map, &meta, 1).expect("last piece");
    assert_eq!(last.index(), 1);
    assert_eq!(last.offset(), 4096);
    assert_eq!(last.end_offset(), 5000);
    assert_eq!(last.length(), 904);
    assert_eq!(last.hash(), HASH_B);
}

#[test]
fn from_metainfo_handles_a_64_mib_piece() {
    let total = MAX_PIECE_LENGTH + 100;
    let meta = meta(MAX_PIECE_LENGTH, total, vec![HASH_A, HASH_B]);
    let map = FileMap::build(&meta).expect("file map");

    let first = PieceSpec::from_metainfo(&map, &meta, 0).expect("first piece");
    assert_eq!(first.length(), MAX_PIECE_LENGTH as usize);
    assert_eq!(first.end_offset(), MAX_PIECE_LENGTH);

    let last = PieceSpec::from_metainfo(&map, &meta, 1).expect("last piece");
    assert_eq!(last.offset(), MAX_PIECE_LENGTH);
    assert_eq!(last.length(), 100);
    assert_eq!(last.end_offset(), total);
}

#[test]
fn new_checks_length_limits_and_offset_overflow() {
    assert_eq!(
        PieceSpec::new(0, 0, 0, HASH_A),
        Err(PieceSpecError::ZeroLength)
    );

    let max = usize::try_from(MAX_PIECE_LENGTH).expect("max fits usize");
    let over = usize::try_from(MAX_PIECE_LENGTH + 1).expect("limit+1 fits usize");
    assert!(matches!(
        PieceSpec::new(0, 0, over, HASH_A),
        Err(PieceSpecError::PieceTooLarge { length, max: limit })
            if length == over && limit == MAX_PIECE_LENGTH
    ));
    assert!(PieceSpec::new(0, 0, max, HASH_A).is_ok());

    assert_eq!(
        PieceSpec::new(0, u64::MAX, 1, HASH_A),
        Err(PieceSpecError::LengthOverflow {
            offset: u64::MAX,
            length: 1,
        })
    );
    let boundary = PieceSpec::new(0, u64::MAX - 1, 1, HASH_A).expect("boundary");
    assert_eq!(boundary.end_offset(), u64::MAX);
}

#[test]
fn from_metainfo_rejects_invalid_declared_piece_length() {
    let good = meta(4096, 5000, vec![HASH_A, HASH_B]);
    let map = FileMap::build(&good).expect("file map");

    let mut zero = good.clone();
    zero.piece_length = 0;
    assert_eq!(
        PieceSpec::from_metainfo(&map, &zero, 0),
        Err(PieceSpecError::ZeroPieceLength)
    );

    let mut too_large = good.clone();
    too_large.piece_length = MAX_PIECE_LENGTH + 1;
    assert!(matches!(
        PieceSpec::from_metainfo(&map, &too_large, 0),
        Err(PieceSpecError::PieceLengthTooLarge {
            piece_length,
            max,
        }) if piece_length == MAX_PIECE_LENGTH + 1 && max == MAX_PIECE_LENGTH
    ));
}

#[test]
fn from_metainfo_rejects_map_meta_mismatches() {
    let good = meta(4096, 5000, vec![HASH_A, HASH_B]);
    let map = FileMap::build(&good).expect("file map");

    assert_eq!(
        PieceSpec::from_metainfo(&map, &good, 2),
        Err(PieceSpecError::PieceOutOfRange {
            piece: 2,
            piece_count: 2,
        })
    );

    let mut wrong_piece_length = good.clone();
    wrong_piece_length.piece_length = 8192;
    assert_eq!(
        PieceSpec::from_metainfo(&map, &wrong_piece_length, 0),
        Err(PieceSpecError::PieceLengthMismatch {
            map: 4096,
            metainfo: 8192,
        })
    );

    let mut wrong_total = good.clone();
    wrong_total.total_length = 5001;
    assert_eq!(
        PieceSpec::from_metainfo(&map, &wrong_total, 0),
        Err(PieceSpecError::TotalLengthMismatch {
            map: 5000,
            metainfo: 5001,
        })
    );

    let mut wrong_hashes = good.clone();
    wrong_hashes.piece_hashes.pop();
    assert_eq!(
        PieceSpec::from_metainfo(&map, &wrong_hashes, 0),
        Err(PieceSpecError::HashCountMismatch {
            expected: 2,
            found: 1,
        })
    );
}

#[test]
fn piece_spec_is_copy_and_keeps_its_hash() {
    let spec = PieceSpec::new(3, 100, 4096, HASH_A).expect("spec");
    let copy = spec;
    assert_eq!(copy, spec);
    assert_eq!(copy.hash(), HASH_A);
    assert_eq!(copy.index(), 3);
    assert_eq!(copy.offset(), 100);
    assert_eq!(copy.length(), 4096);
    assert_eq!(copy.end_offset(), 4196);
}
