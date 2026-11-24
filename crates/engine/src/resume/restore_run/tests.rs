use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use sha1::{Digest, Sha1};
use tempfile::TempDir;

use super::*;
use crate::bitfield::Bitfield;
use crate::metainfo::{FileInfo, InfoHash, MetaInfo, Mode, PIECE_HASH_LENGTH};
use crate::piece::{HashPool, RecheckCancel, RecheckError, Rechecker};
use crate::resume::{LoadError, ResumeStore, decode_document, encode_document};
use crate::storage::StorageConfig;
use crate::storage::StorageHandle;
use crate::storage::layout::FileMap;

const HASH: [u8; PIECE_HASH_LENGTH] = [0x6b; PIECE_HASH_LENGTH];

fn hash_of(bytes: &[u8]) -> [u8; PIECE_HASH_LENGTH] {
    let digest = Sha1::digest(bytes);
    let mut hash = [0u8; PIECE_HASH_LENGTH];
    hash.copy_from_slice(&digest);
    hash
}

fn test_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| {
            let value = index.wrapping_mul(43).wrapping_add(17) & 0xFF;
            u8::try_from(value).expect("byte")
        })
        .collect()
}

fn all_ones(piece_count: u32) -> Bitfield {
    let count = usize::try_from(piece_count).expect("piece count");
    let mut bytes = vec![0xFF; count.div_ceil(8)];
    let remainder = count % 8;
    if remainder != 0
        && let Some(last) = bytes.last_mut()
    {
        *last = (1u8 << remainder) - 1;
    }
    Bitfield::from_bytes(&bytes, piece_count).expect("bitfield")
}

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    resume_dir: PathBuf,
    meta: Arc<MetaInfo>,
    map: Arc<FileMap>,
    data: Vec<u8>,
}

impl Fixture {
    fn new<S: AsRef<str>>(files: &[(S, u64)], piece_length: u64) -> Self {
        let total: u64 = files.iter().map(|(_, length)| *length).sum();
        let data = test_bytes(usize::try_from(total).expect("total length"));

        let mut file_infos = Vec::with_capacity(files.len());
        for (name, length) in files {
            file_infos.push(FileInfo {
                path: vec!["t".to_owned(), name.as_ref().to_owned()],
                length: *length,
            });
        }

        let piece_len = usize::try_from(piece_length).expect("piece length");
        let piece_hashes = data.chunks(piece_len).map(hash_of).collect();
        let meta = Arc::new(MetaInfo {
            info_hash: InfoHash::from_bytes(HASH),
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

        let dir = TempDir::new().expect("create temp dir");
        let root = dir.path().join("root");
        let resume_dir = dir.path().join("resume");
        fs::create_dir_all(&root).expect("create root");
        let fixture = Self {
            _dir: dir,
            root,
            resume_dir,
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
                fs::create_dir_all(parent).expect("create parent");
            }
            let start = usize::try_from(file.offset).expect("file offset");
            let end = start + usize::try_from(file.length).expect("file length");
            fs::write(path, &self.data[start..end]).expect("write file");
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

    fn rechecker(&self, storage: &StorageHandle) -> Rechecker {
        Rechecker::new(
            storage.clone(),
            Arc::clone(&self.map),
            Arc::clone(&self.meta),
            Arc::new(HashPool::new()),
        )
        .expect("rechecker")
    }

    fn store(&self) -> ResumeStore {
        ResumeStore::new(self.resume_dir.clone())
    }

    fn hash(&self) -> InfoHash {
        self.meta.info_hash
    }

    fn file_path(&self, index: usize) -> PathBuf {
        let file = self.map.files().get(index).expect("file index");
        self.root.join(file.to_path())
    }

    fn flip_file(&self, index: usize) {
        let path = self.file_path(index);
        let mut bytes = fs::read(&path).expect("read file");
        bytes[0] ^= 0xFF;
        fs::write(&path, &bytes).expect("write file");

        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open file");
        file.set_modified(SystemTime::now() + Duration::from_secs(60))
            .expect("set modification time");
    }

    fn corrupt_file(&self, index: usize) {
        let path = self.file_path(index);
        let mut bytes = fs::read(&path).expect("read file");
        for byte in &mut bytes {
            *byte ^= 0xFF;
        }
        fs::write(path, bytes).expect("write file");
    }
}

async fn restore(
    fixture: &Fixture,
    store: &ResumeStore,
    storage: &StorageHandle,
) -> RestoreOutcome {
    let rechecker = fixture.rechecker(storage);
    store
        .restore(&rechecker, &RecheckCancel::new(), None)
        .await
        .expect("restore")
}

#[tokio::test]
async fn restore_rechecks_a_stale_file_and_the_spot_sample() {
    let files: Vec<(String, u64)> = (0..20)
        .map(|index| (format!("f{index:02}.bin"), 4u64))
        .collect();
    let fixture = Fixture::new(&files, 4);
    let storage = fixture.open().await;
    let store = fixture.store();

    store
        .save(fixture.hash(), &fixture.map, all_ones(20), &storage)
        .await
        .expect("save");
    fixture.flip_file(0);

    let outcome = restore(&fixture, &store, &storage).await;

    assert!(matches!(outcome.source(), RestoreSource::Resume));
    assert_eq!(outcome.checked(), 6, "one stale piece plus five samples");
    assert_eq!(outcome.bits().count_verified(), 19);
    for piece in 0..20 {
        assert_eq!(
            outcome.bits().get(piece).expect("get piece"),
            piece != 0,
            "piece {piece}"
        );
    }

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn restore_without_resume_reads_only_readable_pieces() {
    let fixture = Fixture::new(&[("a.bin", 4u64), ("b.bin", 4u64)], 4);
    let storage = fixture.open().await;
    let store = fixture.store();
    fs::remove_file(fixture.file_path(1)).expect("remove file");

    let outcome = restore(&fixture, &store, &storage).await;

    assert!(matches!(outcome.source(), RestoreSource::NoResume));
    assert_eq!(outcome.checked(), 1, "the missing file's piece is not read");
    assert!(outcome.bits().get(0).expect("get piece 0"));
    assert!(!outcome.bits().get(1).expect("get piece 1"));

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn restore_falls_back_when_the_document_does_not_match() {
    let fixture = Fixture::new(&[("a.bin", 12u64)], 4);
    let storage = fixture.open().await;
    let store = fixture.store();
    store
        .save(fixture.hash(), &fixture.map, all_ones(3), &storage)
        .await
        .expect("save");

    let path = store.document_path(&fixture.hash());
    let mut document = decode_document(&fs::read(&path).expect("read document")).expect("decode");
    document.total_length += 1;
    fs::write(&path, encode_document(&document).expect("encode")).expect("write document");

    let outcome = restore(&fixture, &store, &storage).await;

    match outcome.source() {
        RestoreSource::Invalid(ValidationError::TotalLength { expected, found }) => {
            assert_eq!((*expected, *found), (12, 13));
        }
        other => panic!("unexpected restore source: {other:?}"),
    }
    assert!(
        store.document_path(&fixture.hash()).exists(),
        "a mismatched document must be left in place"
    );
    assert_eq!(outcome.checked(), 3);
    assert_eq!(outcome.bits().count_verified(), 3);

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn restore_falls_back_when_the_document_is_corrupt() {
    let fixture = Fixture::new(&[("a.bin", 8u64)], 4);
    let storage = fixture.open().await;
    let store = fixture.store();
    fs::create_dir_all(store.directory()).expect("create resume directory");
    fs::write(store.document_path(&fixture.hash()), b"not bencode").expect("write document");

    let outcome = restore(&fixture, &store, &storage).await;

    assert!(matches!(
        outcome.source(),
        RestoreSource::LoadFailed(LoadError::Decode(_))
    ));
    assert!(
        store.document_path(&fixture.hash()).exists(),
        "a corrupt document must be left in place"
    );
    assert_eq!(outcome.checked(), 2);
    assert_eq!(outcome.bits().count_verified(), 2);

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn restore_falls_back_to_a_full_recheck_when_a_spot_check_fails() {
    let fixture = Fixture::new(&[("a.bin", 24u64)], 4);
    let storage = fixture.open().await;
    let store = fixture.store();
    fixture.corrupt_file(0);
    store
        .save(fixture.hash(), &fixture.map, all_ones(6), &storage)
        .await
        .expect("save");

    let outcome = restore(&fixture, &store, &storage).await;

    assert!(matches!(outcome.source(), RestoreSource::SpotCheckFailed));
    assert!(
        store.document_path(&fixture.hash()).exists(),
        "a failed spot check must leave the document in place"
    );
    assert_eq!(outcome.checked(), 11, "five samples, then all six pieces");
    assert_eq!(outcome.bits().count_verified(), 0);
    for piece in 0..6 {
        assert!(
            !outcome.bits().get(piece).expect("get piece"),
            "piece {piece}"
        );
    }

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn restore_reads_only_the_spot_sample_when_nothing_changed() {
    let fixture = Fixture::new(&[("a.bin", 24u64)], 4);
    let storage = fixture.open().await;
    let store = fixture.store();
    store
        .save(fixture.hash(), &fixture.map, all_ones(6), &storage)
        .await
        .expect("save");

    let outcome = restore(&fixture, &store, &storage).await;

    assert!(matches!(outcome.source(), RestoreSource::Resume));
    assert_eq!(outcome.checked(), 5);
    assert_eq!(outcome.bits().count_verified(), 6);
    assert!(
        outcome.bits().get(4).expect("get piece 4"),
        "piece 4 was outside the sample and keeps its saved bit"
    );

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn restore_propagates_cancellation() {
    let fixture = Fixture::new(&[("a.bin", 8u64)], 4);
    let storage = fixture.open().await;
    let store = fixture.store();
    let rechecker = fixture.rechecker(&storage);
    let cancel = RecheckCancel::new();
    cancel.cancel();

    let error = store
        .restore(&rechecker, &cancel, None)
        .await
        .expect_err("cancelled restore");

    assert!(matches!(
        error,
        RestoreRunError::Recheck(RecheckError::Cancelled)
    ));

    storage.shutdown().await.expect("shutdown");
}
