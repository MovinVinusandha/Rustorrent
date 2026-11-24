use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use sha1::{Digest, Sha1};
use tempfile::TempDir;
use tokio::sync::mpsc;

use super::*;
use crate::bitfield::Bitfield;
use crate::metainfo::{FileInfo, InfoHash, MetaInfo, Mode, PIECE_HASH_LENGTH};
use crate::piece::verify::HashGate;
use crate::piece::{HashPool, PieceSpecError};
use crate::storage::backend::{StorageConfig, StorageError, UnsafeReason};
use crate::storage::layout::FileMap;
use crate::storage::thread::{MAX_RANGE_LEN, StorageHandle, StorageHandleError};

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

    fn file_path(&self, index: usize) -> PathBuf {
        let file = self.map.files().get(index).expect("file index");
        self.root.join(file.to_path())
    }

    fn temp_path(&self, name: &str) -> PathBuf {
        self._dir.path().join(name)
    }

    fn flip_byte(&self, file: usize, offset: u64) {
        let path = self.file_path(file);
        let mut data = fs::read(&path).expect("read file");
        let index = usize::try_from(offset).expect("in-file offset");
        data[index] ^= 0xFF;
        fs::write(&path, &data).expect("write file");
    }

    fn truncate_file(&self, file: usize, len: u64) {
        let path = self.file_path(file);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open file")
            .set_len(len)
            .expect("truncate file");
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
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
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
        .run(
            RecheckSelection::Only(&selection),
            &base,
            &RecheckCancel::new(),
            None,
        )
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
async fn damaged_piece_is_absent_and_loses_its_base_bit() {
    let fixture = Fixture::new(&[(&["a.bin"], 3 * MIB)], MIB);
    fixture.flip_byte(0, MIB + 123);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let mut base = fixture.base();
    for piece in 0..3 {
        base.mark_rechecked_present(piece).expect("base bit");
    }

    let outcome = rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
        .expect("recheck");

    assert_eq!(outcome.checked(), 3);
    assert_eq!(outcome.present(), 2);
    assert_eq!(outcome.absent(), 1);
    assert!(outcome.bits().get(0).expect("piece 0"));
    assert!(!outcome.bits().get(1).expect("piece 1"));
    assert!(outcome.bits().get(2).expect("piece 2"));
    assert_eq!(outcome.bits().count_verified(), 2);
    assert_eq!(base.count_verified(), 3, "the base bitfield is not mutated");

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn unselected_damaged_piece_keeps_its_base_bit() {
    let fixture = Fixture::new(&[(&["a.bin"], 3 * MIB)], MIB);
    fixture.flip_byte(0, 123);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let mut base = fixture.base();
    base.mark_rechecked_present(0).expect("base piece 0");
    base.mark_rechecked_present(1).expect("base piece 1");

    let selection = [1u32];
    let outcome = rechecker
        .run(
            RecheckSelection::Only(&selection),
            &base,
            &RecheckCancel::new(),
            None,
        )
        .await
        .expect("recheck");

    assert_eq!(outcome.checked(), 1);
    assert_eq!(outcome.present(), 1);
    assert_eq!(outcome.absent(), 0);
    assert!(
        outcome.bits().get(0).expect("piece 0"),
        "an unselected damaged piece keeps its base bit"
    );
    assert!(outcome.bits().get(1).expect("piece 1"));
    assert_eq!(outcome.bits().count_verified(), 2);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn missing_file_makes_its_pieces_absent() {
    let fixture = Fixture::new(&[(&["a.bin"], MIB), (&["b.bin"], MIB)], MIB);
    fs::remove_file(fixture.file_path(1)).expect("remove second file");
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let mut base = fixture.base();
    base.mark_rechecked_present(0).expect("base piece 0");
    base.mark_rechecked_present(1).expect("base piece 1");

    let outcome = rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
        .expect("recheck");

    assert_eq!(outcome.checked(), 2);
    assert_eq!(outcome.present(), 1);
    assert_eq!(outcome.absent(), 1);
    assert!(outcome.bits().get(0).expect("piece 0"));
    assert!(!outcome.bits().get(1).expect("piece 1"));
    assert_eq!(outcome.bits().count_verified(), 1);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn short_read_abandons_the_piece_without_reading_remaining_chunks() {
    let fixture = Fixture::new(&[(&["a.bin"], 2 * MIB)], 2 * MIB);
    fixture.truncate_file(0, 300 * KIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    // The first read succeeds and the second returns 44 KiB of the 256 KiB it
    // asked for. A third read would hit the injected failure, so this passes
    // only if the piece is abandoned instead of reading the rest of the batch.
    storage.fail_read_after(2).await.expect("arm read failure");

    let selection = [0u32];
    let outcome = rechecker
        .run(
            RecheckSelection::Only(&selection),
            &fixture.base(),
            &RecheckCancel::new(),
            None,
        )
        .await
        .expect("recheck");

    assert_eq!(outcome.checked(), 1);
    assert_eq!(outcome.present(), 0);
    assert_eq!(outcome.absent(), 1);
    assert_eq!(outcome.bits().count_verified(), 0);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn storage_error_stops_the_run_and_is_typed() {
    let fixture = Fixture::new(&[(&["a.bin"], 2 * MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let mut base = fixture.base();
    base.mark_rechecked_present(0).expect("base piece 0");
    base.mark_rechecked_present(1).expect("base piece 1");

    storage.fail_read_after(1).await.expect("arm read failure");

    match rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
    {
        Err(RecheckError::Storage {
            piece: 0,
            offset: 262_144,
            source: StorageHandleError::Storage(StorageError::PermissionDenied { .. }),
        }) => {}
        other => panic!("expected a typed permission error, got {other:?}"),
    }
    assert_eq!(
        base.count_verified(),
        2,
        "a failed run must not publish a bitfield"
    );

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_swap_is_a_security_error_not_an_absent_piece() {
    let fixture = Fixture::new(&[(&["a.bin"], MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let path = fixture.file_path(0);
    let outside = fixture.temp_path("outside.bin");
    fs::write(&outside, vec![0u8; usize::try_from(MIB).expect("length")]).expect("write outside");
    fs::remove_file(&path).expect("remove file");
    std::os::unix::fs::symlink(&outside, &path).expect("create symlink");

    match rechecker
        .run(
            RecheckSelection::All,
            &fixture.base(),
            &RecheckCancel::new(),
            None,
        )
        .await
    {
        Err(RecheckError::Storage {
            piece: 0,
            offset: 0,
            source:
                StorageHandleError::Storage(StorageError::UnsafePath {
                    reason: UnsafeReason::Symlink,
                    ..
                }),
        }) => {}
        other => panic!("expected an unsafe path error, got {other:?}"),
    }

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn cancelled_before_start_reads_nothing_and_releases_the_lease() {
    let fixture = Fixture::new(&[(&["a.bin"], MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let base = fixture.base();
    let cancel = RecheckCancel::new();
    cancel.cancel();

    // A read now would fail. The cancelled run must not read, and the dummy
    // read below proves the one-shot hook is still armed.
    storage.fail_read_after(0).await.expect("arm read failure");

    match rechecker
        .run(RecheckSelection::All, &base, &cancel, None)
        .await
    {
        Err(RecheckError::Cancelled) => {}
        other => panic!("expected cancellation, got {other:?}"),
    }
    match storage.read_range(0, 1).await {
        Err(StorageHandleError::Storage(StorageError::PermissionDenied { .. })) => {}
        other => panic!("the cancelled run must not read, got {other:?}"),
    }

    let outcome = rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
        .expect("run after cancellation");
    assert_eq!(outcome.present(), 1);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn active_run_holds_the_lease_until_cancelled() {
    let fixture = Fixture::new(&[(&["a.bin"], 16 * MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = Arc::new(fixture.rechecker(storage.clone(), Arc::new(HashPool::new())));
    let base = Arc::new(fixture.base());
    let cancel = RecheckCancel::new();
    let (tx, mut rx) = mpsc::channel(4);

    let task = {
        let rechecker = Arc::clone(&rechecker);
        let base = Arc::clone(&base);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            rechecker
                .run(RecheckSelection::All, &base, &cancel, Some(&tx))
                .await
        })
    };

    let first = rx.recv().await.expect("first progress update");
    assert_eq!(first.checked(), 1);

    match rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
    {
        Err(RecheckError::AlreadyRunning) => {}
        other => panic!("expected AlreadyRunning, got {other:?}"),
    }

    cancel.cancel();
    match task.await.expect("task") {
        Err(RecheckError::Cancelled) => {}
        other => panic!("expected cancellation, got {other:?}"),
    }

    let outcome = rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
        .expect("run after cancellation");
    assert_eq!(outcome.present(), 16);
    assert_eq!(outcome.bits().count_verified(), 16);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn dropping_the_run_releases_the_lease() {
    let fixture = Fixture::new(&[(&["a.bin"], MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let base = fixture.base();

    let dropped = tokio::time::timeout(
        std::time::Duration::ZERO,
        rechecker.run(RecheckSelection::All, &base, &RecheckCancel::new(), None),
    )
    .await;
    assert!(
        dropped.is_err(),
        "the run should be dropped at its first await"
    );

    let outcome = rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
        .expect("run after a dropped future");
    assert_eq!(outcome.present(), 1);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn progress_updates_are_bounded_and_advisory() {
    let fixture = Fixture::new(&[(&["a.bin"], 4 * MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let (tx, mut rx) = mpsc::channel(1);

    let outcome = rechecker
        .run(
            RecheckSelection::All,
            &fixture.base(),
            &RecheckCancel::new(),
            Some(&tx),
        )
        .await
        .expect("recheck");
    assert_eq!(outcome.checked(), 4);
    assert_eq!(outcome.present(), 4);

    let only = rx.try_recv().expect("one buffered update");
    assert_eq!(only.piece(), 0);
    assert_eq!(only.checked(), 1);
    assert_eq!(only.total(), 4);
    assert_eq!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty),
        "a full channel drops later updates, including the final one"
    );

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn progress_reports_every_selected_piece() {
    let fixture = Fixture::new(&[(&["a.bin"], 4 * MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let (tx, mut rx) = mpsc::channel(4);
    let selection = [0u32, 2];

    let outcome = rechecker
        .run(
            RecheckSelection::Only(&selection),
            &fixture.base(),
            &RecheckCancel::new(),
            Some(&tx),
        )
        .await
        .expect("recheck");
    assert_eq!(outcome.checked(), 2);
    assert_eq!(outcome.present(), 2);

    let first = rx.try_recv().expect("first update");
    assert_eq!((first.piece(), first.checked(), first.total()), (0, 1, 2));
    let last = rx.try_recv().expect("final update");
    assert_eq!((last.piece(), last.checked(), last.present()), (2, 2, 2));
    assert_eq!(last.absent(), 0);
    assert_eq!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty));

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn storage_shutdown_mid_run_is_a_typed_error() {
    let fixture = Fixture::new(&[(&["a.bin"], 8 * MIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = Arc::new(fixture.rechecker(storage.clone(), Arc::new(HashPool::new())));
    let base = Arc::new(fixture.base());
    let cancel = RecheckCancel::new();
    let (tx, mut rx) = mpsc::channel(4);

    let task = {
        let rechecker = Arc::clone(&rechecker);
        let base = Arc::clone(&base);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            rechecker
                .run(RecheckSelection::All, &base, &cancel, Some(&tx))
                .await
        })
    };

    rx.recv().await.expect("first progress update");
    storage.shutdown().await.expect("shutdown");

    match task.await.expect("task") {
        Err(RecheckError::Storage {
            source: StorageHandleError::ShuttingDown,
            ..
        }) => {}
        other => panic!("expected a shutdown error, got {other:?}"),
    }

    match rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
    {
        Err(RecheckError::Storage {
            source: StorageHandleError::ShuttingDown,
            ..
        }) => {}
        other => panic!("expected a shutdown error, got {other:?}"),
    }
}

#[tokio::test]
async fn last_short_piece_hashes_its_actual_length() {
    let fixture = Fixture::new(&[(&["a.bin"], 2 * MIB + 200 * KIB)], MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    let outcome = rechecker
        .run(
            RecheckSelection::All,
            &fixture.base(),
            &RecheckCancel::new(),
            None,
        )
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
        .run(
            RecheckSelection::Only(&[]),
            &base,
            &RecheckCancel::new(),
            None,
        )
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
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
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
        .run(
            RecheckSelection::All,
            &fixture.base(),
            &RecheckCancel::new(),
            None,
        )
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

#[tokio::test]
async fn the_next_batch_is_read_while_the_current_batch_hashes() {
    let fixture = Fixture::new(&[(&["a.bin"], 2 * MIB)], 2 * MIB);
    let storage = fixture.open().await;
    let pool = Arc::new(HashPool::new());
    let (gate, mut entered) = HashGate::new();
    pool.set_test_gate(Arc::clone(&gate));
    let rechecker = Arc::new(fixture.rechecker(storage.clone(), Arc::clone(&pool)));
    let base = Arc::new(fixture.base());

    let task = {
        let rechecker = Arc::clone(&rechecker);
        let base = Arc::clone(&base);
        tokio::spawn(async move {
            rechecker
                .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
                .await
        })
    };

    entered
        .recv()
        .await
        .expect("first hash job entered the gate");

    // While the first batch is held in the gate, the second batch must already
    // be read. Without overlap this stays at four reads.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if storage.read_count().await.expect("read count") >= 8 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the second batch was not read while the first batch hashed"
        );
        tokio::task::yield_now().await;
    }

    gate.open();
    let outcome = task.await.expect("task").expect("recheck");
    assert_eq!(outcome.present(), 1);
    assert_eq!(pool.started_jobs(), 2);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_short_read_in_the_overlapped_batch_still_abandons_the_piece() {
    let fixture = Fixture::new(&[(&["a.bin"], 2 * MIB)], 2 * MIB);
    fixture.truncate_file(0, MIB + 100 * KIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));

    // Four reads fill the first batch, the fifth returns 100 KiB of the
    // 256 KiB it asked for. A sixth read would hit the injected failure.
    storage.fail_read_after(5).await.expect("arm read failure");

    let selection = [0u32];
    let outcome = rechecker
        .run(
            RecheckSelection::Only(&selection),
            &fixture.base(),
            &RecheckCancel::new(),
            None,
        )
        .await
        .expect("recheck");

    assert_eq!(outcome.checked(), 1);
    assert_eq!(outcome.present(), 0);
    assert_eq!(outcome.absent(), 1);
    assert_eq!(outcome.bits().count_verified(), 0);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn cancelling_during_the_final_hash_returns_cancelled() {
    let fixture = Fixture::new(&[(&["a.bin"], MIB)], MIB);
    let storage = fixture.open().await;
    let pool = Arc::new(HashPool::new());
    let (gate, mut entered) = HashGate::new();
    pool.set_test_gate(Arc::clone(&gate));
    let rechecker = Arc::new(fixture.rechecker(storage.clone(), Arc::clone(&pool)));
    let base = Arc::new(fixture.base());
    let cancel = RecheckCancel::new();

    let task = {
        let rechecker = Arc::clone(&rechecker);
        let base = Arc::clone(&base);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            rechecker
                .run(RecheckSelection::All, &base, &cancel, None)
                .await
        })
    };

    entered.recv().await.expect("final hash entered the gate");
    cancel.cancel();
    gate.open();

    match task.await.expect("task") {
        Err(RecheckError::Cancelled) => {}
        other => panic!("expected cancellation, got {other:?}"),
    }
    assert_eq!(base.count_verified(), 0);

    // The lease is released even though the final hash completed.
    let outcome = rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
        .expect("run after cancellation");
    assert_eq!(outcome.present(), 1);

    drop(rechecker);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_panicking_hash_job_is_a_typed_error() {
    let fixture = Fixture::new(&[(&["a.bin"], MIB)], MIB);
    let storage = fixture.open().await;
    let pool = Arc::new(HashPool::new());
    let (gate, _entered) = HashGate::new();
    pool.set_test_gate(Arc::clone(&gate));
    gate.panic_on_enter();
    let rechecker = fixture.rechecker(storage.clone(), Arc::clone(&pool));
    let base = fixture.base();

    match rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
    {
        Err(RecheckError::HashTask { piece: 0, .. }) => {}
        other => panic!("expected a hash task error, got {other:?}"),
    }
    assert_eq!(base.count_verified(), 0);

    // A fresh pool can retry, and the storage worker survived the panic.
    let retry = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let outcome = retry
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
        .expect("retry after a hash panic");
    assert_eq!(outcome.present(), 1);

    drop(rechecker);
    drop(retry);
    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_storage_error_in_the_overlapped_batch_stops_the_run() {
    let fixture = Fixture::new(&[(&["a.bin"], 2 * MIB)], 2 * MIB);
    let storage = fixture.open().await;
    let rechecker = fixture.rechecker(storage.clone(), Arc::new(HashPool::new()));
    let base = fixture.base();

    // Four reads fill the first batch; the fifth is the first read of the
    // overlapped batch and fails.
    storage.fail_read_after(4).await.expect("arm read failure");

    match rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
    {
        Err(RecheckError::Storage {
            piece: 0,
            offset,
            source: StorageHandleError::Storage(StorageError::PermissionDenied { .. }),
        }) => assert_eq!(offset, MIB),
        other => panic!("expected a typed permission error, got {other:?}"),
    }
    assert_eq!(base.count_verified(), 0);

    // The first batch's hash finished and released its permit, and the
    // one-shot injection is gone, so a retry succeeds.
    let outcome = rechecker
        .run(RecheckSelection::All, &base, &RecheckCancel::new(), None)
        .await
        .expect("run after the error");
    assert_eq!(outcome.present(), 1);

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

    // Validation must reject a selection before any read: this one-shot
    // failure would abort the run if a read happened.
    storage.fail_read_after(0).await.expect("arm read failure");

    let unordered = [2u32, 1];
    match rechecker
        .run(
            RecheckSelection::Only(&unordered),
            &base,
            &RecheckCancel::new(),
            None,
        )
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
        .run(
            RecheckSelection::Only(&duplicate),
            &base,
            &RecheckCancel::new(),
            None,
        )
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
        .run(
            RecheckSelection::Only(&out_of_range),
            &base,
            &RecheckCancel::new(),
            None,
        )
        .await
    {
        Err(RecheckError::SelectionOutOfRange {
            piece: 3,
            piece_count: 3,
        }) => {}
        other => panic!("expected an out-of-range selection error, got {other:?}"),
    }

    let short_base = Bitfield::new(2).expect("short bitfield");
    match rechecker
        .run(
            RecheckSelection::All,
            &short_base,
            &RecheckCancel::new(),
            None,
        )
        .await
    {
        Err(RecheckError::PieceCountMismatch {
            expected: 3,
            found: 2,
        }) => {}
        other => panic!("expected a piece count mismatch, got {other:?}"),
    }

    match storage.read_range(0, 1).await {
        Err(StorageHandleError::Storage(StorageError::PermissionDenied { .. })) => {}
        other => panic!("selection validation must not read, got {other:?}"),
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
