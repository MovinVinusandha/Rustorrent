use proptest::prelude::*;

use super::*;

#[test]
fn max_piece_count_is_derived_from_metainfo_limits() {
    assert_eq!(
        MAX_PIECE_COUNT,
        MAX_STRING_BYTES as u32 / PIECE_HASH_LENGTH as u32
    );
}

#[test]
fn new_rejects_piece_count_above_the_limit() {
    match Bitfield::new(MAX_PIECE_COUNT + 1) {
        Err(BitfieldError::TooManyPieces { count, max }) => {
            assert_eq!(count, MAX_PIECE_COUNT + 1);
            assert_eq!(max, MAX_PIECE_COUNT);
        }
        other => panic!("expected TooManyPieces, got {other:?}"),
    }
}

#[test]
fn new_accepts_zero_and_the_maximum() {
    let empty = Bitfield::new(0).expect("empty bitfield");
    assert_eq!(empty.piece_count(), 0);
    assert_eq!(empty.byte_len(), 0);
    assert!(empty.as_bytes().is_empty());

    let maximum = Bitfield::new(MAX_PIECE_COUNT).expect("maximum bitfield");
    assert_eq!(maximum.piece_count(), MAX_PIECE_COUNT);
    assert_eq!(maximum.byte_len(), (MAX_PIECE_COUNT as usize).div_ceil(8));
    assert_eq!(maximum.count_verified(), 0);
}

#[test]
fn byte_length_is_exact() {
    let cases = [
        (0u32, 0usize),
        (1, 1),
        (7, 1),
        (8, 1),
        (9, 2),
        (16, 2),
        (17, 3),
        (64, 8),
        (65, 9),
    ];
    for (piece_count, expected) in cases {
        let bitfield = Bitfield::new(piece_count).expect("bitfield");
        assert_eq!(bitfield.byte_len(), expected, "pieces={piece_count}");
    }
}

#[test]
fn get_set_roundtrip() {
    let mut bitfield = Bitfield::new(17).expect("bitfield");
    for piece in 0..17 {
        assert!(!bitfield.get(piece).expect("get"));
        bitfield.set(piece, true).expect("set");
        assert!(bitfield.get(piece).expect("get"));
        assert_eq!(bitfield.count_verified(), piece + 1);
    }
    for piece in 0..17 {
        bitfield.set(piece, false).expect("clear");
        assert!(!bitfield.get(piece).expect("get"));
    }
    assert_eq!(bitfield.count_verified(), 0);
}

#[test]
fn out_of_range_access_is_typed_and_does_not_mutate() {
    let mut bitfield = Bitfield::new(9).expect("bitfield");
    let before = bitfield.as_bytes().to_vec();

    assert_eq!(
        bitfield.get(9),
        Err(BitfieldError::OutOfRange {
            piece: 9,
            piece_count: 9,
        })
    );
    assert_eq!(
        bitfield.set(9, true),
        Err(BitfieldError::OutOfRange {
            piece: 9,
            piece_count: 9,
        })
    );
    assert_eq!(
        bitfield.set(u32::MAX, true),
        Err(BitfieldError::OutOfRange {
            piece: u32::MAX,
            piece_count: 9,
        })
    );
    assert_eq!(bitfield.as_bytes(), before.as_slice());
    assert_eq!(bitfield.count_verified(), 0);
}

#[test]
fn trailing_bits_are_zero_after_every_set() {
    for piece_count in 0u32..=17 {
        let mut bitfield = Bitfield::new(piece_count).expect("bitfield");
        for piece in 0..piece_count {
            bitfield.set(piece, true).expect("set");
        }
        assert_eq!(bitfield.count_verified(), piece_count);
        let mask = trailing_mask(piece_count);
        let last = bitfield.as_bytes().last().copied().unwrap_or(0);
        assert_eq!(last & !mask, 0, "pieces={piece_count}");
    }
}

#[test]
fn maximum_size_bitfield_sets_its_last_piece() {
    let mut bitfield = Bitfield::new(MAX_PIECE_COUNT).expect("bitfield");
    let last = MAX_PIECE_COUNT - 1;
    bitfield.set(last, true).expect("set last piece");
    assert!(bitfield.get(last).expect("get last piece"));
    assert_eq!(bitfield.count_verified(), 1);
    let mask = trailing_mask(MAX_PIECE_COUNT);
    let tail = bitfield.as_bytes().last().copied().expect("non-empty");
    assert_eq!(tail & !mask, 0);
}

#[test]
fn from_bytes_rejects_wrong_length() {
    assert_eq!(
        Bitfield::from_bytes(&[0], 9),
        Err(BitfieldError::WrongLength {
            expected: 2,
            found: 1,
            piece_count: 9,
        })
    );
    assert_eq!(
        Bitfield::from_bytes(&[0, 0, 0], 9),
        Err(BitfieldError::WrongLength {
            expected: 2,
            found: 3,
            piece_count: 9,
        })
    );
}

#[test]
fn from_bytes_rejects_trailing_bits() {
    assert_eq!(
        Bitfield::from_bytes(&[0x80], 1),
        Err(BitfieldError::TrailingBitsSet { piece_count: 1 })
    );
    assert_eq!(
        Bitfield::from_bytes(&[0x80], 7),
        Err(BitfieldError::TrailingBitsSet { piece_count: 7 })
    );
    assert_eq!(
        Bitfield::from_bytes(&[0, 0x80], 9),
        Err(BitfieldError::TrailingBitsSet { piece_count: 9 })
    );
}

#[test]
fn from_bytes_rejects_too_many_pieces_before_length() {
    assert_eq!(
        Bitfield::from_bytes(&[], MAX_PIECE_COUNT + 1),
        Err(BitfieldError::TooManyPieces {
            count: MAX_PIECE_COUNT + 1,
            max: MAX_PIECE_COUNT,
        })
    );
}

#[test]
fn from_bytes_accepts_canonical_bytes_and_roundtrips() {
    let bytes = [0b1010_0101, 0b0000_0001];
    let bitfield = Bitfield::from_bytes(&bytes, 9).expect("valid bitfield");
    for piece in [0, 2, 5, 7, 8] {
        assert!(bitfield.get(piece).expect("get"));
    }
    for piece in [1, 3, 4, 6] {
        assert!(!bitfield.get(piece).expect("get"));
    }
    assert_eq!(bitfield.count_verified(), 5);
    assert_eq!(bitfield.as_bytes(), bytes);
    assert_eq!(
        Bitfield::from_bytes(bitfield.as_bytes(), 9).expect("roundtrip"),
        bitfield
    );
}

#[test]
fn clear_zeroes_bits_and_trailing_bytes() {
    let mut bitfield = Bitfield::from_bytes(&[0xFF, 0x01], 9).expect("bitfield");
    assert_eq!(bitfield.count_verified(), 9);
    bitfield.clear();
    assert_eq!(bitfield.count_verified(), 0);
    assert_eq!(bitfield.as_bytes(), &[0, 0]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn set_get_matches_a_boolean_model(
        piece_count in 0u32..=512,
        operations in prop::collection::vec((0u32..=520, any::<bool>()), 0..=200),
    ) {
        let mut bitfield = Bitfield::new(piece_count).expect("bitfield");
        let mut model = vec![false; piece_count as usize];

        for (piece, value) in operations {
            match bitfield.set(piece, value) {
                Ok(()) => {
                    model[piece as usize] = value;
                }
                Err(BitfieldError::OutOfRange { .. }) => {
                    prop_assert!(piece >= piece_count);
                }
                Err(other) => prop_assert!(false, "unexpected error: {other:?}"),
            }
        }

        for (piece, expected) in model.iter().enumerate() {
            prop_assert_eq!(
                bitfield.get(piece as u32).expect("get"),
                *expected,
                "piece {}", piece
            );
        }
        prop_assert_eq!(
            bitfield.count_verified(),
            model.iter().filter(|value| **value).count() as u32
        );
        prop_assert_eq!(
            bitfield.byte_len(),
            (piece_count as usize).div_ceil(8)
        );
        let last = bitfield.as_bytes().last().copied().unwrap_or(0);
        prop_assert_eq!(last & !trailing_mask(piece_count), 0);

        let roundtrip = Bitfield::from_bytes(bitfield.as_bytes(), piece_count)
            .expect("roundtrip");
        prop_assert_eq!(roundtrip, bitfield);
    }

    #[test]
    fn nonzero_trailing_bits_are_rejected(
        piece_count in 1u32..=512,
        valid_bits in any::<u8>(),
    ) {
        prop_assume!(piece_count % 8 != 0);
        let len = (piece_count as usize).div_ceil(8);
        let mut bytes = vec![0u8; len];
        let mask = 1u8 << (piece_count % 8);
        bytes[len - 1] = mask | (valid_bits & (mask - 1));
        prop_assert_eq!(
            Bitfield::from_bytes(&bytes, piece_count),
            Err(BitfieldError::TrailingBitsSet { piece_count })
        );
    }
}
