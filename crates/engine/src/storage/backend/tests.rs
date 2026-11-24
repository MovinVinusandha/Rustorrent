use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

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

fn raw_map(components: &[&str]) -> Arc<FileMap> {
    let disk_components: Vec<String> = components
        .iter()
        .map(|component| (*component).to_owned())
        .collect();
    let file = MappedFile {
        original_path: disk_components.clone(),
        disk_components,
        length: 0,
        offset: 0,
        pieces: 0..0,
    };
    Arc::new(FileMap::from_mapped_files(vec![file], 0, 1, 0))
}

fn open(root: &Path, map: Arc<FileMap>) -> Result<Storage, StorageError> {
    Storage::open(root, map, StorageConfig::default())
}

fn open_with(
    root: &Path,
    map: Arc<FileMap>,
    config: StorageConfig,
) -> Result<Storage, StorageError> {
    Storage::open(root, map, config)
}

fn set_len_config() -> StorageConfig {
    StorageConfig {
        preallocation: Preallocation::SetLen,
        ..StorageConfig::default()
    }
}

fn cap_config(max_open_handles: usize) -> StorageConfig {
    StorageConfig {
        max_open_handles,
        ..StorageConfig::default()
    }
}

#[cfg(unix)]
fn create_dir_symlink(original: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(original, link).expect("create directory symlink");
    true
}

#[cfg(unix)]
fn create_file_symlink(original: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(original, link).expect("create file symlink");
    true
}

#[cfg(windows)]
fn create_dir_symlink(original: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_dir(original, link) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => false,
        Err(error) => panic!("create directory symlink: {error}"),
    }
}

#[cfg(windows)]
fn create_file_symlink(original: &Path, link: &Path) -> bool {
    match std::os::windows::fs::symlink_file(original, link) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => false,
        Err(error) => panic!("create file symlink: {error}"),
    }
}

/// One entry of a directory snapshot.
#[derive(Debug, PartialEq, Eq)]
enum SnapshotEntry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

/// Records every path, kind, and byte below `root`, sorted by path.
///
/// Tests compare two snapshots to prove that nothing outside the download root
/// was created, removed, or modified.
fn snapshot(root: &Path) -> Vec<(PathBuf, SnapshotEntry)> {
    let mut entries = Vec::new();
    collect_snapshot(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn collect_snapshot(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, SnapshotEntry)>) {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => panic!("snapshot {path:?}: {error}"),
    };
    let relative = path
        .strip_prefix(root)
        .expect("snapshot path is below its root")
        .to_path_buf();

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).expect("read link");
        entries.push((relative, SnapshotEntry::Symlink(target)));
        return;
    }

    if metadata.is_dir() {
        entries.push((relative, SnapshotEntry::Directory));
        let mut children: Vec<PathBuf> = fs::read_dir(path)
            .expect("read dir")
            .map(|entry| entry.expect("dir entry").path())
            .collect();
        children.sort();
        for child in children {
            collect_snapshot(root, &child, entries);
        }
        return;
    }

    let bytes = fs::read(path).expect("read file");
    entries.push((relative, SnapshotEntry::File(bytes)));
}

#[test]
fn open_rejects_relative_root() {
    let map = map_with(&[(&["a"], 0)]);
    let error = Storage::open(Path::new("relative"), map, StorageConfig::default())
        .expect_err("relative root must fail");

    assert!(matches!(
        error,
        StorageError::InvalidConfig { reason } if reason.contains("absolute")
    ));
}

#[test]
fn open_rejects_zero_handles() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let config = StorageConfig {
        max_open_handles: 0,
        ..StorageConfig::default()
    };

    let error =
        Storage::open(&root, map_with(&[(&["a"], 0)]), config).expect_err("zero handles must fail");
    assert!(matches!(
        error,
        StorageError::InvalidConfig { reason } if reason.contains("max_open_handles")
    ));
}

#[test]
fn open_rejects_missing_root() {
    let dir = temp_dir();
    let root = dir.path().join("missing");
    let error = open(&root, map_with(&[(&["a"], 0)])).expect_err("missing root must fail");

    assert!(matches!(
        error,
        StorageError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn open_rejects_root_file() {
    let dir = temp_dir();
    let root = dir.path().join("root");
    fs::write(&root, b"x").expect("write root file");

    let error = open(&root, map_with(&[(&["a"], 0)])).expect_err("root file must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::NotDirectory,
            ..
        }
    ));
}

#[test]
fn open_accepts_symlinked_root() {
    let dir = temp_dir();
    let real = dir.path().join("real");
    fs::create_dir_all(&real).expect("create real root");
    let link = dir.path().join("link");
    if !create_dir_symlink(&real, &link) {
        return;
    }

    let storage = open(&link, map_with(&[(&["a"], 0)])).expect("symlinked root is allowed");
    assert_eq!(storage.root(), link.as_path());
}

#[test]
fn rejects_symlinked_directory() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).expect("create outside");
    if !create_dir_symlink(&outside, &root.join("top")) {
        return;
    }

    let before = snapshot(&outside);
    let error = open(&root, map_with(&[(&["top", "x"], 0)])).expect_err("symlink must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn rejects_symlinked_file() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, b"secret").expect("write outside file");
    fs::create_dir_all(root.join("top")).expect("create top");
    if !create_file_symlink(&outside, &root.join("top").join("a")) {
        return;
    }

    let before = snapshot(&outside);
    let error = open(&root, map_with(&[(&["top", "a"], 6)])).expect_err("symlink must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn rejects_directory_where_file_expected() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::create_dir_all(root.join("top").join("a")).expect("create dir");

    let error = open(&root, map_with(&[(&["top", "a"], 0)])).expect_err("dir must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::NotFile,
            ..
        }
    ));
}

#[test]
fn rejects_file_where_directory_expected() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::write(root.join("top"), b"x").expect("write file");

    let error = open(&root, map_with(&[(&["top", "a"], 0)])).expect_err("file must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::NotDirectory,
            ..
        }
    ));
}

#[cfg(unix)]
#[test]
fn rejects_duplicate_inode() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::create_dir_all(root.join("top")).expect("create top");
    fs::write(root.join("top").join("a"), b"x").expect("write a");
    fs::hard_link(root.join("top").join("a"), root.join("top").join("b")).expect("hard link");

    let map = map_with(&[(&["top", "a"], 1), (&["top", "b"], 1)]);
    let error = open(&root, map).expect_err("hard link must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::DuplicateFile,
            ..
        }
    ));
}

#[test]
fn rejects_unsanitized_components() {
    let dir = temp_dir();
    let root = root_in(&dir);

    for component in ["con", "..", "a/b", "a\\b", "C:"] {
        let error = open(&root, raw_map(&[component])).expect_err("unsafe component must fail");
        assert!(
            matches!(
                error,
                StorageError::UnsafePath {
                    reason: UnsafeReason::InvalidComponent,
                    ..
                }
            ),
            "component {component:?}"
        );
    }

    let error = open(&root, raw_map(&[])).expect_err("empty path must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::InvalidComponent,
            ..
        }
    ));
}

#[test]
fn accepts_missing_parents_and_reports_paths() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let storage = open(&root, map_with(&[(&["a", "b", "c"], 0)])).expect("missing parents are ok");

    assert_eq!(storage.file_count(), 1);
    let expected = root.join("a").join("b").join("c");
    assert_eq!(storage.file_path(0), Some(expected.as_path()));
    assert_eq!(storage.root(), root.as_path());
    assert_eq!(storage.map().files().len(), 1);
    assert_eq!(storage.config().max_open_handles, DEFAULT_MAX_OPEN_HANDLES);
    assert_eq!(storage.config().preallocation, Preallocation::Off);
    assert!(storage.file_path(1).is_none());
}

#[test]
fn accepts_existing_distinct_files() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::create_dir_all(root.join("top")).expect("create top");
    fs::write(root.join("top").join("a"), b"a").expect("write a");
    fs::write(root.join("top").join("b"), b"b").expect("write b");

    let map = map_with(&[(&["top", "a"], 1), (&["top", "b"], 1)]);
    let storage = open(&root, map).expect("distinct files are ok");
    assert_eq!(storage.file_count(), 2);
}

#[test]
fn off_creates_zero_length_files_and_parents() {
    let dir = temp_dir();
    let root = root_in(&dir);

    open(&root, map_with(&[(&["a", "empty"], 0)])).expect("open");

    let path = root.join("a").join("empty");
    assert!(path.is_file());
    assert_eq!(fs::metadata(&path).expect("metadata").len(), 0);
}

#[test]
fn off_leaves_nonzero_files_and_parents_missing() {
    let dir = temp_dir();
    let root = root_in(&dir);

    open(&root, map_with(&[(&["a", "data"], 10)])).expect("open");

    assert!(!root.join("a").exists());
    assert!(!root.join("a").join("data").exists());
}

#[test]
fn off_preserves_existing_file_content() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::create_dir_all(root.join("a")).expect("create dirs");
    fs::write(root.join("a").join("data"), b"keep").expect("write data");

    open(&root, map_with(&[(&["a", "data"], 10)])).expect("open");

    assert_eq!(
        fs::read(root.join("a").join("data")).expect("read"),
        b"keep"
    );
}

#[test]
fn set_len_creates_all_files_and_extends() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::create_dir_all(root.join("a")).expect("create dirs");
    fs::write(root.join("a").join("data"), b"abc").expect("write data");

    let map = map_with(&[(&["a", "data"], 5), (&["a", "empty"], 0)]);
    open_with(&root, map, set_len_config()).expect("open");

    let data = root.join("a").join("data");
    assert_eq!(fs::metadata(&data).expect("metadata").len(), 5);
    let bytes = fs::read(&data).expect("read data");
    assert_eq!(&bytes[..3], b"abc");
    assert_eq!(&bytes[3..], &[0, 0]);

    let empty = root.join("a").join("empty");
    assert_eq!(fs::metadata(&empty).expect("metadata").len(), 0);
}

#[test]
fn set_len_does_not_truncate_longer_file() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::create_dir_all(root.join("a")).expect("create dirs");
    fs::write(root.join("a").join("data"), b"0123456789").expect("write data");

    open_with(&root, map_with(&[(&["a", "data"], 5)]), set_len_config()).expect("open");

    assert_eq!(
        fs::read(root.join("a").join("data")).expect("read"),
        b"0123456789"
    );
}

#[test]
fn extend_if_short_never_shrinks() {
    let dir = temp_dir();
    let path = dir.path().join("file");
    fs::write(&path, b"0123456789").expect("write");

    extend_if_short(&path, 5).expect("short expectation is a no-op");
    assert_eq!(fs::read(&path).expect("read"), b"0123456789");

    extend_if_short(&path, 12).expect("extend");
    let bytes = fs::read(&path).expect("read");
    assert_eq!(&bytes[..10], b"0123456789");
    assert_eq!(&bytes[10..], &[0, 0]);
}

#[test]
fn existing_zero_length_file_is_never_truncated() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::create_dir_all(root.join("a")).expect("create dirs");
    fs::write(root.join("a").join("empty"), b"keep").expect("write file");

    open_with(&root, map_with(&[(&["a", "empty"], 0)]), set_len_config()).expect("open");

    assert_eq!(
        fs::read(root.join("a").join("empty")).expect("read"),
        b"keep"
    );
}

#[cfg(unix)]
#[test]
fn created_files_have_no_execute_bits() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    let root = root_in(&dir);
    open(&root, map_with(&[(&["a", "empty"], 0)])).expect("open");

    let path = root.join("a").join("empty");
    let mode = fs::metadata(&path).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o111, 0);
}

#[test]
fn ensure_parent_dirs_rejects_file_component() {
    let dir = temp_dir();
    let root = root_in(&dir);
    fs::write(root.join("a"), b"x").expect("write file");

    let error = ensure_parent_dirs(&root, &["a".to_owned()]).expect_err("file parent must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::NotDirectory,
            ..
        }
    ));
}

#[test]
fn ensure_parent_dirs_rejects_symlink() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).expect("create outside");
    if !create_dir_symlink(&outside, &root.join("a")) {
        return;
    }

    let before = snapshot(&outside);
    let error = ensure_parent_dirs(&root, &["a".to_owned()]).expect_err("symlink parent must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn write_range_creates_missing_files_and_parents() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage =
        open(&root, map_with(&[(&["a", "one"], 4), (&["a", "two"], 4)])).expect("open");

    storage.write_range(0, b"abcdefgh").expect("write");

    assert_eq!(
        fs::read(root.join("a").join("one")).expect("read one"),
        b"abcd"
    );
    assert_eq!(
        fs::read(root.join("a").join("two")).expect("read two"),
        b"efgh"
    );
}

#[test]
fn write_range_creates_only_touched_files() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage =
        open(&root, map_with(&[(&["a", "one"], 4), (&["a", "two"], 4)])).expect("open");

    storage.write_range(2, b"xy").expect("write");

    assert_eq!(
        fs::read(root.join("a").join("one")).expect("read one"),
        [0, 0, b'x', b'y']
    );
    assert!(!root.join("a").join("two").exists());
}

#[test]
fn read_range_reads_across_files() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage =
        open(&root, map_with(&[(&["a", "one"], 4), (&["a", "two"], 4)])).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");

    let mut buf = [0u8; 8];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 8);
    assert_eq!(&buf, b"abcdefgh");
}

#[test]
fn read_range_stops_at_short_file() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage =
        open(&root, map_with(&[(&["a", "one"], 4), (&["a", "two"], 4)])).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");
    fs::write(root.join("a").join("one"), b"ab").expect("truncate first");

    let mut buf = [0u8; 8];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 2);
    assert_eq!(&buf[..2], b"ab");
}

#[test]
fn read_range_stops_at_missing_file() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage =
        open(&root, map_with(&[(&["a", "one"], 4), (&["a", "two"], 4)])).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");
    fs::remove_file(root.join("a").join("two")).expect("remove second");

    let mut buf = [0u8; 8];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 4);
    assert_eq!(&buf[..4], b"abcd");
}

#[test]
fn read_range_does_not_skip_a_missing_prefix() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage =
        open(&root, map_with(&[(&["a", "one"], 4), (&["a", "two"], 4)])).expect("open");
    storage.write_range(4, b"efgh").expect("write second");

    let mut buf = [0u8; 8];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 0);
}

#[test]
fn read_range_ignores_bytes_past_mapped_length() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage =
        open(&root, map_with(&[(&["a", "one"], 4), (&["a", "two"], 4)])).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");

    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(root.join("a").join("one"))
        .expect("open first");
    file.write_all(b"EXTRA").expect("append");
    drop(file);

    let mut buf = [0u8; 8];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 8);
    assert_eq!(&buf, b"abcdefgh");
}

#[test]
fn ranges_ending_at_total_length_are_valid() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage = open(&root, map_with(&[(&["a", "data"], 4)])).expect("open");
    storage.write_range(0, b"abcd").expect("write");

    let mut tail = [0u8; 2];
    assert_eq!(storage.read_range(2, &mut tail).expect("read"), 2);
    assert_eq!(&tail, b"cd");

    let mut past = [0u8; 1];
    assert_eq!(storage.read_range(4, &mut past).expect("read"), 0);
    assert_eq!(storage.read_range(u64::MAX, &mut past).expect("read"), 0);
}

#[test]
fn zero_length_buffers_do_no_work() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage = open(&root, map_with(&[(&["a", "data"], 4)])).expect("open");

    assert_eq!(storage.read_range(0, &mut []).expect("read"), 0);
    storage.write_range(0, &[]).expect("write");
    assert!(!root.join("a").exists());
}

#[test]
fn write_range_rejects_out_of_range_and_overflow() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage = open(&root, map_with(&[(&["a", "data"], 4)])).expect("open");

    for (offset, data) in [(2u64, &b"abcde"[..]), (u64::MAX, &b"x"[..])] {
        let error = storage
            .write_range(offset, data)
            .expect_err("out of range must fail");
        assert!(
            matches!(error, StorageError::OutOfRange { .. }),
            "offset {offset}"
        );
    }
}

#[test]
fn read_then_write_reuses_a_writable_handle() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage = open(&root, map_with(&[(&["a", "data"], 4)])).expect("open");
    storage.write_range(0, b"abcd").expect("write");
    storage.close().expect("close");

    let mut buf = [0u8; 4];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 4);
    storage.write_range(0, b"wxyz").expect("write after read");

    assert_eq!(
        fs::read(root.join("a").join("data")).expect("read"),
        b"wxyz"
    );
}

#[test]
fn read_range_does_not_create_missing_files() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage = open(&root, map_with(&[(&["a", "data"], 4)])).expect("open");

    let mut buf = [0u8; 4];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 0);
    assert!(!root.join("a").exists());
}

#[test]
fn strict_handle_cap_evicts_without_syncing() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a"], 1), (&["b"], 1), (&["c"], 1)]);
    let mut storage = open_with(&root, map, cap_config(1)).expect("open");

    storage.write_range(0, b"xyz").expect("write");

    assert_eq!(storage.handles.entries.len(), 1);
    assert_eq!(storage.handles.entries[0].index, 2);
    assert_eq!(storage.handles.needs_sync.len(), 2);
    assert!(storage.handles.needs_sync.contains(&0));
    assert!(storage.handles.needs_sync.contains(&1));

    storage.sync().expect("sync");
    assert!(storage.handles.needs_sync.is_empty());
    assert!(!storage.handles.entries[0].dirty);

    storage.close().expect("close");
    assert!(storage.handles.entries.is_empty());
}

#[test]
fn sync_reopens_evicted_dirty_file() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a"], 1), (&["b"], 1)]);
    let mut storage = open_with(&root, map, cap_config(1)).expect("open");
    storage.write_range(0, b"ab").expect("write");

    fs::remove_file(root.join("a")).expect("remove evicted file");

    let error = storage.sync().expect_err("reopen must fail");
    assert!(matches!(
        error,
        StorageError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound
    ));
}

#[test]
fn sync_attempts_every_file_and_keeps_failures() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a", "one"], 4), (&["b", "two"], 4)]);
    let mut storage = open_with(&root, map, cap_config(1)).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");
    assert!(storage.handles.needs_sync.contains(&0));

    fs::remove_file(root.join("a").join("one")).expect("remove evicted file");

    let error = storage.sync().expect_err("reopen must fail");
    assert!(matches!(
        error,
        StorageError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound
    ));
    assert!(
        storage.handles.needs_sync.contains(&0),
        "failed file must stay pending"
    );
    assert!(
        !storage.handles.entries[0].dirty,
        "the cached file must still be flushed"
    );

    fs::write(root.join("a").join("one"), b"safe").expect("restore file");
    storage.sync().expect("retry must succeed");
    assert!(storage.handles.needs_sync.is_empty());
}

#[test]
fn close_keeps_state_when_the_flush_fails() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a", "one"], 4), (&["b", "two"], 4)]);
    let mut storage = open_with(&root, map, cap_config(1)).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");

    fs::remove_file(root.join("a").join("one")).expect("remove evicted file");

    let error = storage.close().expect_err("close must fail");
    assert!(matches!(
        error,
        StorageError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound
    ));
    assert!(
        storage.handles.needs_sync.contains(&0),
        "pending file must survive a failed close"
    );
    assert_eq!(
        storage.handles.entries.len(),
        1,
        "cache must survive a failed close"
    );

    fs::write(root.join("a").join("one"), b"safe").expect("restore file");
    storage.close().expect("retry must succeed");
    assert!(storage.handles.entries.is_empty());
    assert!(storage.handles.needs_sync.is_empty());
}

#[test]
fn sync_upgrades_a_read_only_handle_for_a_pending_file() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a", "one"], 4), (&["b", "two"], 4)]);
    let mut storage = open_with(&root, map, cap_config(1)).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");

    let mut buf = [0u8; 4];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 4);
    assert!(storage.handles.needs_sync.contains(&0));
    assert_eq!(storage.handles.entries[0].index, 0);
    assert!(!storage.handles.entries[0].writable);

    storage.sync().expect("sync");
    assert!(storage.handles.needs_sync.is_empty());
    assert!(storage.handles.entries[0].writable);
    assert!(!storage.handles.entries[0].dirty);
}

#[cfg(unix)]
#[test]
fn read_range_does_not_require_write_access_to_pending_files() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a", "one"], 4), (&["b", "two"], 4)]);
    let mut storage = open_with(&root, map, cap_config(1)).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");
    assert!(storage.handles.needs_sync.contains(&0));

    fs::set_permissions(
        root.join("a").join("one"),
        fs::Permissions::from_mode(0o444),
    )
    .expect("make read-only");

    let mut buf = [0u8; 4];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 4);
    assert_eq!(&buf, b"abcd");
}

#[test]
fn close_syncs_and_drops_handles() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage = open(&root, map_with(&[(&["a"], 4)])).expect("open");
    storage.write_range(0, b"data").expect("write");
    assert_eq!(storage.handles.entries.len(), 1);

    storage.close().expect("close");
    assert!(storage.handles.entries.is_empty());
    assert!(storage.handles.needs_sync.is_empty());

    let mut buf = [0u8; 4];
    assert_eq!(storage.read_range(0, &mut buf).expect("read"), 4);
    assert_eq!(&buf, b"data");
}

#[test]
fn read_range_rejects_symlink_swapped_file() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage = open(&root, map_with(&[(&["a", "data"], 4)])).expect("open");

    fs::create_dir_all(root.join("a")).expect("create parent");
    let outside = dir.path().join("outside");
    fs::write(&outside, b"secret").expect("write outside");
    if !create_file_symlink(&outside, &root.join("a").join("data")) {
        return;
    }

    let before = snapshot(&outside);
    let mut buf = [0u8; 4];
    let error = storage
        .read_range(0, &mut buf)
        .expect_err("symlink must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn write_range_rejects_symlinked_parent() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let mut storage = open(&root, map_with(&[(&["a", "data"], 4)])).expect("open");

    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).expect("create outside");
    fs::write(outside.join("data"), b"keep").expect("write outside");
    if !create_dir_symlink(&outside, &root.join("a")) {
        return;
    }

    let before = snapshot(&outside);
    let error = storage
        .write_range(0, b"evil")
        .expect_err("symlinked parent must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn read_range_rejects_swapped_parent() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a", "one"], 4), (&["b", "two"], 4)]);
    let mut storage = open_with(&root, map, cap_config(1)).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");
    assert!(storage.handles.needs_sync.contains(&0));

    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).expect("create outside");
    fs::write(outside.join("one"), b"evil").expect("write outside");
    fs::remove_dir_all(root.join("a")).expect("remove parent");
    if !create_dir_symlink(&outside, &root.join("a")) {
        return;
    }

    let before = snapshot(&outside);
    let mut buf = [0u8; 4];
    let error = storage
        .read_range(0, &mut buf)
        .expect_err("swapped parent must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn sync_reopen_revalidates_ancestors() {
    let dir = temp_dir();
    let root = root_in(&dir);
    let map = map_with(&[(&["a", "one"], 4), (&["b", "two"], 4)]);
    let mut storage = open_with(&root, map, cap_config(1)).expect("open");
    storage.write_range(0, b"abcdefgh").expect("write");
    assert!(storage.handles.needs_sync.contains(&0));

    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).expect("create outside");
    fs::write(outside.join("one"), b"evil").expect("write outside");
    fs::remove_dir_all(root.join("a")).expect("remove parent");
    if !create_dir_symlink(&outside, &root.join("a")) {
        return;
    }

    let before = snapshot(&outside);
    let error = storage.sync().expect_err("swapped parent must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert_eq!(snapshot(&outside), before);
}

#[test]
fn invalid_component_error_stays_under_root() {
    let dir = temp_dir();
    let root = root_in(&dir);

    for component in ["..", "/etc/passwd", "C:\\Windows"] {
        let error = open(&root, raw_map(&[component])).expect_err("unsafe component must fail");
        match error {
            StorageError::UnsafePath { path, reason } => {
                assert_eq!(reason, UnsafeReason::InvalidComponent, "{component:?}");
                assert_eq!(path, root, "{component:?}");
            }
            other => panic!("unexpected error for {component:?}: {other:?}"),
        }
    }

    let error = ensure_parent_dirs(&root, &["..".to_owned()]).expect_err("raw component must fail");
    match error {
        StorageError::UnsafePath { path, reason } => {
            assert_eq!(reason, UnsafeReason::InvalidComponent);
            assert_eq!(path, root);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn verify_opened_rejects_a_different_inode() {
    let dir = temp_dir();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    fs::write(&first, b"a").expect("write first");
    fs::write(&second, b"b").expect("write second");

    let metadata = fs::symlink_metadata(&first).expect("metadata");
    let expected = file_id(&metadata);
    assert!(expected.is_some());

    let file = open_read(&second).expect("open second");
    let error = verify_opened(&second, &file, expected).expect_err("mismatch must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Changed,
            ..
        }
    ));

    let file = open_read(&first).expect("open first");
    verify_opened(&first, &file, expected).expect("same inode");
}

#[test]
fn classify_maps_not_a_directory() {
    let error = classify(
        Path::new("file"),
        io::Error::from(io::ErrorKind::NotADirectory),
    );
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::NotDirectory,
            ..
        }
    ));
}

#[test]
fn write_all_at_retries_short_counts_and_interrupted() {
    let mut data = [0u8; 6];
    let mut step = 0;

    write_all_at(
        |buf, offset| {
            step += 1;
            let offset = offset as usize;
            match step {
                1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                2 => {
                    data[offset..offset + 2].copy_from_slice(&buf[..2]);
                    Ok(2)
                }
                3 => {
                    data[offset..offset + 1].copy_from_slice(&buf[..1]);
                    Ok(1)
                }
                _ => {
                    let len = buf.len();
                    data[offset..offset + len].copy_from_slice(buf);
                    Ok(len)
                }
            }
        },
        b"abcdef",
        0,
    )
    .expect("write");

    assert_eq!(&data, b"abcdef");
    assert_eq!(step, 4);
}

#[test]
fn write_all_at_zero_write_is_write_zero() {
    let error = write_all_at(|_, _| Ok(0), b"x", 0).expect_err("zero write must fail");
    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
}

#[test]
fn write_all_at_checks_offset_overflow() {
    let error = write_all_at(|_, _| Ok(1), b"xy", u64::MAX).expect_err("overflow must fail");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn read_up_to_retries_and_stops_at_eof() {
    let data = b"abcdef";
    let mut buf = [0u8; 8];
    let mut step = 0;

    let read = read_up_to(
        |buf, offset| {
            step += 1;
            let offset = offset as usize;
            match step {
                1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                2 => {
                    buf[..2].copy_from_slice(&data[offset..offset + 2]);
                    Ok(2)
                }
                3 => {
                    buf[..3].copy_from_slice(&data[offset..offset + 3]);
                    Ok(3)
                }
                _ => Ok(0),
            }
        },
        &mut buf,
        0,
    )
    .expect("read");

    assert_eq!(read, 5);
    assert_eq!(&buf[..5], b"abcde");
}

#[test]
fn read_up_to_checks_offset_overflow() {
    let mut buf = [0u8; 2];
    let error = read_up_to(
        |buf, _| {
            buf[0] = 1;
            Ok(1)
        },
        &mut buf,
        u64::MAX,
    )
    .expect_err("overflow must fail");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn classify_maps_disk_full() {
    let error = classify(
        Path::new("file"),
        io::Error::from(io::ErrorKind::StorageFull),
    );
    assert!(matches!(error, StorageError::DiskFull { .. }));
}

#[test]
fn classify_maps_quota_exceeded() {
    let error = classify(
        Path::new("file"),
        io::Error::from(io::ErrorKind::QuotaExceeded),
    );
    assert!(matches!(error, StorageError::DiskFull { .. }));
}

#[test]
fn classify_maps_permission_denied() {
    let error = classify(
        Path::new("file"),
        io::Error::from(io::ErrorKind::PermissionDenied),
    );
    assert!(matches!(error, StorageError::PermissionDenied { .. }));
}

#[test]
fn classify_maps_other_errors_to_io() {
    let error = classify(Path::new("file"), io::Error::from(io::ErrorKind::NotFound));
    assert!(matches!(error, StorageError::Io { .. }));
}

#[cfg(windows)]
#[test]
fn classify_maps_windows_disk_full_codes() {
    for code in [39, 112] {
        let error = classify(Path::new("file"), io::Error::from_raw_os_error(code));
        assert!(
            matches!(error, StorageError::DiskFull { .. }),
            "code {code}"
        );
    }
}

#[cfg(windows)]
#[test]
fn classify_maps_windows_file_in_use() {
    for code in [32, 33] {
        let error = classify(Path::new("file"), io::Error::from_raw_os_error(code));
        assert!(
            matches!(error, StorageError::FileInUse { .. }),
            "code {code}"
        );
    }
}

#[test]
fn system_time_ns_handles_epoch_and_overflow() {
    assert_eq!(system_time_ns(SystemTime::UNIX_EPOCH), Some(0));

    // Windows stores SystemTime in 100 ns ticks, so use a representable value
    // for the exact boundary instead of i64::MAX nanoseconds.
    let max_ns = (i64::MAX / 100) * 100;
    let max = Duration::from_nanos(u64::try_from(max_ns).expect("positive bound"));
    assert_eq!(system_time_ns(SystemTime::UNIX_EPOCH + max), Some(max_ns));
    assert_eq!(system_time_ns(SystemTime::UNIX_EPOCH - max), Some(-max_ns));

    // Clearly beyond the i64 nanosecond range and representable on every
    // supported platform (roughly 300 years from the epoch).
    let too_far = Duration::from_secs(300 * 365 * 24 * 60 * 60);
    assert_eq!(system_time_ns(SystemTime::UNIX_EPOCH + too_far), None);
    assert_eq!(system_time_ns(SystemTime::UNIX_EPOCH - too_far), None);
}

#[test]
fn system_time_ns_handles_pre_epoch_times() {
    let before = SystemTime::UNIX_EPOCH - Duration::from_secs(3_600);

    assert_eq!(system_time_ns(before), Some(-3_600_000_000_000));
}
