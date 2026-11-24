use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use sha1::{Digest, Sha1};
use tempfile::TempDir;

use super::*;
use crate::bitfield::Bitfield;
use crate::metainfo::{FileInfo, InfoHash, MetaInfo, Mode, PIECE_HASH_LENGTH};
use crate::piece::{HashPool, PieceSpecError};
use crate::storage::backend::StorageConfig;
use crate::storage::layout::FileMap;
use crate::storage::thread::{MAX_RANGE_LEN, StorageHandle};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn hash_of(bytes: &[u8]) -> [u8; PIECE_HASH_LENGTH] {
    let digest = Sha1::digest(bytes);
    let mut hash = [0u8; PIECE_HASH_LENGTH];
    hash.copy_from_slice(&digest);
    hash
}

fn test_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| {
            let value = index.wrapping_mul(41).wrapping_add(13) & 0xFF;
            u8::try_from(value).expect("byte")
        })
        .collect()
}

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    meta: Arc<MetaInfo>,
    map: Arc<FileMap>,
    data: Vec<u8>,
}

impl Fixture {
    fn new(files: &[(&[&str], u64)], piece_length: u64) -> Self {
        let total: u64 = files.iter().map(|(_, length)| *length).sum();
        let data = test_bytes(usize::try_from(total).expect("total length"));

        let mut file_infos = Vec::with_capacity(files.len());
        for (components, length) in files {
            let mut path = vec!["t".to_owned()];
            path.extend(components.iter().map(|component| (*component).to_owned()));
            file_infos.push(FileInfo {
                path,
                length: *length,
            });
        }

        let piece_len = usize::try_from(piece_length).expect("piece length");
        let piece_hashes = data.chunks(piece_len).map(hash_of).collect();
        let meta = Arc::new(MetaInfo {
            info_hash: InfoHash::from_bytes([0u8; PIECE_HASH_LENGTH]),
            name: "t".to_owned(),
            mode: Mode::Multi,
            files: file_infos,
            total_length: total,
            piece_length,
            piece_hashes,
            announce: None,
            announce_list: Vec::new(),
            private: false,
        });
        let map = Arc::new(FileMap::build(&meta).expect("file map"));

        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("root");
        fs::create_dir_all(&root).expect("root");
        let fixture = Self {
            _dir: dir,
            root,
            meta,
            map,
            data,
        };
        fixture.write_all_files();
        fixture
    }

    fn write_all_files(&self) {
        for file in self.map.files() {
            let path = self.root.join(file.to_path());
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent directory");
            }
            let start = usize::try_from(file.offset).expect("file offset");
            let end = start + usize::try_from(file.length).expect("file length");
            fs::write(&path, &self.data[start..end]).expect("write file");
        }
    }

    async fn open(&self) -> StorageHandle {
        StorageHandle::open(
            self.root.clone(),
            Arc::clone(&self.map),
            StorageConfig::default(),
        )
        .await
        .expect("open storage")
    }

    fn base(&self) -> Bitfield {
        Bitfield::new(self.map.piece_count()).expect("base bitfield")
    }

    fn rechecker(&self, storage: StorageHandle, pool: Arc<HashPool>) -> Rechecker {
        Rechecker::new(storage, Arc::clone(&self.map), Arc::clone(&self.meta), pool)
            .expect("rechecker")
    }
}

#[tokio::test]
async fn all_valid_pieces_are_present() {
    let fixture = Fixture::new(&[(&["a.bin"], 5 * MIB / 2)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let base = fixture.base();

    let outcome = rechecker
        .run(RecheckSelection::All, &base)
        .await
        .expect("recheck");

    assert_eq!(outcome.checked(), 3);
    assert_eq!(outcome.present(), 3);
    assert_eq!(outcome.absent(), 0);
    assert_eq!(outcome.bits().count_verified(), 3);
    for piece in 0..3 {
        assert!(outcome.bits().get(piece).expect("get piece"));
    }
    assert_eq!(base.count_verified(), 0, "the base bitfield is not mutated");

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn subset_recheck_preserves_unselected_bits() {
    let fixture = Fixture::new(&[(&["a.bin"], 5 * MIB / 2)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let mut base = fixture.base();
    base.mark_rechecked_present(0).expect("base piece 0");
    base.mark_rechecked_present(2).expect("base piece 2");
    let selection = [1u32];

    let outcome = rechecker
        .run(RecheckSelection::Only(&selection), &base)
        .await
        .expect("recheck");

    assert_eq!(outcome.checked(), 1);
    assert_eq!(outcome.present(), 1);
    assert_eq!(outcome.absent(), 0);
    assert!(outcome.bits().get(0).expect("piece 0"));
    assert!(outcome.bits().get(1).expect("piece 1"));
    assert!(outcome.bits().get(2).expect("piece 2"));
    assert_eq!(outcome.bits().count_verified(), 3);
    assert_eq!(base.count_verified(), 2, "the base bitfield is not mutated");

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn last_short_piece_hashes_its_actual_length() {
    let fixture = Fixture::new(&[(&["a.bin"], 2 * MIB + 200 * KIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let outcome = rechecker
        .run(RecheckSelection::All, &fixture.base())
        .await
        .expect("recheck");

    assert_eq!(outcome.checked(), 3);
    assert_eq!(outcome.present(), 3);
    assert_eq!(outcome.absent(), 0);
    assert_eq!(outcome.bits().count_verified(), 3);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn empty_selection_returns_the_base_without_reading() {
    let fixture = Fixture::new(&[(&["a.bin"], MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let mut base = fixture.base();
    base.mark_rechecked_present(0).expect("base piece 0");
    storage
        .fail_read_after(0)
        .await
        .expect("arm a read failure that must not be used");

    let outcome = rechecker
        .run(RecheckSelection::Only(&[]), &base)
        .await
        .expect("empty recheck");

    assert_eq!(outcome.checked(), 0);
    assert_eq!(outcome.present(), 0);
    assert_eq!(outcome.absent(), 0);
    assert_eq!(outcome.bits(), &base);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn zero_pieces_all_returns_the_base() {
    let fixture = Fixture::new(&[(&["empty.bin"], 0)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let base = fixture.base();

    let outcome = rechecker
        .run(RecheckSelection::All, &base)
        .await
        .expect("zero-piece recheck");

    assert_eq!(outcome.checked(), 0);
    assert_eq!(outcome.present(), 0);
    assert_eq!(outcome.absent(), 0);
    assert_eq!(outcome.bits(), &base);
    assert_eq!(outcome.bits().piece_count(), 0);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn batching_uses_one_hash_job_per_batch() {
    let fixture = Fixture::new(&[(&["a.bin"], 2 * MIB)], 2 * MIB);
    let storage = fixture.open().await;
    let pool = Arc::new(HashPool::new());
    let rechecker = fixture.rechecker(storage.clone(), Arc::clone(&pool));

    let outcome = rechecker
        .run(RecheckSelection::All, &fixture.base())
        .await
        .expect("recheck");

    assert_eq!(outcome.present(), 1);
    assert_eq!(
        pool.started_jobs(),
        2,
        "2 MiB at 1 MiB per batch uses two jobs, not eight chunks"
    );
    assert_eq!(pool.active_jobs(), 0);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[test]
fn chunk_constants_fit_the_storage_limit() {
    assert_eq!(RECHECK_CHUNK_LEN, 256 * 1024);
    assert_eq!(RECHECK_CHUNKS_PER_JOB, 4);
    assert_eq!(RECHECK_BATCH_LEN, 1024 * 1024);
    const { assert!(RECHECK_CHUNK_LEN <= MAX_RANGE_LEN) };
    const { assert!(RECHECK_BATCH_LEN <= MAX_RANGE_LEN) };
}

#[tokio::test]
async fn selection_validation_is_typed() {
    let fixture = Fixture::new(&[(&["a.bin"], 5 * MIB / 2)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let base = fixture.base();

    let unordered = [2u32, 1];
    match rechecker
        .run(RecheckSelection::Only(&unordered), &base)
        .await
    {
        Err(RecheckError::SelectionNotSorted {
            previous: 2,
            current: 1,
        }) => {}
        other => panic!("expected an unsorted selection error, got {other:?}"),
    }

    let duplicate = [1u32, 1];
    match rechecker
        .run(RecheckSelection::Only(&duplicate), &base)
        .await
    {
        Err(RecheckError::SelectionNotSorted {
            previous: 1,
            current: 1,
        }) => {}
        other => panic!("expected a duplicate selection error, got {other:?}"),
    }

    let out_of_range = [3u32];
    match rechecker
        .run(RecheckSelection::Only(&out_of_range), &base)
        .await
    {
        Err(RecheckError::SelectionOutOfRange {
            piece: 3,
            piece_count: 3,
        }) => {}
        other => panic!("expected an out-of-range selection error, got {other:?}"),
    }

    let short_base = Bitfield::new(2).expect("short bitfield");
    match rechecker.run(RecheckSelection::All, &short_base).await {
        Err(RecheckError::PieceCountMismatch {
            expected: 3,
            found: 2,
        }) => {}
        other => panic!("expected a piece count mismatch, got {other:?}"),
    }

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn new_rejects_mismatched_metainfo() {
    let fixture = Fixture::new(&[(&["a.bin"], MIB)], MIB);
    let storage = fixture.open().await;
    let mut meta = (*fixture.meta).clone();
    meta.total_length += 1;
    let meta = Arc::new(meta);

    match Rechecker::new(
        storage.clone(),
        Arc::clone(&fixture.map),
        meta,
        Arc::new(HashPool::new()),
    ) {
        Err(RecheckError::Spec(PieceSpecError::TotalLengthMismatch { .. })) => {}
        Err(other) => panic!("expected a total length mismatch, got {other:?}"),
        Ok(_) => panic!("expected a total length mismatch, got a rechecker"),
    }

    storage.shutdown().await.expect("shutdown");
}

#[test]
fn rechecker_is_send_and_sync() {
    fn assert_traits<T: Send + Sync>() {}
    assert_traits::<Rechecker>();
}
