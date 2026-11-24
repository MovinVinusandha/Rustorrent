use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::timeout;

use super::*;
use crate::metainfo::MAX_PIECE_LENGTH;
use crate::piece::assert_budget_idle;

const MIB: u64 = 1024 * 1024;

fn memory_budget(mib: u64) -> MemoryBudget {
    MemoryBudget::new(mib * MIB).expect("memory budget")
}

fn torrent(budget: &MemoryBudget, piece_mib: u64) -> TorrentBudget {
    budget
        .torrent_budget(piece_mib * MIB)
        .expect("torrent budget")
}

fn bytes(mib: u64) -> usize {
    usize::try_from(mib * MIB).expect("usize bytes")
}

#[test]
fn default_constants_match_the_adr() {
    assert_eq!(DEFAULT_GLOBAL_BUDGET, 128 * MIB);
    assert_eq!(MAX_TORRENT_SHARE, 64 * MIB);
}

#[test]
fn memory_budget_rejects_invalid_capacity() {
    assert_eq!(MemoryBudget::new(0).err(), Some(BudgetError::ZeroCapacity));

    let too_large = Semaphore::MAX_PERMITS as u64 + 1;
    assert!(matches!(
        MemoryBudget::new(too_large),
        Err(BudgetError::CapacityTooLarge { capacity }) if capacity == too_large
    ));
}

#[test]
fn torrent_budget_share_and_slots_follow_the_adr() {
    let budget = memory_budget(128);
    let cases = [
        (1u64, 2u64, 2usize),
        (16, 32, 2),
        (32, 64, 2),
        (33, 64, 1),
        (40, 64, 1),
        (64, 64, 1),
    ];

    for (piece_mib, share_mib, slots) in cases {
        let torrent = torrent(&budget, piece_mib);
        assert_eq!(torrent.piece_length(), piece_mib * MIB);
        assert_eq!(torrent.share(), share_mib * MIB, "piece={piece_mib}");
        assert_eq!(torrent.max_in_flight(), slots, "piece={piece_mib}");
        assert_eq!(torrent.in_flight(), 0);
        assert_eq!(torrent.in_use_bytes(), 0);
    }
}

#[test]
fn torrent_budget_rejects_bad_piece_lengths() {
    let budget = memory_budget(128);
    assert_eq!(
        budget.torrent_budget(0).err(),
        Some(BudgetError::ZeroPieceLength)
    );
    assert!(matches!(
        budget.torrent_budget(MAX_PIECE_LENGTH + 1),
        Err(BudgetError::PieceTooLarge {
            piece_length,
            max,
        }) if piece_length == MAX_PIECE_LENGTH + 1 && max == MAX_PIECE_LENGTH
    ));

    let small = memory_budget(1);
    assert!(matches!(
        small.torrent_budget(2 * MIB),
        Err(BudgetError::GlobalTooSmall {
            piece_length,
            global,
        }) if piece_length == 2 * MIB && global == MIB
    ));
}

#[tokio::test]
async fn reserve_and_drop_releases_global_and_torrent_bytes() {
    let budget = memory_budget(8);
    let torrent = torrent(&budget, 1);

    let reservation = torrent.reserve(bytes(1)).await.expect("reserve");
    assert_eq!(reservation.bytes(), bytes(1));
    assert_eq!(budget.in_use_bytes(), MIB);
    assert_eq!(budget.available_bytes(), 7 * MIB);
    assert_eq!(torrent.in_use_bytes(), MIB);
    assert_eq!(torrent.in_flight(), 1);

    drop(reservation);
    assert_eq!(budget.available_bytes(), 8 * MIB);
    assert_budget_idle(&budget, &torrent);
}

#[tokio::test]
async fn actual_piece_length_can_be_shorter_than_piece_length() {
    let budget = memory_budget(8);
    let torrent = torrent(&budget, 1);

    let reservation = torrent.reserve(4096).await.expect("reserve short piece");
    assert_eq!(reservation.bytes(), 4096);
    assert_eq!(torrent.in_use_bytes(), 4096);
    drop(reservation);
    assert_budget_idle(&budget, &torrent);
}

#[tokio::test]
async fn invalid_reservations_are_rejected_without_waiting() {
    let budget = memory_budget(8);
    let torrent = torrent(&budget, 1);

    assert_eq!(torrent.try_reserve(0).err(), Some(BudgetError::ZeroLength));
    assert_eq!(
        torrent.try_reserve(bytes(3)).err(),
        Some(BudgetError::ExceedsPieceLength {
            requested: bytes(3),
            piece_length: MIB,
        })
    );

    assert_budget_idle(&budget, &torrent);
}

#[test]
fn a_reservation_larger_than_one_piece_length_is_rejected() {
    let budget = memory_budget(8);
    let torrent = torrent(&budget, 1);

    // The old cap was the share (two piece lengths), which let one buffer
    // reserve twice the memory it can actually use.
    for requested in [bytes(2), bytes(3)] {
        assert_eq!(
            torrent.try_reserve(requested).err(),
            Some(BudgetError::ExceedsPieceLength {
                requested,
                piece_length: MIB,
            })
        );
    }
    assert_budget_idle(&budget, &torrent);
}

#[test]
fn failed_try_reserve_releases_the_slot_it_acquired() {
    let budget = memory_budget(1);
    let torrent = torrent(&budget, 1);

    let first = torrent.try_reserve(bytes(1)).expect("first reservation");
    assert_eq!(torrent.in_flight(), 1);

    assert_eq!(
        torrent.try_reserve(bytes(1)).err(),
        Some(BudgetError::WouldBlock)
    );
    assert_eq!(torrent.in_flight(), 1, "failed try leaked a slot");
    assert_eq!(torrent.in_use_bytes(), MIB);

    drop(first);
    let second = torrent.try_reserve(bytes(1)).expect("slot after release");
    assert_eq!(torrent.in_flight(), 1);
    drop(second);
    assert_budget_idle(&budget, &torrent);
}

#[tokio::test]
async fn global_budget_blocks_until_a_reservation_is_dropped() {
    let budget = memory_budget(2);
    let first = torrent(&budget, 1);
    let second = torrent(&budget, 1);

    let one = first.reserve(bytes(1)).await.expect("first");
    let two = first.reserve(bytes(1)).await.expect("second");
    assert_eq!(budget.in_use_bytes(), 2 * MIB);
    assert_eq!(
        second.try_reserve(bytes(1)).err(),
        Some(BudgetError::WouldBlock)
    );

    let mut pending = Box::pin(second.reserve(bytes(1)));
    // A zero deadline polls the reservation once: it must stay pending, which
    // proves it is waiting for global bytes without relying on wall-clock time.
    let early = timeout(Duration::ZERO, &mut pending).await;
    assert!(early.is_err(), "reservation should wait for global bytes");

    drop(one);
    let third = timeout(Duration::from_secs(30), pending)
        .await
        .expect("reservation should finish after a release")
        .expect("reserve");
    assert_eq!(second.in_flight(), 1);
    assert_eq!(budget.in_use_bytes(), 2 * MIB);

    drop(third);
    drop(two);
    assert_budget_idle(&budget, &second);
}

#[tokio::test]
async fn torrent_slots_limit_a_third_buffer() {
    let budget = memory_budget(128);
    let torrent = torrent(&budget, 1);

    let one = torrent.reserve(bytes(1)).await.expect("first");
    let two = torrent.reserve(bytes(1)).await.expect("second");
    assert_eq!(torrent.in_flight(), 2);

    let mut pending = Box::pin(torrent.reserve(bytes(1)));
    let early = timeout(Duration::from_millis(50), &mut pending).await;
    assert!(early.is_err(), "third buffer should wait for a slot");

    drop(one);
    let three = timeout(Duration::from_secs(30), pending)
        .await
        .expect("slot should become available")
        .expect("reserve");
    assert_eq!(torrent.in_flight(), 2);

    drop(three);
    drop(two);
    assert_budget_idle(&budget, &torrent);
}

#[tokio::test]
async fn cancelling_a_global_wait_releases_the_acquired_slot() {
    let budget = memory_budget(1);
    let torrent = torrent(&budget, 1);

    let held = torrent.reserve(bytes(1)).await.expect("held");
    assert_eq!(torrent.in_flight(), 1);

    let mut pending = Box::pin(torrent.reserve(bytes(1)));
    let early = timeout(Duration::from_millis(50), &mut pending).await;
    assert!(early.is_err(), "second reservation should wait for bytes");
    drop(pending);

    assert_eq!(torrent.in_flight(), 1, "cancelled reserve leaked a slot");
    assert_eq!(torrent.in_use_bytes(), MIB);
    assert_eq!(budget.in_use_bytes(), MIB);

    drop(held);
    assert_budget_idle(&budget, &torrent);
}

#[tokio::test]
async fn cancelling_a_slot_wait_leaks_nothing() {
    let budget = memory_budget(128);
    // A 64 MiB piece has exactly one slot, so the second request waits on the
    // slot semaphore while global bytes are still available.
    let torrent = torrent(&budget, 64);
    let held = torrent.reserve(bytes(64)).await.expect("held");
    assert_eq!(torrent.in_flight(), 1);

    let mut pending = Box::pin(torrent.reserve(bytes(1)));
    let early = timeout(Duration::from_millis(50), &mut pending).await;
    assert!(early.is_err(), "second buffer should wait for the slot");
    assert_eq!(budget.in_use_bytes(), 64 * MIB);

    drop(pending);
    assert_eq!(torrent.in_flight(), 1, "cancelled slot wait leaked a slot");
    assert_eq!(torrent.in_use_bytes(), 64 * MIB);

    drop(held);
    assert_budget_idle(&budget, &torrent);
}

#[tokio::test]
async fn a_huge_piece_fits_twice_and_never_deadlocks() {
    let budget = memory_budget(128);
    let first = torrent(&budget, 64);
    let second = torrent(&budget, 64);
    let third = torrent(&budget, 64);

    assert_eq!(first.share(), 64 * MIB);
    assert_eq!(first.max_in_flight(), 1);

    let one = first.reserve(bytes(64)).await.expect("first huge piece");
    assert_eq!(
        first.try_reserve(bytes(64)).err(),
        Some(BudgetError::WouldBlock),
        "a 64 MiB piece must have exactly one slot"
    );
    let two = second.reserve(bytes(64)).await.expect("second huge piece");
    assert_eq!(budget.in_use_bytes(), 128 * MIB);

    assert_eq!(
        third.try_reserve(bytes(64)).err(),
        Some(BudgetError::WouldBlock)
    );
    let mut pending = Box::pin(third.reserve(bytes(64)));
    let early = timeout(Duration::from_millis(50), &mut pending).await;
    assert!(early.is_err(), "third huge piece should wait");

    drop(one);
    let three = timeout(Duration::from_secs(30), pending)
        .await
        .expect("third huge piece should proceed after a release")
        .expect("reserve");
    assert_eq!(budget.in_use_bytes(), 128 * MIB);

    drop(three);
    drop(two);
    assert_budget_idle(&budget, &first);
    assert_budget_idle(&budget, &second);
}

#[tokio::test]
async fn clones_share_the_same_torrent_limits() {
    let budget = memory_budget(8);
    let torrent = torrent(&budget, 1);
    let clone = torrent.clone();

    let reservation = clone.reserve(bytes(1)).await.expect("reserve");
    assert_eq!(torrent.in_use_bytes(), MIB);
    assert_eq!(torrent.in_flight(), 1);
    assert_eq!(clone.in_use_bytes(), MIB);
    assert_eq!(clone.in_flight(), 1);

    drop(reservation);
    assert_budget_idle(&budget, &torrent);
    assert_eq!(clone.in_flight(), 0);
}
