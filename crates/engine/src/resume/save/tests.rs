use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use super::*;
use crate::bitfield::Bitfield;
use crate::metainfo::InfoHash;
use crate::resume::decode_document;
use crate::storage::StorageConfig;
use crate::storage::layout::{FileMap, MappedFile};

const INFO: [u8; 20] = [0x5a; 20];

fn info_hash() -> InfoHash {
    InfoHash::from_bytes(INFO)
}

fn map_with(files: &[(&[&str], u64)], piece_length: u64) -> Arc<FileMap> {
    let mut offset = 0u64;
    let mut mapped = Vec::with_capacity(files.len());

    for (components, length) in files {
        let disk_components: Vec<String> = components
            .iter()
            .map(|component| (*component).to_owned())
            .collect();
        let first_piece = u32::try_from(offset / piece_length).expect("first piece");
        let pieces = if *length == 0 {
            first_piece..first_piece
        } else {
            let last_piece =
                u32::try_from((offset + *length - 1) / piece_length).expect("last piece");
            first_piece..last_piece + 1
        };
        mapped.push(MappedFile {
            original_path: disk_components.clone(),
            disk_components,
            length: *length,
            offset,
            pieces,
        });
        offset += *length;
    }

    let piece_count = u32::try_from(offset.div_ceil(piece_length)).expect("piece count");
    Arc::new(FileMap::from_mapped_files(
        mapped,
        offset,
        piece_length,
        piece_count,
    ))
}

fn fixture() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("create temp dir");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("create root");
    (dir, root)
}

fn write_file(root: &Path, components: &[&str], bytes: &[u8]) {
    let mut path = root.to_path_buf();
    for component in components {
        path.push(component);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, bytes).expect("write file");
}

async fn open(root: &Path, map: &Arc<FileMap>) -> StorageHandle {
    StorageHandle::open(
        root.to_path_buf(),
        Arc::clone(map),
        StorageConfig::default(),
    )
    .await
    .expect("open storage")
}

/// Sorted file names in `dir`.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list directory")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// Backdates `path` by `age` so the sweep sees an old file.
fn set_age(path: &Path, age: Duration) {
    let modified = SystemTime::now()
        .checked_sub(age)
        .expect("clock before the epoch");
    let file = std::fs::File::options()
        .append(true)
        .open(path)
        .expect("open file");
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .expect("set mtime");
}

fn document_name() -> String {
    format!("{}.resume", info_hash())
}

#[tokio::test]
async fn save_writes_a_valid_document_and_leaves_no_temp_file() {
    let (dir, root) = fixture();
    let resume_dir = dir.path().join("resume");
    let map = map_with(&[(&["a.bin"], 20)], 10);
    write_file(&root, &["a.bin"], &[0x11; 20]);
    let storage = open(&root, &map).await;
    let observed = storage.observe_files().await.expect("observe files");
    let snapshot = Bitfield::from_bytes(&[0b01], 2).expect("bitfield");
    let store = ResumeStore::new(resume_dir.clone());

    let future = store.save(info_hash(), &map, snapshot.clone(), &storage);
    fn require_send<T: Send>(future: T) -> T {
        future
    }
    require_send(future).await.expect("save");

    let path = store.document_path(&info_hash());
    let bytes = std::fs::read(&path).expect("read document");
    let document = decode_document(&bytes).expect("decode document");

    assert_eq!(document.info_hash, INFO);
    assert_eq!(document.piece_length, 10);
    assert_eq!(document.total_length, 20);
    assert_eq!(document.piece_count, 2);
    assert_eq!(document.layout_hash, layout_fingerprint(&map));
    assert_eq!(document.bitfield, snapshot);
    assert!(document.updated_at >= 0);
    assert_eq!(document.files.len(), 1);
    assert_eq!(document.files[0].length, 20);
    assert_eq!(document.files[0].observed_size, 20);
    match observed.as_slice() {
        [ObservedFile::Present { mtime_ns, .. }] => assert_eq!(
            document.files[0].mtime_ns,
            mtime_ns.unwrap_or(MTIME_UNAVAILABLE)
        ),
        other => panic!("unexpected observation: {other:?}"),
    }

    assert_eq!(names_in(&resume_dir), [document_name()]);

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn stale_temps_are_swept_but_recent_and_foreign_files_survive() {
    let (dir, root) = fixture();
    let resume_dir = dir.path().join("resume");
    std::fs::create_dir_all(&resume_dir).expect("create resume directory");
    let map = map_with(&[(&["a.bin"], 4)], 4);
    write_file(&root, &["a.bin"], &[0x77; 4]);
    let storage = open(&root, &map).await;
    let store = ResumeStore::new(resume_dir.clone());

    let stale = resume_dir.join(format!("{}.tmp.1.2.3", document_name()));
    let recent = resume_dir.join(format!("{}.tmp.4.5.6", document_name()));
    let malformed = resume_dir.join(format!("{}.tmp.7.8", document_name()));
    let foreign = resume_dir.join("notes.txt");
    for path in [&stale, &recent, &malformed, &foreign] {
        std::fs::write(path, b"junk").expect("write file");
    }
    set_age(&stale, Duration::from_secs(25 * 60 * 60));
    set_age(&malformed, Duration::from_secs(25 * 60 * 60));

    store
        .save(
            info_hash(),
            &map,
            Bitfield::new(1).expect("bitfield"),
            &storage,
        )
        .await
        .expect("save");

    assert!(!stale.exists(), "an old temp must be swept");
    assert!(recent.exists(), "a recent temp may belong to a live save");
    assert!(malformed.exists(), "only the temp pattern is swept");
    assert!(foreign.exists(), "unrelated files must survive");
    assert!(store.document_path(&info_hash()).is_file());

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn concurrent_saves_leave_one_valid_document_and_no_temp_files() {
    let (dir, root) = fixture();
    let resume_dir = dir.path().join("resume");
    let map = map_with(&[(&["a.bin"], 8)], 4);
    write_file(&root, &["a.bin"], &[0x88; 8]);
    let storage = open(&root, &map).await;
    let first = Bitfield::from_bytes(&[0b01], 2).expect("bitfield");
    let second = Bitfield::from_bytes(&[0b10], 2).expect("bitfield");
    let store_a = ResumeStore::new(resume_dir.clone());
    let store_b = ResumeStore::new(resume_dir.clone());

    let (saved_a, saved_b) = tokio::join!(
        store_a.save(info_hash(), &map, first.clone(), &storage),
        store_b.save(info_hash(), &map, second.clone(), &storage),
    );
    saved_a.expect("first save");
    saved_b.expect("second save");

    let bytes = std::fs::read(store_a.document_path(&info_hash())).expect("read document");
    let document = decode_document(&bytes).expect("decode document");
    assert!(
        document.bitfield == first || document.bitfield == second,
        "the document must be one complete snapshot, never a mixture"
    );
    assert_eq!(names_in(&resume_dir), [document_name()]);

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_failed_write_names_the_document_but_not_the_directory() {
    let (dir, root) = fixture();
    let resume_dir = dir.path().join("resume");
    std::fs::create_dir_all(&resume_dir).expect("create resume directory");
    let map = map_with(&[(&["a.bin"], 4)], 4);
    write_file(&root, &["a.bin"], &[0x99; 4]);
    let storage = open(&root, &map).await;
    let store = ResumeStore::new(resume_dir.clone());
    std::fs::create_dir(store.document_path(&info_hash())).expect("document directory");

    let error = store
        .save(
            info_hash(),
            &map,
            Bitfield::new(1).expect("bitfield"),
            &storage,
        )
        .await
        .expect_err("rename over a directory must fail");

    match &error {
        SaveError::Write { name, .. } => assert_eq!(name, &document_name()),
        other => panic!("unexpected error: {other:?}"),
    }
    let message = error.to_string();
    assert!(
        message.contains(&document_name()),
        "the message must name the document: {message}"
    );
    assert!(
        !message.contains(resume_dir.to_str().expect("utf-8 path")),
        "the message must not contain the directory: {message}"
    );

    storage.shutdown().await.expect("shutdown");
}

#[test]
fn only_well_formed_resume_temp_names_match() {
    assert!(is_resume_temp(OsStr::new("abc.resume.tmp.1.2.3")));
    assert!(is_resume_temp(OsStr::new("abc.resume.resume.tmp.1.2.3")));
    assert!(!is_resume_temp(OsStr::new("abc.resume")));
    assert!(!is_resume_temp(OsStr::new("abc.resume.tmp")));
    assert!(!is_resume_temp(OsStr::new("abc.resume.tmp.1.2")));
    assert!(!is_resume_temp(OsStr::new("abc.resume.tmp.1.2.3.4")));
    assert!(!is_resume_temp(OsStr::new("abc.resume.tmp.a.2.3")));
    assert!(!is_resume_temp(OsStr::new("abc.resume.tmp.1..3")));
    assert!(!is_resume_temp(OsStr::new(".resume.tmp.1.2.3")));
}

#[tokio::test]
async fn save_marks_a_missing_file_as_unverifiable() {
    let (dir, root) = fixture();
    let resume_dir = dir.path().join("resume");
    let map = map_with(&[(&["a.bin"], 4), (&["b.bin"], 4)], 4);
    write_file(&root, &["a.bin"], &[0x22; 4]);
    let storage = open(&root, &map).await;
    let store = ResumeStore::new(resume_dir);

    store
        .save(
            info_hash(),
            &map,
            Bitfield::new(2).expect("bitfield"),
            &storage,
        )
        .await
        .expect("save");

    let bytes = std::fs::read(store.document_path(&info_hash())).expect("read document");
    let document = decode_document(&bytes).expect("decode document");

    assert_eq!(document.files.len(), 2);
    assert_eq!(document.files[0].length, 4);
    assert_eq!(document.files[0].observed_size, 4);
    assert_ne!(document.files[0].mtime_ns, MTIME_UNAVAILABLE);
    assert_eq!(document.files[1].length, 4);
    assert_eq!(document.files[1].observed_size, 0);
    assert_eq!(document.files[1].mtime_ns, MTIME_UNAVAILABLE);

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn save_rejects_a_snapshot_with_the_wrong_piece_count() {
    let (dir, root) = fixture();
    let resume_dir = dir.path().join("resume");
    let map = map_with(&[(&["a.bin"], 20)], 10);
    write_file(&root, &["a.bin"], &[0x33; 20]);
    let storage = open(&root, &map).await;
    let store = ResumeStore::new(resume_dir.clone());

    let error = store
        .save(
            info_hash(),
            &map,
            Bitfield::new(3).expect("bitfield"),
            &storage,
        )
        .await
        .expect_err("wrong piece count");
    assert!(matches!(
        error,
        SaveError::PieceCount {
            expected: 2,
            found: 3
        }
    ));
    assert!(!resume_dir.exists(), "nothing must be written");

    store
        .save(
            info_hash(),
            &map,
            Bitfield::new(2).expect("bitfield"),
            &storage,
        )
        .await
        .expect("correct snapshot");

    storage.shutdown().await.expect("shutdown");
}

#[test]
fn the_save_lease_allows_one_save_per_torrent_and_is_released() {
    let dir = TempDir::new().expect("create temp dir");
    let store = ResumeStore::new(dir.path().join("resume"));

    let lease = store.acquire(info_hash()).expect("first lease");
    assert!(matches!(
        store.acquire(info_hash()),
        Err(SaveError::AlreadySaving)
    ));

    let other = store
        .acquire(InfoHash::from_bytes([0x77; 20]))
        .expect("a different torrent");
    drop(other);

    drop(lease);
    let lease = store.acquire(info_hash()).expect("lease released");
    drop(lease);
}

#[tokio::test]
async fn a_second_save_for_the_same_torrent_is_rejected() {
    let (dir, root) = fixture();
    let map = map_with(&[(&["a.bin"], 4)], 4);
    write_file(&root, &["a.bin"], &[0x55; 4]);
    let storage = open(&root, &map).await;
    let store = ResumeStore::new(dir.path().join("resume"));
    let bits = Bitfield::new(1).expect("bitfield");

    let lease = store.acquire(info_hash()).expect("first lease");
    let error = store
        .save(info_hash(), &map, bits.clone(), &storage)
        .await
        .expect_err("save while leased");
    assert!(matches!(error, SaveError::AlreadySaving));
    assert!(!store.document_path(&info_hash()).exists());

    drop(lease);
    store
        .save(info_hash(), &map, bits, &storage)
        .await
        .expect("save after the lease is released");
    assert!(store.document_path(&info_hash()).exists());

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_blocking_writer_keeps_the_lease_until_it_finishes() {
    let (dir, root) = fixture();
    let map = map_with(&[(&["a.bin"], 4)], 4);
    write_file(&root, &["a.bin"], &[0x66; 4]);
    let storage = open(&root, &map).await;
    let store = Arc::new(ResumeStore::new(dir.path().join("resume")));
    let bits = Bitfield::new(1).expect("bitfield");
    let lease = store.acquire(info_hash()).expect("lease");

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = tokio::task::spawn_blocking(move || {
        let _lease = lease;
        ready_tx.send(()).ok();
        release_rx.recv().ok();
    });
    ready_rx.await.expect("writer ready");

    let error = store
        .save(info_hash(), &map, bits.clone(), &storage)
        .await
        .expect_err("save while a blocking writer holds the lease");
    assert!(matches!(error, SaveError::AlreadySaving));

    release_tx.send(()).expect("release writer");
    writer.await.expect("join writer");

    store
        .save(info_hash(), &map, bits, &storage)
        .await
        .expect("save after the blocking writer finishes");
    assert!(store.document_path(&info_hash()).exists());

    storage.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_failed_flush_writes_nothing_and_releases_the_lease() {
    let (dir, root) = fixture();
    let resume_dir = dir.path().join("resume");
    let map = map_with(&[(&["a.bin"], 4)], 4);
    write_file(&root, &["a.bin"], &[0x44; 4]);
    let store = ResumeStore::new(resume_dir);
    let bits = Bitfield::new(1).expect("bitfield");

    let dead = open(&root, &map).await;
    dead.shutdown().await.expect("shutdown");
    let error = store
        .save(info_hash(), &map, bits.clone(), &dead)
        .await
        .expect_err("closed storage");
    assert!(matches!(error, SaveError::Storage(_)));
    assert!(!store.document_path(&info_hash()).exists());

    let live = open(&root, &map).await;
    store
        .save(info_hash(), &map, bits, &live)
        .await
        .expect("retry after the failed save");
    assert!(store.document_path(&info_hash()).exists());

    live.shutdown().await.expect("shutdown");
}

#[test]
fn resume_files_map_missing_and_present_observations() {
    let map = map_with(&[(&["a.bin"], 4), (&["b.bin"], 6)], 4);

    assert!(matches!(
        resume_files(&map, &[]),
        Err(SaveError::ObservedFileCount {
            expected: 2,
            found: 0
        })
    ));

    let files = resume_files(
        &map,
        &[
            ObservedFile::Present {
                size: 4,
                mtime_ns: Some(123),
            },
            ObservedFile::Missing,
        ],
    )
    .expect("file entries");

    assert_eq!(
        files,
        vec![
            ResumeFile::new(4, 4, 123),
            ResumeFile::new(6, 0, MTIME_UNAVAILABLE),
        ]
    );
}

#[test]
fn document_path_is_the_lowercase_hex_info_hash() {
    let dir = TempDir::new().expect("create temp dir");
    let store = ResumeStore::new(dir.path().to_path_buf());

    assert_eq!(
        store.document_path(&info_hash()),
        dir.path().join(format!("{}.resume", "5a".repeat(20)))
    );
}
