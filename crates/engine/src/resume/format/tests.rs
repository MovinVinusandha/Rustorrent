use super::*;
use crate::bencode::{self, Value};
use crate::bitfield::{Bitfield, BitfieldError};
use crate::storage::layout::{FileMap, MappedFile};

const INFO: [u8; 20] = [0x11; 20];
const LAYOUT: [u8; 20] = [0x22; 20];

fn document() -> ResumeDocument {
    ResumeDocument::new(
        INFO,
        4,
        8,
        LAYOUT,
        Bitfield::from_bytes(&[0b0000_0001], 2).expect("bitfield"),
        vec![ResumeFile::new(3, 3, 100), ResumeFile::new(5, 5, -200)],
        1234,
    )
}

fn file_value(length: i64, observed_size: i64, mtime_ns: i64) -> Value<'static> {
    Value::dict(vec![
        (b"length", Value::int(length)),
        (b"mtime_ns", Value::int(mtime_ns)),
        (b"observed_size", Value::int(observed_size)),
    ])
    .expect("file dictionary")
}

fn one_file(length: i64, observed_size: i64, mtime_ns: i64) -> Value<'static> {
    Value::list(vec![file_value(length, observed_size, mtime_ns)])
}

/// Builds a raw document so individual fields can be made invalid.
struct RawDocument {
    version: i64,
    info_hash: Vec<u8>,
    layout_hash: Vec<u8>,
    piece_length: i64,
    total_length: i64,
    piece_count: i64,
    bitfield: Vec<u8>,
    files: Value<'static>,
    updated_at: i64,
}

impl Default for RawDocument {
    fn default() -> Self {
        Self {
            version: 1,
            info_hash: INFO.to_vec(),
            layout_hash: LAYOUT.to_vec(),
            piece_length: 4,
            total_length: 8,
            piece_count: 2,
            bitfield: vec![0b0000_0001],
            files: one_file(8, 8, 0),
            updated_at: 0,
        }
    }
}

impl RawDocument {
    fn encode(&self) -> Vec<u8> {
        let value = Value::dict(vec![
            (b"bitfield", Value::bytes(&self.bitfield)),
            (b"files", self.files.clone()),
            (b"info_hash", Value::bytes(&self.info_hash)),
            (b"layout_hash", Value::bytes(&self.layout_hash)),
            (b"piece_count", Value::int(self.piece_count)),
            (b"piece_length", Value::int(self.piece_length)),
            (b"total_length", Value::int(self.total_length)),
            (b"updated_at", Value::int(self.updated_at)),
            (b"version", Value::int(self.version)),
        ])
        .expect("document dictionary");
        bencode::encode(&value).expect("encode")
    }
}

fn map_with(files: &[(&[&str], u64)]) -> FileMap {
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

    FileMap::from_mapped_files(mapped, offset, 1024, 1)
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[test]
fn round_trip_document() {
    let document = document();
    let encoded = encode_document(&document).expect("encode");

    assert!(encoded.starts_with(b"d8:bitfield"));
    assert_eq!(decode_document(&encoded).expect("decode"), document);
}

#[test]
fn round_trip_zero_pieces_and_negative_mtime() {
    let document = ResumeDocument::new(
        INFO,
        16,
        0,
        LAYOUT,
        Bitfield::new(0).expect("bitfield"),
        vec![ResumeFile::new(0, 0, -3_600_000_000_000)],
        -1,
    );
    let encoded = encode_document(&document).expect("encode");

    assert_eq!(decode_document(&encoded).expect("decode"), document);
}

#[test]
fn decode_rejects_every_truncated_prefix() {
    let encoded = encode_document(&document()).expect("encode");

    for end in 0..encoded.len() {
        assert!(
            decode_document(&encoded[..end]).is_err(),
            "prefix of {end} bytes decoded"
        );
    }
}

#[test]
fn decode_rejects_unknown_version() {
    let encoded = RawDocument {
        version: 2,
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::UnsupportedVersion { version: 2 })
    );
}

#[test]
fn decode_rejects_oversized_input_before_parsing() {
    let oversized = vec![0u8; MAX_RESUME_BYTES + 1];

    assert_eq!(
        decode_document(&oversized),
        Err(DecodeError::TooLarge {
            len: MAX_RESUME_BYTES + 1,
            max: MAX_RESUME_BYTES,
        })
    );
}

#[test]
fn decode_rejects_huge_declared_string_lengths() {
    assert!(decode_document(b"d8:bitfield99999999999999999999:").is_err());
}

#[test]
fn decode_rejects_bad_info_hash_length() {
    let encoded = RawDocument {
        info_hash: vec![0u8; 19],
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::WrongLength {
            field: "info_hash",
            expected: 20,
            found: 19,
        })
    );
}

#[test]
fn decode_rejects_bad_layout_hash_length() {
    let encoded = RawDocument {
        layout_hash: vec![0u8; 19],
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::WrongLength {
            field: "layout_hash",
            expected: 20,
            found: 19,
        })
    );
}

#[test]
fn decode_rejects_zero_piece_length() {
    let encoded = RawDocument {
        piece_length: 0,
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(decode_document(&encoded), Err(DecodeError::ZeroPieceLength));
}

#[test]
fn decode_rejects_negative_piece_count() {
    let encoded = RawDocument {
        piece_count: -1,
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::OutOfRange {
            field: "piece_count"
        })
    );
}

#[test]
fn decode_rejects_piece_count_above_u32() {
    let encoded = RawDocument {
        piece_count: i64::MAX,
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::OutOfRange {
            field: "piece_count"
        })
    );
}

#[test]
fn decode_rejects_negative_file_length() {
    let encoded = RawDocument {
        files: one_file(-1, 8, 0),
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::OutOfRange { field: "length" })
    );
}

#[test]
fn decode_rejects_bad_bitfield_length() {
    let encoded = RawDocument {
        total_length: 16,
        piece_count: 9,
        bitfield: vec![0b0000_0001],
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::Bitfield(BitfieldError::WrongLength {
            expected: 2,
            found: 1,
            piece_count: 9,
        }))
    );
}

#[test]
fn decode_rejects_trailing_bitfield_bits() {
    let encoded = RawDocument {
        total_length: 4,
        piece_count: 1,
        bitfield: vec![0b0000_0010],
        files: one_file(4, 4, 0),
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::Bitfield(BitfieldError::TrailingBitsSet {
            piece_count: 1,
        }))
    );
}

#[test]
fn decode_rejects_non_dictionary_input() {
    assert_eq!(decode_document(b"i1e"), Err(DecodeError::NotADictionary));
}

#[test]
fn decode_rejects_files_that_are_not_a_list() {
    let encoded = RawDocument {
        files: Value::int(1),
        ..RawDocument::default()
    }
    .encode();

    assert_eq!(
        decode_document(&encoded),
        Err(DecodeError::WrongType { field: "files" })
    );
}

#[test]
fn decode_rejects_duplicate_dictionary_keys() {
    assert!(matches!(
        decode_document(b"d1:ai1e1:ai2ee"),
        Err(DecodeError::Bencode(_))
    ));
}

#[test]
fn encode_rejects_integer_out_of_range() {
    let document = ResumeDocument::new(
        INFO,
        4,
        8,
        LAYOUT,
        Bitfield::from_bytes(&[0b0000_0001], 2).expect("bitfield"),
        vec![ResumeFile::new(u64::MAX, 0, 0)],
        0,
    );

    assert_eq!(
        encode_document(&document),
        Err(EncodeError::IntegerOutOfRange { field: "length" })
    );
}

#[test]
fn encode_rejects_bitfield_piece_count_mismatch() {
    let mut document = ResumeDocument::new(
        INFO,
        4,
        8,
        LAYOUT,
        Bitfield::new(1).expect("bitfield"),
        vec![ResumeFile::new(8, 8, 0)],
        0,
    );
    document.piece_count = 2;

    assert_eq!(
        encode_document(&document),
        Err(EncodeError::BitfieldCount {
            expected: 2,
            found: 1,
        })
    );
}

#[test]
fn layout_fingerprint_is_stable_and_covers_the_layout() {
    let base = map_with(&[(&["t", "a.txt"], 3), (&["t", "b.bin"], 5)]);
    let same = map_with(&[(&["t", "a.txt"], 3), (&["t", "b.bin"], 5)]);
    let renamed = map_with(&[(&["t", "c.txt"], 3), (&["t", "b.bin"], 5)]);
    let resized = map_with(&[(&["t", "a.txt"], 4), (&["t", "b.bin"], 5)]);
    let reordered = map_with(&[(&["t", "b.bin"], 5), (&["t", "a.txt"], 3)]);
    let split = map_with(&[(&["t", "a"], 3), (&["t", "txt"], 5)]);

    let fingerprint = layout_fingerprint(&base);
    assert_eq!(fingerprint, layout_fingerprint(&same));
    assert_ne!(fingerprint, layout_fingerprint(&renamed));
    assert_ne!(fingerprint, layout_fingerprint(&resized));
    assert_ne!(fingerprint, layout_fingerprint(&reordered));
    assert_ne!(fingerprint, layout_fingerprint(&split));
    assert_eq!(
        hex(&fingerprint),
        "e0dc5b033565df21a8bff5c025f80f9f932454e7"
    );
}

#[test]
fn encoded_document_is_canonical() {
    let encoded = encode_document(&document()).expect("encode");

    assert!(encoded.starts_with(b"d8:bitfield1:\x01"));
    assert!(encoded.ends_with(b"7:versioni1ee"));
    assert_eq!(RESUME_VERSION, 1);
}
