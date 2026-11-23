use proptest::prelude::*;

use super::*;
use crate::metainfo::PIECE_HASH_LENGTH;
use crate::piece::{MemoryBudget, TorrentBudget};

const MIB: u64 = 1024 * 1024;
const HASH: [u8; PIECE_HASH_LENGTH] = [0x5A; PIECE_HASH_LENGTH];

fn spec(length: usize) -> PieceSpec {
    PieceSpec::new(0, 0, length, HASH).expect("piece spec")
}

fn torrent_budget(length: usize) -> TorrentBudget {
    let budget = MemoryBudget::new(128 * MIB).expect("memory budget");
    let declared = u64::try_from(length).expect("length fits u64");
    budget.torrent_budget(declared).expect("torrent budget")
}

fn piece_buffer(length: usize) -> PieceBuffer {
    let reservation = torrent_budget(length)
        .try_reserve(length)
        .expect("reservation");
    PieceBuffer::new(spec(length), reservation).expect("piece buffer")
}

fn block_bytes(block: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| {
            let base = usize::try_from(block).expect("block fits usize");
            u8::try_from((base * 31 + index) & 0xFF).expect("byte")
        })
        .collect()
}

fn block_offset(block: u32) -> u32 {
    let offset = usize::try_from(block).expect("block fits usize") * BLOCK_LEN;
    u32::try_from(offset).expect("offset fits u32")
}

#[test]
fn in_order_blocks_complete_the_piece() {
    let len = 4 * BLOCK_LEN;
    let mut buffer = piece_buffer(len);
    assert_eq!(buffer.piece(), 0);
    assert_eq!(buffer.piece_len(), len);
    assert_eq!(buffer.block_count(), 4);
    assert_eq!(buffer.reserved_bytes(), len);
    assert_eq!(buffer.received_blocks(), 0);
    assert!(!buffer.is_complete());
    assert_eq!(buffer.complete_bytes(), None);

    for block in 0..4 {
        let bytes = block_bytes(block, BLOCK_LEN);
        let outcome = buffer
            .insert(PeerKey::from_raw(1), block_offset(block), &bytes)
            .expect("insert");
        assert_eq!(
            outcome,
            InsertOutcome {
                block,
                complete: block == 3,
            }
        );
        assert_eq!(buffer.received_blocks(), block + 1);
        assert_eq!(buffer.is_complete(), block == 3);
    }

    let expected: Vec<u8> = (0..4)
        .flat_map(|block| block_bytes(block, BLOCK_LEN))
        .collect();
    assert_eq!(buffer.complete_bytes(), Some(expected.as_slice()));
}

#[test]
fn out_of_order_blocks_complete_the_piece() {
    let len = 4 * BLOCK_LEN;
    let mut buffer = piece_buffer(len);
    let order = [2u32, 0, 3, 1];
    let last_position = order.len() - 1;

    for (position, block) in order.into_iter().enumerate() {
        let bytes = block_bytes(block, BLOCK_LEN);
        let outcome = buffer
            .insert(PeerKey::from_raw(1), block_offset(block), &bytes)
            .expect("insert");
        assert_eq!(outcome.complete, position == last_position);
        assert_eq!(buffer.received_blocks(), position as u32 + 1);
    }

    let expected: Vec<u8> = (0..4)
        .flat_map(|block| block_bytes(block, BLOCK_LEN))
        .collect();
    assert_eq!(buffer.complete_bytes(), Some(expected.as_slice()));
}

#[test]
fn short_last_block_uses_the_actual_piece_length() {
    let len = 2 * BLOCK_LEN + 100;
    let mut buffer = piece_buffer(len);
    assert_eq!(buffer.block_count(), 3);
    assert_eq!(buffer.block_len(0), Some(BLOCK_LEN));
    assert_eq!(buffer.block_len(1), Some(BLOCK_LEN));
    assert_eq!(buffer.block_len(2), Some(100));
    assert_eq!(buffer.block_len(3), None);
    assert_eq!(buffer.block_len(u32::MAX), None);

    let oversized = block_bytes(2, BLOCK_LEN);
    assert_eq!(
        buffer
            .insert(PeerKey::from_raw(1), block_offset(2), &oversized)
            .err(),
        Some(BlockError::WrongLength {
            block: 2,
            expected: 100,
            found: BLOCK_LEN,
        })
    );

    let last = block_bytes(2, 100);
    let outcome = buffer
        .insert(PeerKey::from_raw(1), block_offset(2), &last)
        .expect("short last block");
    assert!(!outcome.complete);
    assert_eq!(buffer.received_blocks(), 1);
}

#[test]
fn piece_smaller_than_a_block_has_one_short_block() {
    let len = 4096;
    let mut buffer = piece_buffer(len);
    assert_eq!(buffer.block_count(), 1);
    assert_eq!(buffer.block_len(0), Some(4096));
    assert_eq!(buffer.block_len(1), None);

    let full_block = block_bytes(0, BLOCK_LEN);
    assert_eq!(
        buffer.insert(PeerKey::from_raw(1), 0, &full_block).err(),
        Some(BlockError::WrongLength {
            block: 0,
            expected: 4096,
            found: BLOCK_LEN,
        })
    );

    let bytes = block_bytes(0, 4096);
    let outcome = buffer
        .insert(PeerKey::from_raw(1), 0, &bytes)
        .expect("insert");
    assert_eq!(
        outcome,
        InsertOutcome {
            block: 0,
            complete: true,
        }
    );
    assert_eq!(buffer.complete_bytes(), Some(bytes.as_slice()));
}

#[test]
fn duplicate_preserves_the_first_peer_and_bytes() {
    let len = BLOCK_LEN;
    let mut buffer = piece_buffer(len);
    let first = block_bytes(0, BLOCK_LEN);

    buffer
        .insert(PeerKey::from_raw(1), 0, &first)
        .expect("first write");

    let second = vec![0xFF; BLOCK_LEN];
    assert_eq!(
        buffer.insert(PeerKey::from_raw(2), 0, &second).err(),
        Some(BlockError::Duplicate { block: 0 })
    );
    assert_eq!(buffer.received_blocks(), 1);
    assert_eq!(
        buffer.block_contributor(0),
        Some(PeerKey::from_raw(1)),
        "the duplicate must not steal attribution"
    );
    assert_eq!(buffer.complete_bytes(), Some(first.as_slice()));
}

#[test]
fn misaligned_offsets_are_rejected() {
    let mut buffer = piece_buffer(BLOCK_LEN);
    let bytes = block_bytes(0, BLOCK_LEN);

    for offset in [1u32, BLOCK_LEN as u32 - 1, BLOCK_LEN as u32 + 1] {
        assert_eq!(
            buffer.insert(PeerKey::from_raw(1), offset, &bytes).err(),
            Some(BlockError::Misaligned {
                offset,
                block_len: BLOCK_LEN,
            })
        );
    }

    assert_eq!(buffer.received_blocks(), 0);
    assert_eq!(buffer.complete_bytes(), None);
    assert!(buffer.contributors().is_empty());
}

#[test]
fn out_of_range_offsets_are_rejected() {
    let len = 4096;
    let mut buffer = piece_buffer(len);
    let bytes = block_bytes(1, BLOCK_LEN);

    for offset in [BLOCK_LEN as u32, 2 * BLOCK_LEN as u32] {
        assert_eq!(
            buffer.insert(PeerKey::from_raw(1), offset, &bytes).err(),
            Some(BlockError::OutOfRange {
                offset,
                piece_len: len,
            })
        );
    }

    assert_eq!(buffer.received_blocks(), 0);
}

#[test]
fn wrong_length_payloads_are_rejected() {
    let len = 2 * BLOCK_LEN + 10;
    let mut buffer = piece_buffer(len);

    let cases = [
        (0u32, BLOCK_LEN - 1, BLOCK_LEN),
        (1, BLOCK_LEN + 1, BLOCK_LEN),
        (2, 11, 10),
        (2, 0, 10),
    ];
    for (block, found, expected) in cases {
        let bytes = block_bytes(block, found);
        assert_eq!(
            buffer
                .insert(PeerKey::from_raw(1), block_offset(block), &bytes)
                .err(),
            Some(BlockError::WrongLength {
                block,
                expected,
                found,
            })
        );
    }

    assert_eq!(buffer.received_blocks(), 0);
    let valid = block_bytes(1, BLOCK_LEN);
    buffer
        .insert(PeerKey::from_raw(1), block_offset(1), &valid)
        .expect("valid block after rejections");
    assert_eq!(buffer.received_blocks(), 1);
}

#[test]
fn validation_order_is_alignment_range_length_then_duplicate() {
    let len = 4096;
    let mut buffer = piece_buffer(len);

    assert_eq!(
        buffer.insert(PeerKey::from_raw(1), 4097, &[]).err(),
        Some(BlockError::Misaligned {
            offset: 4097,
            block_len: BLOCK_LEN,
        }),
        "alignment is checked before range and length"
    );
    assert_eq!(
        buffer
            .insert(PeerKey::from_raw(1), BLOCK_LEN as u32, &[])
            .err(),
        Some(BlockError::OutOfRange {
            offset: BLOCK_LEN as u32,
            piece_len: len,
        }),
        "range is checked before length"
    );

    let first = block_bytes(0, len);
    buffer
        .insert(PeerKey::from_raw(1), 0, &first)
        .expect("first write");
    assert_eq!(
        buffer.insert(PeerKey::from_raw(2), 0, &[1, 2, 3]).err(),
        Some(BlockError::WrongLength {
            block: 0,
            expected: len,
            found: 3,
        }),
        "length is checked before duplicate"
    );
    assert_eq!(
        buffer.block_contributor(0),
        Some(PeerKey::from_raw(1)),
        "a rejected duplicate must not touch the buffer"
    );
}

#[test]
fn rejected_blocks_leave_the_buffer_untouched() {
    let len = 3 * BLOCK_LEN;
    let mut buffer = piece_buffer(len);

    let middle = block_bytes(1, BLOCK_LEN);
    buffer
        .insert(PeerKey::from_raw(5), block_offset(1), &middle)
        .expect("middle block");

    let bad = [
        (PeerKey::from_raw(6), 1u32, block_bytes(9, BLOCK_LEN)),
        (
            PeerKey::from_raw(6),
            block_offset(3),
            block_bytes(9, BLOCK_LEN),
        ),
        (PeerKey::from_raw(6), 0, block_bytes(9, BLOCK_LEN - 1)),
        (PeerKey::from_raw(6), block_offset(1), vec![0xAB; BLOCK_LEN]),
    ];
    for (peer, offset, bytes) in bad {
        assert!(buffer.insert(peer, offset, &bytes).is_err());
    }

    let first = block_bytes(0, BLOCK_LEN);
    let last = block_bytes(2, BLOCK_LEN);
    buffer
        .insert(PeerKey::from_raw(7), 0, &first)
        .expect("first block");
    buffer
        .insert(PeerKey::from_raw(9), block_offset(2), &last)
        .expect("last block");

    let expected: Vec<u8> = [first.as_slice(), middle.as_slice(), last.as_slice()].concat();
    assert_eq!(buffer.complete_bytes(), Some(expected.as_slice()));
    assert_eq!(
        buffer.contributors(),
        vec![
            PeerKey::from_raw(5),
            PeerKey::from_raw(7),
            PeerKey::from_raw(9)
        ]
    );
}

#[test]
fn debug_shows_shape_only_never_the_data() {
    let len = 2 * BLOCK_LEN;
    let mut buffer = piece_buffer(len);

    assert_eq!(
        format!("{buffer:?}"),
        "PieceBuffer { index: 0, length: 32768, received: 0, .. }"
    );

    buffer
        .insert(PeerKey::from_raw(1), 0, &vec![0xAB; BLOCK_LEN])
        .expect("block 0");
    assert_eq!(
        format!("{buffer:?}"),
        "PieceBuffer { index: 0, length: 32768, received: 1, .. }"
    );
}

#[test]
fn contributors_are_sorted_and_deduplicated() {
    let len = 3 * BLOCK_LEN;
    let mut buffer = piece_buffer(len);

    for (block, peer) in [(0u32, 9u64), (1, 3), (2, 9)] {
        let bytes = block_bytes(block, BLOCK_LEN);
        buffer
            .insert(PeerKey::from_raw(peer), block_offset(block), &bytes)
            .expect("insert");
    }

    assert_eq!(
        buffer.contributors(),
        vec![PeerKey::from_raw(3), PeerKey::from_raw(9)]
    );
    assert_eq!(buffer.block_contributor(0), Some(PeerKey::from_raw(9)));
    assert_eq!(buffer.block_contributor(1), Some(PeerKey::from_raw(3)));
    assert_eq!(buffer.block_contributor(2), Some(PeerKey::from_raw(9)));
    assert_eq!(buffer.block_contributor(3), None);

    let mut partial = piece_buffer(2 * BLOCK_LEN);
    let bytes = block_bytes(0, BLOCK_LEN);
    partial
        .insert(PeerKey::from_raw(1), 0, &bytes)
        .expect("insert");
    assert_eq!(partial.block_contributor(1), None, "missing block");
}

#[test]
fn incomplete_piece_has_no_bytes() {
    let len = 2 * BLOCK_LEN;
    let mut buffer = piece_buffer(len);
    let bytes = block_bytes(1, BLOCK_LEN);

    buffer
        .insert(PeerKey::from_raw(1), block_offset(1), &bytes)
        .expect("insert");

    assert_eq!(buffer.received_blocks(), 1);
    assert!(!buffer.is_complete());
    assert_eq!(buffer.complete_bytes(), None);
    assert_eq!(buffer.block_len(1), Some(BLOCK_LEN));
}

#[test]
fn reservation_must_match_the_piece_length() {
    let len = BLOCK_LEN;
    let torrent = torrent_budget(len);

    let too_small = torrent.try_reserve(len - 1).expect("small reservation");
    assert_eq!(
        PieceBuffer::new(spec(len), too_small).err(),
        Some(PieceBufferError::ReservationMismatch {
            reserved: len - 1,
            needed: len,
        })
    );
    assert_eq!(torrent.in_use_bytes(), 0, "failed buffer leaked bytes");
    assert_eq!(torrent.in_flight(), 0, "failed buffer leaked a slot");

    let too_large = torrent.try_reserve(len).expect("large reservation");
    assert_eq!(
        PieceBuffer::new(spec(len - 1), too_large).err(),
        Some(PieceBufferError::ReservationMismatch {
            reserved: len,
            needed: len - 1,
        })
    );
    assert_eq!(torrent.in_use_bytes(), 0);
    assert_eq!(torrent.in_flight(), 0);
}

#[test]
fn dropping_the_buffer_releases_the_reservation() {
    let len = 4 * BLOCK_LEN;
    let torrent = torrent_budget(len);
    let reservation = torrent.try_reserve(len).expect("reservation");
    let buffer = PieceBuffer::new(spec(len), reservation).expect("buffer");

    assert_eq!(torrent.in_use_bytes(), u64::try_from(len).expect("len"));
    assert_eq!(torrent.in_flight(), 1);

    drop(buffer);
    assert_eq!(torrent.in_use_bytes(), 0);
    assert_eq!(torrent.in_flight(), 0);
}

#[test]
fn spec_and_reserved_bytes_are_exposed() {
    let len = 4096;
    let torrent = torrent_budget(len);
    let reservation = torrent.try_reserve(len).expect("reservation");
    let spec = PieceSpec::new(7, 1234, len, HASH).expect("spec");
    let buffer = PieceBuffer::new(spec, reservation).expect("buffer");

    assert_eq!(buffer.piece(), 7);
    assert_eq!(buffer.piece_len(), len);
    assert_eq!(buffer.reserved_bytes(), len);
    assert_eq!(buffer.spec(), &spec);
    assert_eq!(buffer.spec().offset(), 1234);
    assert_eq!(buffer.spec().hash(), HASH);
}

#[test]
fn only_duplicates_are_benign() {
    assert!(!BlockError::Duplicate { block: 0 }.is_protocol_violation());
    assert!(
        BlockError::Misaligned {
            offset: 1,
            block_len: BLOCK_LEN,
        }
        .is_protocol_violation()
    );
    assert!(
        BlockError::OutOfRange {
            offset: BLOCK_LEN as u32,
            piece_len: BLOCK_LEN,
        }
        .is_protocol_violation()
    );
    assert!(
        BlockError::WrongLength {
            block: 0,
            expected: BLOCK_LEN,
            found: 1,
        }
        .is_protocol_violation()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn inserts_match_a_block_model(
        piece_len in 1usize..=(4 * BLOCK_LEN + 17),
        operations in prop::collection::vec(
            (0u32..=64, any::<u64>(), any::<bool>()),
            0..=48,
        ),
    ) {
        let block_count = piece_len.div_ceil(BLOCK_LEN) as u32;
        let mut buffer = piece_buffer(piece_len);
        prop_assert_eq!(buffer.block_count(), block_count);

        let mut filled = vec![false; block_count as usize];
        let mut expected = vec![0u8; piece_len];

        for (block, raw_peer, correct_len) in operations {
            let offset = block_offset(block);
            let peer = PeerKey::from_raw(raw_peer);
            let in_range = block < block_count;
            let expected_len = if in_range {
                (piece_len - block as usize * BLOCK_LEN).min(BLOCK_LEN)
            } else {
                0
            };
            let supplied_len = if correct_len {
                expected_len
            } else {
                expected_len + 1
            };
            let bytes = block_bytes(block, supplied_len);
            let result = buffer.insert(peer, offset, &bytes);

            if !in_range {
                prop_assert_eq!(
                    result.err(),
                    Some(BlockError::OutOfRange {
                        offset,
                        piece_len,
                    })
                );
            } else if supplied_len != expected_len {
                prop_assert_eq!(
                    result.err(),
                    Some(BlockError::WrongLength {
                        block,
                        expected: expected_len,
                        found: supplied_len,
                    })
                );
            } else if filled[block as usize] {
                prop_assert_eq!(result.err(), Some(BlockError::Duplicate { block }));
            } else {
                let outcome = result.expect("valid insert");
                prop_assert_eq!(outcome.block, block);
                filled[block as usize] = true;
                let start = block as usize * BLOCK_LEN;
                expected[start..start + expected_len].copy_from_slice(&bytes);
                prop_assert_eq!(
                    outcome.complete,
                    filled.iter().all(|value| *value)
                );
            }
        }

        let received = filled.iter().filter(|value| **value).count() as u32;
        prop_assert_eq!(buffer.received_blocks(), received);
        if filled.iter().all(|value| *value) {
            prop_assert_eq!(buffer.complete_bytes(), Some(expected.as_slice()));
        } else {
            prop_assert_eq!(buffer.complete_bytes(), None);
        }
    }
}
