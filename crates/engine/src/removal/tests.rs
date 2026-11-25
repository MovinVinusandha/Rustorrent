use std::fs;
use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;

use super::*;
use crate::metainfo::InfoHash;
use crate::resume::{RemoveError, ResumeRemoval, ResumeStore};
use crate::storage::StorageConfig;
use crate::storage::layout::{FileMap, MappedFile};
use crate::storage::thread::{StorageHandle, StorageHandleError};
use crate::storage::{RemovalReport, StorageError};

const HASH: [u8; 20] = [0x33; 20];

fn info_hash() -> InfoHash {
    InfoHash::from_bytes(HASH)
}

fn map_with(files: &[(&[&str], u64)]) -> Arc<FileMap> {
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

    Arc::new(FileMap::from_mapped_files(mapped, offset, 1, 0))
}

async fn open_storage(root: &Path, map: Arc<FileMap>) -> StorageHandle {
    StorageHandle::open(root.to_path_buf(), map, StorageConfig::default())
        .await
        .expect("open storage")
}

fn write_document(store: &ResumeStore, hash: InfoHash) {
    fs::write(store.document_path(&hash), b"doc").expect("write resume document");
}

#[tokio::test]
async fn resume_only_removal_keeps_torrent_data() {
    let dir = TempDir::new().expect("create temp dir");
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("create root");
    let resume = ResumeStore::new(dir.path().join("resume"));
    fs::create_dir_all(resume.directory()).expect("create resume dir");
    let hash = info_hash();
    write_document(&resume, hash);
    let handle = open_storage(&root, map_with(&[(&["top", "a"], 4)])).await;
    handle
        .write_range(0, b"data".to_vec())
        .await
        .expect("write data");

    let outcome = remove_torrent(&resume, &handle, hash, false).await;

    assert!(matches!(
        outcome.resume,
        Ok(ResumeRemoval {
            document_removed: true,
            temps_removed: 0
        })
    ));
    assert!(outcome.data.is_none());
    assert!(outcome.is_success());
    assert_eq!(
        fs::read(root.join("top").join("a")).expect("read data"),
        b"data"
    );
    assert!(!resume.document_path(&hash).exists());
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn data_removal_runs_after_resume_and_marks_storage_removed() {
    let dir = TempDir::new().expect("create temp dir");
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("create root");
    let resume = ResumeStore::new(dir.path().join("resume"));
    fs::create_dir_all(resume.directory()).expect("create resume dir");
    let hash = info_hash();
    write_document(&resume, hash);
    let handle = open_storage(&root, map_with(&[(&["top", "a"], 4)])).await;
    handle
        .write_range(0, b"data".to_vec())
        .await
        .expect("write data");

    let outcome = remove_torrent(&resume, &handle, hash, true).await;

    assert!(matches!(
        outcome.resume,
        Ok(ResumeRemoval {
            document_removed: true,
            ..
        })
    ));
    let data = outcome
        .data
        .as_ref()
        .expect("data removal was attempted")
        .as_ref()
        .expect("data removal succeeded");
    assert!(!data.has_failures());
    assert_eq!(data.removed_count, 2, "the file and its directory");
    assert!(!root.join("top").exists());
    assert!(root.is_dir());
    assert!(outcome.is_success());
    assert!(matches!(
        handle.write_range(0, b"data".to_vec()).await,
        Err(StorageHandleError::Storage(StorageError::Removed))
    ));
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_resume_failure_still_removes_data() {
    let dir = TempDir::new().expect("create temp dir");
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("create root");
    let resume = ResumeStore::new(dir.path().join("resume"));
    fs::create_dir_all(resume.directory()).expect("create resume dir");
    let hash = info_hash();
    fs::create_dir(resume.document_path(&hash)).expect("directory in place of document");
    let handle = open_storage(&root, map_with(&[(&["top", "a"], 4)])).await;
    handle
        .write_range(0, b"data".to_vec())
        .await
        .expect("write data");

    let outcome = remove_torrent(&resume, &handle, hash, true).await;

    assert!(matches!(outcome.resume, Err(RemoveError::Remove { .. })));
    let data = outcome
        .data
        .as_ref()
        .expect("data removal was attempted")
        .as_ref()
        .expect("data removal succeeded");
    assert!(!data.has_failures());
    assert!(!root.join("top").exists());
    assert!(!outcome.is_success());
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn a_save_in_progress_skips_data_removal() {
    let dir = TempDir::new().expect("create temp dir");
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("create root");
    let resume = ResumeStore::new(dir.path().join("resume"));
    let hash = info_hash();
    let lease = resume.hold_save_lease(hash);
    let handle = open_storage(&root, map_with(&[(&["top", "a"], 4)])).await;
    handle
        .write_range(0, b"data".to_vec())
        .await
        .expect("write data");

    let outcome = remove_torrent(&resume, &handle, hash, true).await;

    assert!(matches!(outcome.resume, Err(RemoveError::AlreadySaving)));
    assert!(outcome.data.is_none());
    assert!(root.join("top").join("a").is_file());
    drop(lease);
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn missing_resume_data_is_success() {
    let dir = TempDir::new().expect("create temp dir");
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("create root");
    let resume = ResumeStore::new(dir.path().join("resume"));
    let hash = info_hash();
    let handle = open_storage(&root, map_with(&[(&["top", "a"], 4)])).await;
    handle
        .write_range(0, b"data".to_vec())
        .await
        .expect("write data");

    let outcome = remove_torrent(&resume, &handle, hash, true).await;

    assert!(matches!(
        outcome.resume,
        Ok(ResumeRemoval {
            document_removed: false,
            temps_removed: 0
        })
    ));
    assert!(outcome.is_success());
    assert!(!root.join("top").exists());
    handle.shutdown().await.expect("shutdown");
}

#[test]
fn outcome_is_not_success_when_data_failed() {
    let outcome = RemoveOutcome {
        resume: Ok(ResumeRemoval {
            document_removed: true,
            temps_removed: 0,
        }),
        data: Some(Ok(RemovalReport {
            failed_count: 1,
            ..RemovalReport::default()
        })),
    };

    assert!(!outcome.is_success());
}
