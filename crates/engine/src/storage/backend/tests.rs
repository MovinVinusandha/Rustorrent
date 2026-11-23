use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

    let error = open(&root, map_with(&[(&["top", "x"], 0)])).expect_err("symlink must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert!(!outside.join("x").exists());
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

    let error = open(&root, map_with(&[(&["top", "a"], 6)])).expect_err("symlink must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
    assert_eq!(fs::read(&outside).expect("read outside"), b"secret");
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

    let error = ensure_parent_dirs(&root, &["a".to_owned()]).expect_err("symlink parent must fail");
    assert!(matches!(
        error,
        StorageError::UnsafePath {
            reason: UnsafeReason::Symlink,
            ..
        }
    ));
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
