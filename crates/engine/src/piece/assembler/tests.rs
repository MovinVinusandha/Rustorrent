use super::*;
use crate::metainfo::PIECE_HASH_LENGTH;
use crate::piece::{BLOCK_LEN, MemoryBudget, PeerKey, PieceSpec, TorrentBudget};

const MIB: u64 = 1024 * 1024;
const HASH: [u8; PIECE_HASH_LENGTH] = [0x5A; PIECE_HASH_LENGTH];

fn torrent_budget(length: usize) -> TorrentBudget {
    let budget = MemoryBudget::new(128 * MIB).expect("memory budget");
    let declared = u64::try_from(length).expect("length fits u64");
    budget.torrent_budget(declared).expect("torrent budget")
}

fn start_piece(
    assembler: &mut PieceAssembler,
    torrent: &TorrentBudget,
    index: u32,
    length: usize,
    now: Instant,
) {
    let length_u64 = u64::try_from(length).expect("length fits u64");
    let offset = u64::from(index) * length_u64;
    let spec = PieceSpec::new(index, offset, length, HASH).expect("piece spec");
    let reservation = torrent.try_reserve(length).expect("reservation");
    assembler.start_at(spec, reservation, now).expect("start");
}

fn setup_at(length: usize, now: Instant) -> (PieceAssembler, TorrentBudget, PieceSpec) {
    let torrent = torrent_budget(length);
    let spec = PieceSpec::new(0, 0, length, HASH).expect("piece spec");
    let mut assembler = PieceAssembler::new(DEFAULT_STALL_TIMEOUT).expect("assembler");
    start_piece(&mut assembler, &torrent, 0, length, now);
    (assembler, torrent, spec)
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
    block * u32::try_from(BLOCK_LEN).expect("block size fits u32")
}

#[test]
fn completing_a_piece_returns_the_buffer_and_releases_the_slot() {
    let t0 = Instant::now();
    let len = 2 * BLOCK_LEN;
    let (mut assembler, torrent, spec) = setup_at(len, t0);
    let len_u64 = u64::try_from(len).expect("len fits u64");

    assert!(assembler.is_in_flight(0));
    assert_eq!(assembler.in_flight(), 1);
    assert_eq!(assembler.stall_timeout(), DEFAULT_STALL_TIMEOUT);
    assert_eq!(torrent.in_use_bytes(), len_u64);

    let first = block_bytes(0, BLOCK_LEN);
    let progress = t0 + Duration::from_secs(5);
    match assembler
        .insert_at(0, PeerKey::from_raw(1), 0, &first, progress)
        .expect("block 0")
    {
        InsertResult::Incomplete(outcome) => {
            assert_eq!(outcome.block, 0);
            assert!(!outcome.complete);
        }
        InsertResult::Complete(_) => panic!("piece completed too early"),
    }
    assert_eq!(assembler.last_progress(0), Some(progress));

    let last = block_bytes(1, BLOCK_LEN);
    let completed_at = t0 + Duration::from_secs(10);
    match assembler
        .insert_at(
            0,
            PeerKey::from_raw(2),
            block_offset(1),
            &last,
            completed_at,
        )
        .expect("block 1")
    {
        InsertResult::Complete(buffer) => {
            assert_eq!(buffer.piece(), 0);
            assert_eq!(buffer.spec(), &spec);
            let expected = [first.as_slice(), last.as_slice()].concat();
            assert_eq!(buffer.complete_bytes(), Some(expected.as_slice()));
            assert_eq!(
                buffer.contributors(),
                vec![PeerKey::from_raw(1), PeerKey::from_raw(2)]
            );
            assert_eq!(torrent.in_use_bytes(), len_u64);
            drop(buffer);
        }
        InsertResult::Incomplete(_) => panic!("final block did not complete the piece"),
    }

    assert!(!assembler.is_in_flight(0));
    assert_eq!(assembler.in_flight(), 0);
    assert_eq!(torrent.in_use_bytes(), 0);
    assert_eq!(torrent.in_flight(), 0);
}

#[test]
fn a_late_block_after_completion_is_not_started_and_benign() {
    let t0 = Instant::now();
    let len = BLOCK_LEN;
    let (mut assembler, _torrent, _spec) = setup_at(len, t0);
    let bytes = block_bytes(0, len);

    let result = assembler
        .insert_at(0, PeerKey::from_raw(1), 0, &bytes, t0)
        .expect("complete");
    assert!(matches!(result, InsertResult::Complete(_)));

    let error = assembler
        .insert_at(0, PeerKey::from_raw(2), 0, &bytes, t0)
        .err()
        .expect("late block error");
    assert_eq!(error, PieceError::NotStarted { piece: 0 });
    assert!(!error.is_protocol_violation());
}

#[test]
fn inserting_an_unknown_piece_is_not_started() {
    let t0 = Instant::now();
    let mut assembler = PieceAssembler::new(DEFAULT_STALL_TIMEOUT).expect("assembler");

    let error = assembler
        .insert_at(7, PeerKey::from_raw(1), 0, &[], t0)
        .err()
        .expect("unknown piece error");
    assert_eq!(error, PieceError::NotStarted { piece: 7 });
    assert!(!error.is_protocol_violation());
    assert_eq!(assembler.in_flight(), 0);
}

#[test]
fn starting_an_in_flight_piece_is_rejected_without_leaking() {
    let t0 = Instant::now();
    let len = BLOCK_LEN;
    let (mut assembler, torrent, spec) = setup_at(len, t0);
    let len_u64 = u64::try_from(len).expect("len fits u64");

    let reservation = torrent.try_reserve(len).expect("reservation");
    assert_eq!(
        assembler.start_at(spec, reservation, t0).err(),
        Some(PieceError::AlreadyInFlight { piece: 0 })
    );
    assert_eq!(assembler.in_flight(), 1);
    assert_eq!(torrent.in_use_bytes(), len_u64);
    assert_eq!(torrent.in_flight(), 1);
}

#[test]
fn a_zero_stall_timeout_is_rejected() {
    assert_eq!(
        PieceAssembler::new(Duration::ZERO).err(),
        Some(PieceError::ZeroStallTimeout)
    );
}

#[test]
fn stalled_pieces_expire_and_release_their_budget() {
    let t0 = Instant::now();
    let len = 2 * BLOCK_LEN;
    let (mut assembler, torrent, _spec) = setup_at(len, t0);
    let len_u64 = u64::try_from(len).expect("len fits u64");

    assert!(
        assembler
            .sweep_stalled(t0 - Duration::from_secs(1))
            .is_empty()
    );
    assert!(
        assembler
            .sweep_stalled(t0 + DEFAULT_STALL_TIMEOUT - Duration::from_millis(1))
            .is_empty()
    );
    assert_eq!(assembler.in_flight(), 1);
    assert_eq!(torrent.in_use_bytes(), len_u64);

    assert_eq!(assembler.sweep_stalled(t0 + DEFAULT_STALL_TIMEOUT), vec![0]);
    assert!(!assembler.is_in_flight(0));
    assert_eq!(assembler.in_flight(), 0);
    assert_eq!(torrent.in_use_bytes(), 0);
    assert_eq!(torrent.in_flight(), 0);

    let bytes = block_bytes(0, BLOCK_LEN);
    let error = assembler
        .insert_at(
            0,
            PeerKey::from_raw(1),
            0,
            &bytes,
            t0 + DEFAULT_STALL_TIMEOUT,
        )
        .err()
        .expect("expired piece error");
    assert_eq!(error, PieceError::NotStarted { piece: 0 });
}

#[test]
fn slow_steady_progress_keeps_a_piece_alive() {
    let t0 = Instant::now();
    let len = 3 * BLOCK_LEN;
    let (mut assembler, _torrent, _spec) = setup_at(len, t0);

    let first = block_bytes(0, BLOCK_LEN);
    assert!(matches!(
        assembler
            .insert_at(
                0,
                PeerKey::from_raw(1),
                0,
                &first,
                t0 + Duration::from_secs(29),
            )
            .expect("block 0"),
        InsertResult::Incomplete(_)
    ));
    assert!(
        assembler
            .sweep_stalled(t0 + Duration::from_secs(58))
            .is_empty(),
        "29 seconds of progress must stay alive"
    );

    let second = block_bytes(1, BLOCK_LEN);
    assert!(matches!(
        assembler
            .insert_at(
                0,
                PeerKey::from_raw(1),
                block_offset(1),
                &second,
                t0 + Duration::from_secs(58),
            )
            .expect("block 1"),
        InsertResult::Incomplete(_)
    ));
    assert!(
        assembler
            .sweep_stalled(t0 + Duration::from_secs(87))
            .is_empty(),
        "29 seconds of progress must stay alive"
    );
    assert_eq!(
        assembler.sweep_stalled(t0 + Duration::from_secs(88)),
        vec![0],
        "30 seconds without progress must expire"
    );
}

#[test]
fn rejected_blocks_do_not_refresh_the_stall_deadline() {
    let t0 = Instant::now();
    let len = 2 * BLOCK_LEN;
    let (mut assembler, torrent, _spec) = setup_at(len, t0);
    let first = block_bytes(0, BLOCK_LEN);

    assert!(matches!(
        assembler
            .insert_at(
                0,
                PeerKey::from_raw(1),
                0,
                &first,
                t0 + Duration::from_secs(10),
            )
            .expect("block 0"),
        InsertResult::Incomplete(_)
    ));
    assert_eq!(
        assembler.last_progress(0),
        Some(t0 + Duration::from_secs(10))
    );

    let misaligned = assembler
        .insert_at(
            0,
            PeerKey::from_raw(1),
            1,
            &first,
            t0 + Duration::from_secs(20),
        )
        .err()
        .expect("misaligned error");
    assert_eq!(
        misaligned,
        PieceError::Block(BlockError::Misaligned {
            offset: 1,
            block_len: BLOCK_LEN,
        })
    );
    assert!(misaligned.is_protocol_violation());

    let duplicate = assembler
        .insert_at(
            0,
            PeerKey::from_raw(2),
            0,
            &first,
            t0 + Duration::from_secs(25),
        )
        .err()
        .expect("duplicate error");
    assert_eq!(
        duplicate,
        PieceError::Block(BlockError::Duplicate { block: 0 })
    );
    assert!(!duplicate.is_protocol_violation());

    let wrong_length = assembler
        .insert_at(
            0,
            PeerKey::from_raw(2),
            block_offset(1),
            &[1, 2, 3],
            t0 + Duration::from_secs(29),
        )
        .err()
        .expect("wrong-length error");
    assert!(wrong_length.is_protocol_violation());

    assert_eq!(
        assembler.last_progress(0),
        Some(t0 + Duration::from_secs(10)),
        "rejected blocks refreshed the stall deadline"
    );
    assert_eq!(
        assembler.sweep_stalled(t0 + Duration::from_secs(40)),
        vec![0],
        "the deadline is measured from the last valid block"
    );
    assert_eq!(torrent.in_use_bytes(), 0);
}

#[test]
fn pieces_expire_independently_in_piece_order() {
    let t0 = Instant::now();
    let len = BLOCK_LEN;
    let torrent = torrent_budget(len);
    let mut assembler = PieceAssembler::new(DEFAULT_STALL_TIMEOUT).expect("assembler");

    start_piece(&mut assembler, &torrent, 0, len, t0);
    start_piece(
        &mut assembler,
        &torrent,
        2,
        len,
        t0 + Duration::from_secs(10),
    );
    assert_eq!(assembler.in_flight(), 2);

    assert_eq!(
        assembler.sweep_stalled(t0 + Duration::from_secs(35)),
        vec![0],
        "piece 0 is 35 seconds idle, piece 2 is 25"
    );
    assert!(assembler.is_in_flight(2));
    assert_eq!(
        assembler.sweep_stalled(t0 + Duration::from_secs(41)),
        vec![2]
    );
    assert_eq!(assembler.in_flight(), 0);
    assert_eq!(torrent.in_use_bytes(), 0);

    start_piece(&mut assembler, &torrent, 5, len, t0);
    start_piece(&mut assembler, &torrent, 3, len, t0);
    assert_eq!(
        assembler.sweep_stalled(t0 + DEFAULT_STALL_TIMEOUT),
        vec![3, 5],
        "expired pieces are reported in ascending order"
    );
}

#[test]
fn starting_with_a_mismatched_reservation_is_a_buffer_error() {
    let t0 = Instant::now();
    let len = 4096;
    let torrent = torrent_budget(len);
    let spec = PieceSpec::new(0, 0, len, HASH).expect("spec");
    let mut assembler = PieceAssembler::new(DEFAULT_STALL_TIMEOUT).expect("assembler");

    let reservation = torrent.try_reserve(len - 1).expect("reservation");
    let error = assembler
        .start_at(spec, reservation, t0)
        .expect_err("reservation mismatch");
    assert_eq!(
        error,
        PieceError::Buffer(PieceBufferError::ReservationMismatch {
            reserved: len - 1,
            needed: len,
        })
    );
    assert!(!error.is_protocol_violation());
    assert_eq!(assembler.in_flight(), 0);
    assert_eq!(torrent.in_use_bytes(), 0);
}

#[test]
fn only_wrapped_block_violations_are_protocol_violations() {
    assert!(!PieceError::NotStarted { piece: 0 }.is_protocol_violation());
    assert!(!PieceError::AlreadyInFlight { piece: 0 }.is_protocol_violation());
    assert!(!PieceError::ZeroStallTimeout.is_protocol_violation());
    assert!(
        !PieceError::Buffer(PieceBufferError::ReservationMismatch {
            reserved: 1,
            needed: 2,
        })
        .is_protocol_violation()
    );
    assert!(!PieceError::Block(BlockError::Duplicate { block: 0 }).is_protocol_violation());
    assert!(
        PieceError::Block(BlockError::WrongLength {
            block: 0,
            expected: BLOCK_LEN,
            found: 1,
        })
        .is_protocol_violation()
    );
    assert!(
        PieceError::Block(BlockError::Misaligned {
            offset: 1,
            block_len: BLOCK_LEN,
        })
        .is_protocol_violation()
    );
    assert!(
        PieceError::Block(BlockError::OutOfRange {
            offset: BLOCK_LEN as u32,
            piece_len: BLOCK_LEN,
        })
        .is_protocol_violation()
    );
}

#[test]
fn public_start_and_insert_use_the_current_time() {
    let len = BLOCK_LEN;
    let torrent = torrent_budget(len);
    let spec = PieceSpec::new(0, 0, len, HASH).expect("spec");
    let mut assembler = PieceAssembler::new(DEFAULT_STALL_TIMEOUT).expect("assembler");

    let reservation = torrent.try_reserve(len).expect("reservation");
    assembler.start(spec, reservation).expect("start");

    let bytes = block_bytes(0, len);
    match assembler
        .insert(0, PeerKey::from_raw(3), 0, &bytes)
        .expect("insert")
    {
        InsertResult::Complete(buffer) => {
            assert_eq!(buffer.complete_bytes(), Some(bytes.as_slice()));
        }
        InsertResult::Incomplete(_) => panic!("single-block piece did not complete"),
    }
    assert_eq!(assembler.in_flight(), 0);
}
