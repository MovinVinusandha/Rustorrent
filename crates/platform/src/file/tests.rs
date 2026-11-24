use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::*;

fn temp_dir() -> TempDir {
    TempDir::new().expect("create temp dir")
}

fn path_in(dir: &TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

fn write_all_at(file: &File, mut data: &[u8], mut offset: u64) {
    while !data.is_empty() {
        let written = file.write_at(data, offset).expect("write_at");
        assert!(written > 0, "write_at made no progress");
        data = &data[written..];
        offset += written as u64;
    }
}

fn read_exact_at(file: &File, buf: &mut [u8], mut offset: u64) {
    let mut filled = 0;
    while filled < buf.len() {
        let read = file.read_at(&mut buf[filled..], offset).expect("read_at");
        assert!(read > 0, "unexpected EOF");
        filled += read;
        offset += read as u64;
    }
}

#[test]
fn write_then_read_at_offsets() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");

    write_all_at(&file, b"hello", 0);
    write_all_at(&file, b"world", 5);

    let mut buf = [0u8; 10];
    read_exact_at(&file, &mut buf, 0);
    assert_eq!(&buf, b"helloworld");

    let mut word = [0u8; 5];
    read_exact_at(&file, &mut word, 5);
    assert_eq!(&word, b"world");
}

#[test]
fn zero_length_buffers_return_zero() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");

    assert_eq!(file.write_at(&[], 0).expect("write"), 0);
    assert_eq!(file.read_at(&mut [], 0).expect("read"), 0);
}

#[test]
fn read_returns_zero_past_eof() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");
    write_all_at(&file, b"abc", 0);

    let mut buf = [0u8; 4];
    assert_eq!(file.read_at(&mut buf, 10).expect("read"), 0);
}

#[test]
fn read_returns_short_count() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");
    write_all_at(&file, b"abc", 0);

    let mut buf = [0xAAu8; 5];
    assert_eq!(file.read_at(&mut buf, 0).expect("read"), 3);
    assert_eq!(&buf[..3], b"abc");
    assert_eq!(&buf[3..], &[0xAA, 0xAA]);
}

#[test]
fn write_beyond_eof_zero_fills_the_gap() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");

    write_all_at(&file, b"x", 4);
    assert_eq!(file.len().expect("len"), 5);

    let mut buf = [0xFFu8; 4];
    read_exact_at(&file, &mut buf, 0);
    assert_eq!(buf, [0, 0, 0, 0]);
}

#[test]
fn create_new_does_not_truncate_an_existing_file() {
    let dir = temp_dir();
    let path = path_in(&dir, "data");
    let file = create_new(&path).expect("create");
    write_all_at(&file, b"keep", 0);

    let error = create_new(&path).expect_err("second create must fail");
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).expect("read file"), b"keep");
}

#[test]
fn open_write_creates_missing_file_without_truncating() {
    let dir = temp_dir();
    let path = path_in(&dir, "data");

    let file = open_write(&path).expect("create");
    assert_eq!(file.len().expect("len"), 0);
    write_all_at(&file, b"keep", 0);

    let file = open_write(&path).expect("reopen");
    assert_eq!(file.len().expect("len"), 4);
    write_all_at(&file, b"K", 0);
    assert_eq!(std::fs::read(&path).expect("read file"), b"Keep");
}

#[test]
fn open_rw_opens_existing_without_truncating() {
    let dir = temp_dir();
    let path = path_in(&dir, "data");
    std::fs::write(&path, b"keep").expect("write file");

    let file = open_rw(&path).expect("open");
    write_all_at(&file, b"K", 0);
    assert_eq!(std::fs::read(&path).expect("read file"), b"Keep");
}

#[test]
fn open_rw_missing_file_returns_not_found() {
    let dir = temp_dir();
    let error = open_rw(&path_in(&dir, "missing")).expect_err("open must fail");
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

#[test]
fn open_read_reads_existing_file() {
    let dir = temp_dir();
    let path = path_in(&dir, "data");
    std::fs::write(&path, b"content").expect("write file");

    let file = open_read(&path).expect("open");
    let mut buf = [0u8; 7];
    read_exact_at(&file, &mut buf, 0);
    assert_eq!(&buf, b"content");
}

#[test]
fn open_read_missing_file_returns_not_found() {
    let dir = temp_dir();
    let error = open_read(&path_in(&dir, "missing")).expect_err("open must fail");
    assert_eq!(error.kind(), ErrorKind::NotFound);
}

#[test]
fn metadata_matches_len() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");
    write_all_at(&file, b"abc", 0);

    assert_eq!(file.metadata().expect("metadata").len(), 3);
    assert_eq!(file.len().expect("len"), 3);
}

#[test]
fn empty_file_reports_empty() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");
    assert!(file.is_empty().expect("is_empty"));

    write_all_at(&file, b"x", 0);
    assert!(!file.is_empty().expect("is_empty"));
}

#[test]
fn set_len_extends_with_zeros_and_shrinks() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");
    write_all_at(&file, b"abc", 0);

    file.set_len(5).expect("extend");
    assert_eq!(file.len().expect("len"), 5);

    let mut buf = [0xFFu8; 5];
    read_exact_at(&file, &mut buf, 0);
    assert_eq!(&buf[..3], b"abc");
    assert_eq!(&buf[3..], &[0, 0]);

    file.set_len(1).expect("shrink");
    assert_eq!(file.len().expect("len"), 1);
}

#[test]
fn sync_methods_succeed() {
    let dir = temp_dir();
    let file = create_new(&path_in(&dir, "data")).expect("create");
    write_all_at(&file, b"data", 0);

    file.sync_data().expect("sync_data");
    file.sync_all().expect("sync_all");
}

#[test]
fn write_atomic_creates_target_without_leaving_a_temp_file() {
    let dir = temp_dir();
    let target = path_in(&dir, "0000.resume");

    write_atomic(&target, b"first").expect("write atomic");

    assert_eq!(std::fs::read(&target).expect("read target"), b"first");
    assert!(!target.with_file_name("0000.resume.tmp").exists());
}

#[test]
fn write_atomic_replaces_an_existing_target() {
    let dir = temp_dir();
    let target = path_in(&dir, "0000.resume");
    std::fs::write(&target, b"old").expect("write old target");

    write_atomic(&target, b"new").expect("write atomic");

    assert_eq!(std::fs::read(&target).expect("read target"), b"new");
}

#[test]
fn write_atomic_overwrites_a_leftover_truncated_temp_file() {
    let dir = temp_dir();
    let target = path_in(&dir, "0000.resume");
    let temp = path_in(&dir, "0000.resume.tmp");
    std::fs::write(&target, b"old").expect("write old target");
    std::fs::write(&temp, b"d3:cor").expect("write leftover temp");

    // A leftover temp file must never change what the target contains.
    assert_eq!(std::fs::read(&target).expect("read target"), b"old");

    write_atomic(&target, b"new").expect("write atomic");

    assert_eq!(std::fs::read(&target).expect("read target"), b"new");
    assert!(!temp.exists());
}

#[test]
fn write_atomic_failure_keeps_the_target_and_removes_the_temp() {
    let dir = temp_dir();
    let target = path_in(&dir, "target");
    std::fs::create_dir(&target).expect("create target directory");
    let temp = path_in(&dir, "target.tmp");

    let result = write_atomic(&target, b"new");

    assert!(result.is_err(), "rename over a directory must fail");
    assert!(target.is_dir(), "target directory must be unchanged");
    assert!(!temp.exists(), "failed write must clean up its temp file");
}

#[test]
fn write_atomic_empty_contents() {
    let dir = temp_dir();
    let target = path_in(&dir, "0000.resume");
    std::fs::write(&target, b"old").expect("write old target");

    write_atomic(&target, b"").expect("write atomic");

    assert_eq!(std::fs::read(&target).expect("read target"), b"");
}

#[test]
fn write_atomic_rejects_a_path_without_a_file_name() {
    let error = write_atomic(Path::new(""), b"data").expect_err("must fail");

    assert_eq!(error.kind(), ErrorKind::InvalidInput);
}

#[test]
fn temp_path_appends_the_suffix_to_the_full_name() {
    let path = Path::new("/tmp/abc.resume");

    assert_eq!(
        temp_path(path).expect("temp path"),
        Path::new("/tmp/abc.resume.tmp")
    );
}

#[cfg(windows)]
#[test]
fn windows_sharing_violations_are_retryable() {
    assert!(is_sharing_violation(&std::io::Error::from_raw_os_error(32)));
    assert!(is_sharing_violation(&std::io::Error::from_raw_os_error(33)));
    assert!(!is_sharing_violation(&std::io::Error::from_raw_os_error(5)));
}

#[test]
fn file_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<File>();
}
