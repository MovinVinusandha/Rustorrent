#![no_main]

use libfuzzer_sys::fuzz_target;

use bt_engine::piece::{BLOCK_LEN, MemoryBudget, PeerKey, PieceBuffer, PieceSpec};

/// Largest piece the target builds: five 16 KiB blocks, including a short last
/// block. Keeps every case fast enough for coverage-guided fuzzing; the 64 MiB
/// limit is covered by unit tests.
const MAX_PIECE_LEN: u32 = 4 * BLOCK_LEN as u32 + 2;

const GLOBAL_BUDGET: u64 = 128 * 1024 * 1024;

const RECORD_LEN: usize = 14;

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }

    let raw_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let piece_len = (raw_len % MAX_PIECE_LEN) as usize;
    if piece_len == 0 {
        return;
    }

    let budget = MemoryBudget::new(GLOBAL_BUDGET).expect("memory budget");
    let torrent = budget
        .torrent_budget(piece_len as u64)
        .expect("torrent budget");
    let reservation = torrent.try_reserve(piece_len).expect("reservation");
    let spec = PieceSpec::new(0, 0, piece_len, [0; 20]).expect("piece spec");
    let mut buffer = PieceBuffer::new(spec, reservation).expect("piece buffer");

    let blocks = buffer.block_count();
    assert_eq!(blocks, piece_len.div_ceil(BLOCK_LEN) as u32);
    assert_eq!(buffer.received_blocks(), 0);
    assert!(!buffer.is_complete());
    assert!(buffer.complete_bytes().is_none());

    let mut total = 0;
    for block in 0..blocks {
        let len = buffer.block_len(block).expect("in-range block length");
        assert!((1..=BLOCK_LEN).contains(&len));
        total += len;
    }
    assert_eq!(total, piece_len);
    assert_eq!(buffer.block_len(blocks), None);

    let mut filled = vec![false; blocks as usize];
    let mut expected = vec![0u8; piece_len];
    let mut rest = &data[4..];

    while rest.len() >= RECORD_LEN {
        let offset = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let mut peer_bytes = [0u8; 8];
        peer_bytes.copy_from_slice(&rest[4..12]);
        let peer = PeerKey::from_raw(u64::from_le_bytes(peer_bytes));
        let payload_len = usize::from(u16::from_le_bytes([rest[12], rest[13]])) % (BLOCK_LEN + 2);
        let payload: Vec<u8> = (0..payload_len).map(|index| index as u8 ^ 0xA5).collect();
        rest = &rest[RECORD_LEN..];

        let received_before = buffer.received_blocks();
        match buffer.insert(peer, offset, &payload) {
            Ok(outcome) => {
                assert_eq!(buffer.received_blocks(), received_before + 1);
                assert_eq!(buffer.is_complete(), outcome.complete);

                let start = usize::try_from(offset).expect("offset fits usize");
                assert_eq!(start % BLOCK_LEN, 0, "accepted a misaligned block");
                let block = outcome.block;
                let block_len = buffer.block_len(block).expect("in-range block");
                assert_eq!(payload.len(), block_len);
                assert!(!filled[block as usize], "accepted a duplicate");
                filled[block as usize] = true;
                expected[start..start + block_len].copy_from_slice(&payload);
            }
            Err(error) => {
                assert_eq!(
                    buffer.received_blocks(),
                    received_before,
                    "rejected block changed the received count"
                );
                assert_eq!(
                    error.is_protocol_violation(),
                    !matches!(error, bt_engine::piece::BlockError::Duplicate { .. })
                );
            }
        }
    }

    let received = filled.iter().filter(|filled| **filled).count() as u32;
    assert_eq!(buffer.received_blocks(), received);
    if filled.iter().all(|filled| *filled) {
        assert_eq!(buffer.complete_bytes(), Some(expected.as_slice()));
    } else {
        assert!(buffer.complete_bytes().is_none());
    }

    assert_eq!(buffer.reserved_bytes(), piece_len);
});
