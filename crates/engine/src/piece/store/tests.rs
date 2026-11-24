use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

use sha1::{Digest, Sha1};
use tempfile::TempDir;

use super::*;
use crate::bitfield::{Bitfield, BitfieldError};
use crate::metainfo::PIECE_HASH_LENGTH;
use crate::piece::{
    BLOCK_LEN, HashPool, MemoryBudget, PeerKey, PieceBuffer, TorrentBudget, VerifiedPiece,
    assert_budget_idle, test_spec,
};
use crate::storage::backend::{StorageConfig, StorageError};
use crate::storage::layout::{FileMap, MappedFile};
use crate::storage::thread::StorageHandleError;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn temp_dir() -> TempDir {
    TempDir::new().expect("create temp dir")
}

fn root_in(dir: &TempDir) -> PathBuf {
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("create root");
    root
}

fn map_pieces(files: &[(&[&str], u64)], piece_length: u64, piece_count: u32) -> Arc<FileMap> {
    let mut offset = 0u64;
    let mapped: Vec<MappedFile> = files
        .iter()
        .map(|(components, length)| {
            let disk_components: Vec<String> = components
                .iter()
                .map(|component| (*component).to_owned())
                .collect();
            let file = MappedFile {
                original_path: disk_components.clone(),
                disk_components,
                length: *length,
                offset,
                pieces: 0..0,
            };
            offset += *length;
            file
        })
        .collect();

    Arc::new(FileMap::from_mapped_files(
        mapped,
        offset,
        piece_length,
        piece_count,
    ))
}

fn map_with(files: &[(&[&str], u64)]) -> Arc<FileMap> {
    map_pieces(files, 1, 0)
}

async fn open_storage(root: &Path, map: Arc<FileMap>) -> StorageHandle {
    StorageHandle::open(root.to_path_buf(), map, StorageConfig::default())
        .await
        .expect("open storage")
}

fn test_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| {
            let value = index.wrapping_mul(37).wrapping_add(11) & 0xFF;
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

struct Verified {
    piece: VerifiedPiece,
    budget: MemoryBudget,
    torrent: TorrentBudget,
}

async fn verified_piece(
    index: u32,
    offset: u64,
    data: &[u8],
    declared_piece_length: u64,
) -> Verified {
    verify_piece(index, offset, data, declared_piece_length, hash_of(data))
        .await
        .expect("verified piece")
}

async fn verify_piece(
    index: u32,
    offset: u64,
    data: &[u8],
    declared_piece_length: u64,
    hash: [u8; PIECE_HASH_LENGTH],
) -> Result<Verified, crate::piece::VerifyError> {
    let budget = MemoryBudget::new(128 * MIB).expect("memory budget");
    let torrent = budget
        .torrent_budget(declared_piece_length)
        .expect("torrent budget");
    let spec = test_spec(index, offset, data.len(), hash).expect("piece spec");
    let reservation = torrent.try_reserve(data.len()).expect("reservation");
    let mut buffer = PieceBuffer::new(spec, reservation).expect("piece buffer");
    for (block_index, block) in data.chunks(BLOCK_LEN).enumerate() {
        let block_offset = u32::try_from(block_index * BLOCK_LEN).expect("block offset");
        buffer
            .insert(PeerKey::from_raw(1), block_offset, block)
            .expect("insert block");
    }

    let pool = HashPool::new();
    let piece = pool.verify(buffer).await?;
    Ok(Verified {
        piece,
        budget,
        torrent,
    })
}

async fn read_all(storage: &StorageHandle, offset: u64, len: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(len);
    let mut position = offset;
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(MAX_RANGE_LEN);
        let read = storage
            .read_range(position, chunk)
            .await
            .expect("read range");
        assert_eq!(read.len(), chunk, "unexpected short read");
        bytes.extend_from_slice(&read);
        position += u64::try_from(chunk).expect("chunk length");
        remaining -= chunk;
    }
    bytes
}

/// Polls the store future once, then drops it, simulating cancellation while
/// the first chunk is in flight.
async fn poll_store_once(storage: &StorageHandle, verified: &VerifiedPiece) {
    let mut future = pin!(store_verified(storage, verified));
    let mut polled = false;
    std::future::poll_fn(|context| {
        if !polled {
            polled = true;
            let _ = future.as_mut().poll(context);
        }
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn two_and_a_half_mib_piece_round_trips() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let len = usize::try_from(5 * MIB / 2).expect("piece length");
    let map = map_pieces(&[(&["a.bin"], 5 * MIB / 2)], 5 * MIB / 2, 1);
    let storage = open_storage(&root, map).await;

    let data = test_bytes(len);
    let verified = verified_piece(0, 0, &data, 5 * MIB / 2).await;

    let stored = store_verified(&storage, &verified.piece)
        .await
        .expect("store piece");
    assert_eq!(stored.piece(), 0);
    assert_eq!(stored.offset(), 0);
    assert_eq!(stored.length(), len);
    assert_eq!(stored.spec().hash(), hash_of(&data));

    assert_eq!(read_all(&storage, 0, len).await, data);
    assert_eq!(
        verified.torrent.in_use_bytes(),
        u64::try_from(len).expect("length"),
        "the caller still holds the piece reservation"
    );

    drop(verified.piece);
    assert_budget_idle(&verified.budget, &verified.torrent);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn second_chunk_failure_keeps_the_piece_for_a_retry() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let len = usize::try_from(5 * MIB / 2).expect("piece length");
    let map = map_pieces(&[(&["a.bin"], 5 * MIB / 2)], 5 * MIB / 2, 1);
    let storage = open_storage(&root, map).await;

    let data = test_bytes(len);
    let verified = verified_piece(0, 0, &data, 5 * MIB / 2).await;

    storage
        .fail_write_after(1)
        .await
        .expect("arm write failure");
    let error = store_verified(&storage, &verified.piece)
        .await
        .expect_err("the second chunk must fail");
    match &error {
        WriteError::Chunk {
            piece,
            chunk_index,
            offset,
            len: failed_len,
            source,
        } => {
            assert_eq!(*piece, 0);
            assert_eq!(*chunk_index, 1);
            assert_eq!(*offset, MIB);
            assert_eq!(*failed_len, usize::try_from(MIB).expect("mib"));
            assert!(matches!(
                source,
                StorageHandleError::Storage(StorageError::DiskFull { .. })
            ));
        }
        other => panic!("expected a chunk error, got {other:?}"),
    }
    assert!(error.is_disk_full());
    assert_eq!(
        verified.torrent.in_use_bytes(),
        u64::try_from(len).expect("length"),
        "a failed store keeps the piece reservation with the caller"
    );

    let stored = store_verified(&storage, &verified.piece)
        .await
        .expect("retry with the same piece");
    assert_eq!(stored.length(), len);
    assert_eq!(read_all(&storage, 0, len).await, data);

    drop(verified.piece);
    assert_budget_idle(&verified.budget, &verified.torrent);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_cancelled_store_leaves_the_piece_usable() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let len = usize::try_from(5 * MIB / 2).expect("piece length");
    let map = map_pieces(&[(&["a.bin"], 5 * MIB / 2)], 5 * MIB / 2, 1);
    let storage = open_storage(&root, map).await;

    let data = test_bytes(len);
    let verified = verified_piece(0, 0, &data, 5 * MIB / 2).await;

    poll_store_once(&storage, &verified.piece).await;
    assert_eq!(
        verified.torrent.in_use_bytes(),
        u64::try_from(len).expect("length"),
        "the caller still holds the piece after cancellation"
    );

    let stored = store_verified(&storage, &verified.piece)
        .await
        .expect("fresh store after cancellation");
    assert_eq!(stored.length(), len);
    assert_eq!(read_all(&storage, 0, len).await, data);

    drop(verified.piece);
    assert_budget_idle(&verified.budget, &verified.torrent);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn an_out_of_range_write_is_a_typed_error() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 100)]);
    let storage = open_storage(&root, map).await;

    let data = test_bytes(BLOCK_LEN);
    let verified = verified_piece(
        0,
        10 * MIB,
        &data,
        u64::try_from(data.len()).expect("length"),
    )
    .await;

    let error = store_verified(&storage, &verified.piece)
        .await
        .expect_err("a write beyond the torrent must fail");
    match &error {
        WriteError::Chunk {
            piece,
            chunk_index,
            offset,
            len,
            source,
        } => {
            assert_eq!(*piece, 0);
            assert_eq!(*chunk_index, 0);
            assert_eq!(*offset, 10 * MIB);
            assert_eq!(*len, BLOCK_LEN);
            assert!(matches!(
                source,
                StorageHandleError::Storage(StorageError::OutOfRange { .. })
            ));
        }
        other => panic!("expected a chunk error, got {other:?}"),
    }
    assert!(!error.is_disk_full());
    assert_eq!(fs::read_dir(&root).expect("read root").count(), 0);

    drop(verified.piece);
    assert_budget_idle(&verified.budget, &verified.torrent);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn piece_spanning_several_files_round_trips() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let first = 300 * KIB;
    let second = 500 * KIB;
    let third = 700 * KIB;
    let total = first + second + third;
    let map = map_pieces(
        &[
            (&["a.bin"], first),
            (&["b.bin"], second),
            (&["c.bin"], third),
        ],
        total,
        1,
    );
    let storage = open_storage(&root, map).await;

    let data = test_bytes(usize::try_from(total).expect("total length"));
    let verified = verified_piece(0, 0, &data, total).await;
    store_verified(&storage, &verified.piece)
        .await
        .expect("store piece");

    assert_eq!(read_all(&storage, 0, data.len()).await, data);
    assert_eq!(
        fs::read(root.join("a.bin")).expect("read a"),
        &data[..usize::try_from(first).expect("first")]
    );
    assert_eq!(
        fs::read(root.join("b.bin")).expect("read b"),
        &data[usize::try_from(first).expect("first")
            ..usize::try_from(first + second).expect("second")]
    );
    assert_eq!(
        fs::read(root.join("c.bin")).expect("read c"),
        &data[usize::try_from(first + second).expect("second")..]
    );

    drop(verified.piece);
    assert_budget_idle(&verified.budget, &verified.torrent);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn last_short_piece_round_trips() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let last_len = usize::try_from(200 * KIB).expect("last piece");
    let total = 2 * MIB + 200 * KIB;
    let map = map_pieces(&[(&["a.bin"], total)], MIB, 3);
    let storage = open_storage(&root, map).await;

    let data = test_bytes(last_len);
    let verified = verified_piece(2, 2 * MIB, &data, MIB).await;
    let stored = store_verified(&storage, &verified.piece)
        .await
        .expect("store last piece");
    assert_eq!(stored.piece(), 2);
    assert_eq!(stored.offset(), 2 * MIB);
    assert_eq!(stored.length(), last_len);

    assert_eq!(read_all(&storage, 2 * MIB, last_len).await, data);

    drop(verified.piece);
    assert_budget_idle(&verified.budget, &verified.torrent);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn bits_are_set_only_after_a_hash_match_and_a_complete_write() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let len = usize::try_from(5 * MIB / 2).expect("piece length");
    let map = map_pieces(&[(&["a.bin"], 5 * MIB / 2)], 5 * MIB / 2, 1);
    let storage = open_storage(&root, map).await;
    let data = test_bytes(len);
    let mut bitfield = Bitfield::new(1).expect("bitfield");

    // A hash mismatch never produces a VerifiedPiece, so there is nothing to
    // store and the bit stays clear.
    let mismatch = verify_piece(0, 0, &data, 5 * MIB / 2, hash_of(b"wrong piece")).await;
    assert!(mismatch.is_err());
    assert!(!bitfield.get(0).expect("get piece 0"));

    // A complete hash match with a failed second chunk still leaves the bit
    // clear: StoredPiece is only built after every chunk succeeds.
    let verified = verified_piece(0, 0, &data, 5 * MIB / 2).await;
    storage
        .fail_write_after(1)
        .await
        .expect("arm write failure");
    let error = store_verified(&storage, &verified.piece)
        .await
        .expect_err("the second chunk must fail");
    assert!(error.is_disk_full());
    assert!(
        !bitfield.get(0).expect("get piece 0"),
        "a failed chunk must not set the bit"
    );

    let stored = store_verified(&storage, &verified.piece)
        .await
        .expect("retry with the same piece");
    bitfield.mark_stored(&stored).expect("mark stored");
    assert!(bitfield.get(0).expect("get piece 0"));
    assert_eq!(bitfield.count_verified(), 1);
    assert_eq!(read_all(&storage, 0, len).await, data);

    drop(verified.piece);
    assert_budget_idle(&verified.budget, &verified.torrent);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn mark_stored_sets_only_its_piece_and_checks_the_range() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let piece_len = usize::try_from(MIB).expect("piece length");
    let map = map_pieces(&[(&["a.bin"], 2 * MIB)], MIB, 2);
    let storage = open_storage(&root, map).await;

    let data = test_bytes(piece_len);
    let verified = verified_piece(1, MIB, &data, MIB).await;
    let stored = store_verified(&storage, &verified.piece)
        .await
        .expect("store piece 1");

    let mut bitfield = Bitfield::new(2).expect("bitfield");
    bitfield.mark_stored(&stored).expect("mark stored");
    assert!(!bitfield.get(0).expect("get piece 0"));
    assert!(bitfield.get(1).expect("get piece 1"));
    assert_eq!(bitfield.count_verified(), 1);

    let mut too_small = Bitfield::new(1).expect("bitfield");
    assert_eq!(
        too_small.mark_stored(&stored),
        Err(BitfieldError::OutOfRange {
            piece: 1,
            piece_count: 1,
        })
    );
    assert_eq!(too_small.count_verified(), 0);

    drop(verified.piece);
    assert_budget_idle(&verified.budget, &verified.torrent);
    storage.shutdown().await.expect("shutdown");
}
