use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Waker};
use std::time::Duration;

use sha1::{Digest, Sha1};
use tokio::time::timeout;

use super::*;
use crate::metainfo::PIECE_HASH_LENGTH;
use crate::piece::{
    BLOCK_LEN, MemoryBudget, PeerKey, PieceBuffer, PieceSpec, TorrentBudget, assert_budget_idle,
    test_spec,
};

const MIB: u64 = 1024 * 1024;
const WAIT: Duration = Duration::from_secs(30);

struct Fixture {
    budget: MemoryBudget,
    torrent: TorrentBudget,
}

impl Fixture {
    fn new(piece_length: u64) -> Self {
        let budget = MemoryBudget::new(128 * MIB).expect("memory budget");
        let torrent = budget.torrent_budget(piece_length).expect("torrent budget");
        Self { budget, torrent }
    }
}

fn test_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| {
            let value = index.wrapping_mul(31).wrapping_add(7) & 0xFF;
            u8::try_from(value).expect("byte")
        })
        .collect()
}

fn hash_of(bytes: &[u8]) -> [u8; PIECE_HASH_LENGTH] {
    let digest = Sha1::digest(bytes);
    let mut hash = [0u8; PIECE_HASH_LENGTH];
    hash.copy_from_slice(&digest);
    hash
}

fn spec(index: u32, offset: u64, hash: [u8; PIECE_HASH_LENGTH], len: usize) -> PieceSpec {
    test_spec(index, offset, len, hash).expect("piece spec")
}

fn fill(torrent: &TorrentBudget, spec: PieceSpec, data: &[u8], peers: &[u64]) -> PieceBuffer {
    let reservation = torrent.try_reserve(data.len()).expect("reservation");
    let mut buffer = PieceBuffer::new(spec, reservation).expect("piece buffer");
    for (index, block) in data.chunks(BLOCK_LEN).enumerate() {
        let offset = u32::try_from(index * BLOCK_LEN).expect("block offset");
        let peer = peers[index % peers.len()];
        buffer
            .insert(PeerKey::from_raw(peer), offset, block)
            .expect("insert block");
    }
    buffer
}

#[tokio::test]
async fn correct_hash_produces_a_verified_piece() {
    let data = test_bytes(2 * BLOCK_LEN + 100);
    let len = data.len();
    let piece = spec(7, 2 * MIB, hash_of(&data), len);
    let fixture = Fixture::new(u64::try_from(len).expect("length"));
    let buffer = fill(&fixture.torrent, piece, &data, &[11]);

    let pool = HashPool::new();
    let verified = pool.verify(buffer).await.expect("verified piece");

    assert_eq!(verified.spec().index(), 7);
    assert_eq!(verified.piece(), 7);
    assert_eq!(verified.offset(), 2 * MIB);
    assert_eq!(verified.length(), len);
    assert_eq!(verified.data(), data.as_slice());
    assert_eq!(
        fixture.torrent.in_use_bytes(),
        u64::try_from(len).expect("length"),
        "the verified piece keeps the memory reservation"
    );
    assert_eq!(
        pool.available_jobs(),
        DEFAULT_MAX_HASH_JOBS,
        "the hash permit is released before the piece is dropped"
    );

    drop(verified);
    assert_budget_idle(&fixture.budget, &fixture.torrent);
}

#[tokio::test]
async fn wrong_hash_returns_the_contributors() {
    let data = test_bytes(3 * BLOCK_LEN);
    let hash = hash_of(b"some other piece");
    let piece = spec(4, 0, hash, data.len());
    let fixture = Fixture::new(u64::try_from(data.len()).expect("length"));
    // Blocks 0 and 2 come from peer 9, block 1 from peer 3.
    let buffer = fill(&fixture.torrent, piece, &data, &[9, 3, 9]);

    let pool = HashPool::new();
    match pool.verify(buffer).await {
        Err(VerifyError::HashMismatch {
            piece: failed,
            expected,
            found,
            contributors,
        }) => {
            assert_eq!(failed, 4);
            assert_eq!(expected, hash);
            assert_eq!(found, hash_of(&data));
            assert_eq!(
                contributors,
                vec![PeerKey::from_raw(3), PeerKey::from_raw(9)]
            );
        }
        other => panic!("expected a hash mismatch, got {other:?}"),
    }

    assert_eq!(
        pool.available_jobs(),
        DEFAULT_MAX_HASH_JOBS,
        "a mismatch releases the hash permit"
    );
    assert_budget_idle(&fixture.budget, &fixture.torrent);
}

#[tokio::test]
async fn incomplete_buffer_is_rejected_without_taking_a_hash_slot() {
    let data = test_bytes(2 * BLOCK_LEN);
    let piece = spec(0, 0, hash_of(&data), data.len());
    let fixture = Fixture::new(u64::try_from(data.len()).expect("length"));
    let reservation = fixture
        .torrent
        .try_reserve(data.len())
        .expect("reservation");
    let mut buffer = PieceBuffer::new(piece, reservation).expect("piece buffer");
    buffer
        .insert(PeerKey::from_raw(1), 0, &data[..BLOCK_LEN])
        .expect("first block");

    let pool = HashPool::with_max_jobs(1).expect("hash pool");
    match pool.verify(buffer).await {
        Err(VerifyError::Incomplete {
            piece: incomplete,
            received,
            blocks,
        }) => {
            assert_eq!(incomplete, 0);
            assert_eq!(received, 1);
            assert_eq!(blocks, 2);
        }
        other => panic!("expected an incomplete piece, got {other:?}"),
    }

    assert_eq!(pool.available_jobs(), 1, "no hash job was started");
    assert_budget_idle(&fixture.budget, &fixture.torrent);
}

#[tokio::test]
async fn short_last_piece_hashes_exactly_the_actual_length() {
    let data = test_bytes(2 * BLOCK_LEN + 17);
    let len = data.len();
    let fixture = Fixture::new(u64::try_from(len).expect("length"));
    let pool = HashPool::new();

    let exact = spec(9, 0, hash_of(&data), len);
    let buffer = fill(&fixture.torrent, exact, &data, &[1]);
    let verified = pool.verify(buffer).await.expect("verified piece");
    assert_eq!(verified.length(), len);
    assert_eq!(verified.data(), data.as_slice());
    drop(verified);

    // A hash over the same bytes padded to a whole block must not match: the
    // last block is 17 bytes and no padding is ever hashed.
    let mut padded = data.clone();
    padded.resize(3 * BLOCK_LEN, 0);
    let padded_piece = spec(9, 0, hash_of(&padded), len);
    let buffer = fill(&fixture.torrent, padded_piece, &data, &[1]);
    match pool.verify(buffer).await {
        Err(VerifyError::HashMismatch { piece, .. }) => assert_eq!(piece, 9),
        other => panic!("padded hash must not verify, got {other:?}"),
    }

    assert_budget_idle(&fixture.budget, &fixture.torrent);
}

#[tokio::test]
async fn hash_cap_is_never_exceeded() {
    let (gate, mut entered) = HashGate::new();
    let pool = Arc::new(HashPool::with_max_jobs(2).expect("hash pool"));
    pool.set_test_gate(Arc::clone(&gate));

    let mut jobs = Vec::new();
    let mut fixtures = Vec::new();
    for peer in 0..5 {
        let data = test_bytes(BLOCK_LEN);
        let piece = spec(0, 0, hash_of(&data), data.len());
        let fixture = Fixture::new(u64::try_from(data.len()).expect("length"));
        let buffer = fill(&fixture.torrent, piece, &data, &[peer]);
        let pool = Arc::clone(&pool);
        jobs.push(tokio::spawn(async move { pool.verify(buffer).await }));
        fixtures.push(fixture);
    }

    for _ in 0..2 {
        timeout(WAIT, entered.recv())
            .await
            .expect("hash job entered the gate")
            .expect("gate entry");
    }
    assert_eq!(gate.peak(), 2, "two jobs hold the two permits");

    gate.open();
    for (job, fixture) in jobs.into_iter().zip(fixtures) {
        let verified = timeout(WAIT, job)
            .await
            .expect("hash job finished")
            .expect("hash task")
            .expect("verified piece");
        drop(verified);
        assert_budget_idle(&fixture.budget, &fixture.torrent);
    }

    assert_eq!(gate.peak(), 2, "the cap was never exceeded");
    assert_eq!(pool.available_jobs(), 2);
}

#[tokio::test]
async fn cancelling_while_waiting_for_a_permit_releases_the_reservation() {
    let (gate, mut entered) = HashGate::new();
    let pool = Arc::new(HashPool::with_max_jobs(1).expect("hash pool"));
    pool.set_test_gate(Arc::clone(&gate));

    let data = test_bytes(BLOCK_LEN);
    let piece = spec(0, 0, hash_of(&data), data.len());
    let first = Fixture::new(u64::try_from(data.len()).expect("length"));
    let second = Fixture::new(u64::try_from(data.len()).expect("length"));
    let first_buffer = fill(&first.torrent, piece, &data, &[1]);
    let second_buffer = fill(&second.torrent, piece, &data, &[2]);

    let first_job = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.verify(first_buffer).await }
    });
    timeout(WAIT, entered.recv())
        .await
        .expect("first job entered the gate")
        .expect("gate entry");

    // The first job holds the only permit and is stuck in the gate. Polling
    // the second future once must leave it waiting for a permit; dropping it
    // (at the end of this block) then cancels that wait deterministically.
    {
        let mut second = pin!(pool.verify(second_buffer));
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            second.as_mut().poll(&mut context).is_pending(),
            "the second verify waits for the only permit"
        );
    }

    assert_budget_idle(&second.budget, &second.torrent);
    assert_eq!(
        first.torrent.in_use_bytes(),
        u64::try_from(data.len()).expect("length")
    );

    gate.open();
    let verified = timeout(WAIT, first_job)
        .await
        .expect("first job finished")
        .expect("first task")
        .expect("verified piece");
    drop(verified);
    assert_budget_idle(&first.budget, &first.torrent);
}

#[tokio::test]
async fn cancelling_after_the_hash_started_releases_the_reservation() {
    let (gate, mut entered) = HashGate::new();
    let pool = Arc::new(HashPool::with_max_jobs(1).expect("hash pool"));
    pool.set_test_gate(Arc::clone(&gate));

    let data = test_bytes(BLOCK_LEN);
    let piece = spec(0, 0, hash_of(&data), data.len());
    let fixture = Fixture::new(u64::try_from(data.len()).expect("length"));
    let buffer = fill(&fixture.torrent, piece, &data, &[1]);

    let job = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.verify(buffer).await }
    });
    timeout(WAIT, entered.recv())
        .await
        .expect("hash job entered the gate")
        .expect("gate entry");

    job.abort();
    let _ = job.await;
    assert_eq!(
        fixture.torrent.in_use_bytes(),
        u64::try_from(data.len()).expect("length"),
        "the blocking job still owns the reservation"
    );

    gate.open();
    timeout(WAIT, async {
        while fixture.torrent.in_use_bytes() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the cancelled job released its reservation");

    assert_eq!(pool.available_jobs(), 1);
    assert_budget_idle(&fixture.budget, &fixture.torrent);
}

#[cfg(panic = "unwind")]
#[tokio::test]
async fn a_panicking_hash_task_releases_the_reservation() {
    let (gate, _entered) = HashGate::new();
    gate.panic_on_enter();
    let pool = HashPool::with_max_jobs(1).expect("hash pool");
    pool.set_test_gate(Arc::clone(&gate));

    let data = test_bytes(BLOCK_LEN);
    let piece = spec(0, 0, hash_of(&data), data.len());
    let fixture = Fixture::new(u64::try_from(data.len()).expect("length"));
    let buffer = fill(&fixture.torrent, piece, &data, &[1]);

    match pool.verify(buffer).await {
        Err(VerifyError::TaskFailed(error)) => assert!(error.is_panic()),
        other => panic!("expected a failed hash task, got {other:?}"),
    }

    assert_budget_idle(&fixture.budget, &fixture.torrent);
    assert_eq!(pool.available_jobs(), 1);
}

#[test]
fn zero_hash_jobs_is_rejected() {
    assert!(matches!(
        HashPool::with_max_jobs(0),
        Err(HashPoolError::ZeroMaxJobs)
    ));
}

#[test]
fn default_pool_allows_two_hash_jobs() {
    let pool = HashPool::new();
    assert_eq!(pool.max_jobs(), DEFAULT_MAX_HASH_JOBS);
    assert_eq!(pool.available_jobs(), DEFAULT_MAX_HASH_JOBS);
}
