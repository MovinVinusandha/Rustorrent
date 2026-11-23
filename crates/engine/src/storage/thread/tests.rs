use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::*;
use crate::storage::layout::MappedFile;

fn temp_dir() -> TempDir {
    TempDir::new().expect("create temp dir")
}

fn root_in(dir: &TempDir) -> PathBuf {
    let root = dir.path().join("root");
    fs::create_dir_all(&root).expect("create root");
    root
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

async fn open(root: &Path, map: Arc<FileMap>) -> StorageHandle {
    StorageHandle::open(root.to_path_buf(), map, StorageConfig::default())
        .await
        .expect("open storage")
}

fn exit_receiver(handle: &StorageHandle) -> oneshot::Receiver<()> {
    handle
        .exit
        .lock()
        .expect("lock exit hook")
        .take()
        .expect("exit hook")
}

async fn wait_for_exit(receiver: oneshot::Receiver<()>) {
    timeout(Duration::from_secs(5), receiver)
        .await
        .expect("worker exit timed out")
        .expect("worker exit signal");
}

async fn close(handle: StorageHandle) {
    let receiver = exit_receiver(&handle);
    drop(handle);
    wait_for_exit(receiver).await;
}

#[test]
fn handle_is_clone_send_sync() {
    fn assert_traits<T: Clone + Send + Sync>() {}
    assert_traits::<StorageHandle>();
}

#[tokio::test]
async fn open_reports_missing_root() {
    let dir = temp_dir();
    let root = dir.path().join("missing");
    let map = map_with(&[(&["a.bin"], 4)]);

    match StorageHandle::open(root, map, StorageConfig::default()).await {
        Ok(_) => panic!("missing root must not open"),
        Err(error) => assert!(matches!(
            error,
            StorageHandleError::Storage(StorageError::Io { .. })
        )),
    }
}

#[tokio::test]
async fn write_then_read_roundtrip() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 8)]);
    let handle = open(&root, map).await;

    handle
        .write_range(2, b"hello".to_vec())
        .await
        .expect("write");
    assert_eq!(
        handle.read_range(0, 8).await.expect("read"),
        b"\0\0hello".to_vec()
    );
    assert_eq!(
        fs::read(root.join("a.bin")).expect("read file"),
        b"\0\0hello"
    );

    close(handle).await;
}

#[tokio::test]
async fn read_stops_at_a_missing_file_boundary() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 4), (&["b.bin"], 4)]);
    let handle = open(&root, map).await;

    handle
        .write_range(0, b"aaaa".to_vec())
        .await
        .expect("write first file");
    assert_eq!(
        handle.read_range(0, 8).await.expect("read"),
        b"aaaa".to_vec()
    );

    close(handle).await;
}

#[tokio::test]
async fn out_of_range_write_is_typed_and_worker_survives() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 4)]);
    let handle = open(&root, map).await;

    match handle.write_range(3, b"xxxx".to_vec()).await {
        Ok(()) => panic!("out-of-range write must fail"),
        Err(StorageHandleError::Storage(StorageError::OutOfRange { .. })) => {}
        Err(other) => panic!("unexpected error: {other}"),
    }

    handle
        .write_range(0, b"data".to_vec())
        .await
        .expect("write after error");
    assert_eq!(
        handle.read_range(0, 4).await.expect("read"),
        b"data".to_vec()
    );

    close(handle).await;
}

#[tokio::test]
async fn oversized_ranges_are_rejected_before_sending() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 4)]);
    let handle = open(&root, map).await;

    assert!(matches!(
        handle.read_range(0, MAX_RANGE_LEN + 1).await,
        Err(StorageHandleError::RangeTooLarge { .. })
    ));
    assert!(matches!(
        handle.write_range(0, vec![0u8; MAX_RANGE_LEN + 1]).await,
        Err(StorageHandleError::RangeTooLarge { .. })
    ));

    handle
        .sync()
        .await
        .expect("worker survives rejected ranges");
    close(handle).await;
}

#[tokio::test]
async fn empty_ranges_are_noops() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 4)]);
    let handle = open(&root, map).await;

    assert!(handle.read_range(0, 0).await.expect("read").is_empty());
    handle
        .write_range(0, Vec::new())
        .await
        .expect("write empty");

    close(handle).await;
}

#[tokio::test]
async fn clones_share_one_worker() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 4)]);
    let first = open(&root, map).await;
    let second = first.clone();

    first.write_range(0, b"data".to_vec()).await.expect("write");
    assert_eq!(second.read_range(0, 4).await.expect("read"), b"data");

    let receiver = exit_receiver(&first);
    drop(first);
    drop(second);
    wait_for_exit(receiver).await;
}

#[tokio::test]
async fn queue_is_bounded() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 4)]);
    let handle = open(&root, map).await;

    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    assert!(
        handle
            .tx
            .try_send(Command::Hold {
                entered: entered_tx,
                release: release_rx,
            })
            .is_ok()
    );
    entered_rx.await.expect("worker entered hold");

    for _ in 0..QUEUE_CAPACITY {
        let (reply, _response) = oneshot::channel();
        assert!(handle.tx.try_send(Command::Sync { reply }).is_ok());
    }
    let (reply, _response) = oneshot::channel();
    assert!(handle.tx.try_send(Command::Sync { reply }).is_err());

    let sender = tokio::spawn({
        let handle = handle.clone();
        async move { handle.sync().await }
    });
    for _ in 0..64 {
        tokio::task::yield_now().await;
        assert!(!sender.is_finished(), "a full queue must block the sender");
    }

    release_tx.send(()).expect("release hold");
    sender.await.expect("join sender").expect("sync");

    close(handle).await;
}

#[cfg(panic = "unwind")]
#[tokio::test]
async fn panicked_worker_fails_pending_and_future_requests() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 4)]);
    let handle = open(&root, map).await;

    let (reply, response) = oneshot::channel();
    assert!(handle.tx.try_send(Command::Panic).is_ok());
    let _ = handle.tx.try_send(Command::Sync { reply });

    assert!(response.await.is_err());
    assert!(matches!(
        handle.sync().await,
        Err(StorageHandleError::WorkerStopped)
    ));
}

#[tokio::test]
async fn dropping_the_last_handle_closes_files() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a.bin"], 4)]);
    let handle = open(&root, map).await;

    handle
        .write_range(0, b"data".to_vec())
        .await
        .expect("write");
    let receiver = exit_receiver(&handle);
    drop(handle);
    wait_for_exit(receiver).await;

    assert_eq!(fs::read(root.join("a.bin")).expect("read file"), b"data");
    fs::remove_file(root.join("a.bin")).expect("remove after close");
}
