use std::fs;

use tempfile::TempDir;

use super::*;
use crate::bitfield::Bitfield;
use crate::resume::{ResumeDocument, ResumeFile, encode_document};

#[tokio::test]
async fn load_returns_none_when_the_document_is_missing() {
    let dir = TempDir::new().expect("create temp dir");
    let store = ResumeStore::new(dir.path().join("resume"));

    assert!(
        store
            .load(&InfoHash::from_bytes([0x11; 20]))
            .await
            .expect("load")
            .is_none()
    );
}

#[tokio::test]
async fn load_round_trips_a_saved_document() {
    let dir = TempDir::new().expect("create temp dir");
    let store = ResumeStore::new(dir.path().join("resume"));
    let info_hash = InfoHash::from_bytes([0x22; 20]);
    let document = ResumeDocument::new(
        *info_hash.as_bytes(),
        4,
        8,
        [0x33; 20],
        Bitfield::from_bytes(&[0b11], 2).expect("bitfield"),
        vec![ResumeFile::new(4, 4, 1), ResumeFile::new(4, 4, 2)],
        123,
    );

    fs::create_dir_all(store.directory()).expect("create resume directory");
    fs::write(
        store.document_path(&info_hash),
        encode_document(&document).expect("encode"),
    )
    .expect("write document");

    assert_eq!(store.load(&info_hash).await.expect("load"), Some(document));
}

#[tokio::test]
async fn load_rejects_an_oversized_document() {
    let dir = TempDir::new().expect("create temp dir");
    let store = ResumeStore::new(dir.path().join("resume"));
    let info_hash = InfoHash::from_bytes([0x44; 20]);
    fs::create_dir_all(store.directory()).expect("create resume directory");

    let path = store.document_path(&info_hash);
    let file = fs::File::create(&path).expect("create document");
    file.set_len(u64::try_from(MAX_RESUME_BYTES).expect("limit") + 1)
        .expect("extend document");

    match store.load(&info_hash).await {
        Err(LoadError::Decode(DecodeError::TooLarge { len, max })) => {
            assert!(len > max);
            assert_eq!(max, MAX_RESUME_BYTES);
        }
        other => panic!("unexpected load result: {other:?}"),
    }
}

#[tokio::test]
async fn load_errors_name_the_document_but_not_the_directory() {
    let dir = TempDir::new().expect("create temp dir");
    let store = ResumeStore::new(dir.path().join("resume"));
    let info_hash = InfoHash::from_bytes([0x66; 20]);
    fs::create_dir_all(store.directory()).expect("create resume directory");
    fs::create_dir(store.document_path(&info_hash)).expect("document directory");

    // Reading a directory fails on every platform, but through a different
    // call: `File::open` on Windows, `read_to_end` elsewhere.
    let error = store.load(&info_hash).await.expect_err("read a directory");

    match &error {
        LoadError::Io { name, .. } => {
            assert_eq!(name, &format!("{info_hash}.resume"));
        }
        other => panic!("unexpected load result: {other:?}"),
    }
    let message = error.to_string();
    assert!(
        !message.contains(store.directory().to_str().expect("utf-8 path")),
        "the message must not contain the directory: {message}"
    );
}

#[tokio::test]
async fn load_reports_a_corrupt_document_as_a_decode_error() {
    let dir = TempDir::new().expect("create temp dir");
    let store = ResumeStore::new(dir.path().join("resume"));
    let info_hash = InfoHash::from_bytes([0x55; 20]);
    fs::create_dir_all(store.directory()).expect("create resume directory");
    fs::write(store.document_path(&info_hash), b"not bencode").expect("write document");

    assert!(matches!(
        store.load(&info_hash).await,
        Err(LoadError::Decode(_))
    ));
}
