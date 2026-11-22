use std::io::ErrorKind;
use std::path::PathBuf;

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
fn file_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<File>();
}
