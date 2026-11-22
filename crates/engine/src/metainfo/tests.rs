use super::*;

// Fixture info dictionaries, copied byte for byte.
//
// Single-file: `file.txt` holds `hello` (5 bytes), piece length 4, so the two
// piece hashes are SHA-1 of `hell` and `o`.
//
// Info-hashes were computed independently of this crate:
//
//     python3 -c 'import hashlib,sys; print(hashlib.sha1(bytes.fromhex(sys.argv[1])).hexdigest())' <hex>
const SINGLE_INFO: &[u8] = b"d6:lengthi5e4:name8:file.txt12:piece lengthi4e6:pieces40:\xa5\xce\xc7\xaf_z\xabv\x9c\xf0\xd4\xaaD\x0e\x01\xc7\xbf\xc3q\xb2z\x81\xaf>Y\x1a\xc7\x13\xf8\x1e\xa1\xef\xe9=\xcf6\x15}\x83ve";
const SINGLE_INFO_HASH: &str = "269d5c49e062c6b94788155e71659e472c752c0d";

// Multi-file: directory `dir` with `a.txt` (1 byte) and `b.txt` (2 bytes),
// piece length 4, so one piece hash covers `abc`.
const MULTI_INFO: &[u8] = b"d5:filesld6:lengthi1e4:pathl5:a.txteed6:lengthi2e4:pathl5:b.txteee4:name3:dir12:piece lengthi4e6:pieces20:\xa9\x99>6G\x06\x81j\xba>%qxP\xc2l\x9c\xd0\xd8\x9de";
const MULTI_INFO_HASH: &str = "6fea2a866e2f72f21dc5c1047b9af51d3f3d3a12";

fn bencode_str(text: &str) -> Vec<u8> {
    let mut out = format!("{}:", text.len()).into_bytes();
    out.extend_from_slice(text.as_bytes());
    out
}

fn announce(url: &str) -> Vec<u8> {
    let mut out = bencode_str("announce");
    out.extend_from_slice(&bencode_str(url));
    out
}

fn bencode_announce_list(tiers: &[&[&str]]) -> Vec<u8> {
    let mut out = bencode_str("announce-list");
    out.push(b'l');
    for tier in tiers {
        out.push(b'l');
        for url in *tier {
            out.extend_from_slice(&bencode_str(url));
        }
        out.push(b'e');
    }
    out.push(b'e');
    out
}

/// Builds a metainfo file around an exact `info` dictionary.
fn wrap(info: &[u8], extra: &[u8]) -> Vec<u8> {
    let mut torrent = Vec::new();
    torrent.push(b'd');
    torrent.extend_from_slice(extra);
    torrent.extend_from_slice(b"4:info");
    torrent.extend_from_slice(info);
    torrent.push(b'e');
    torrent
}

/// Single-file `info` with the given name, length, piece length and pieces.
fn info_with_name(name: &str, length: i64, piece_length: u64, pieces: &[u8]) -> Vec<u8> {
    let mut info = Vec::new();
    info.push(b'd');
    info.extend_from_slice(format!("6:lengthi{length}e4:name").as_bytes());
    info.extend_from_slice(&bencode_str(name));
    info.extend_from_slice(format!("12:piece lengthi{piece_length}e").as_bytes());
    info.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
    info.extend_from_slice(pieces);
    info.push(b'e');
    info
}

/// Single-file `info` named `x`.
fn info_with(length: i64, piece_length: u64, pieces: &[u8]) -> Vec<u8> {
    info_with_name("x", length, piece_length, pieces)
}

/// Multi-file `info` with explicit paths and lengths.
fn multi_info_with(
    name: &str,
    files: &[(&[&str], i64)],
    piece_length: u64,
    pieces: &[u8],
) -> Vec<u8> {
    let mut info = Vec::new();
    info.push(b'd');
    info.extend_from_slice(b"5:filesl");
    for (path, length) in files {
        info.extend_from_slice(format!("d6:lengthi{length}e4:pathl").as_bytes());
        for component in *path {
            info.extend_from_slice(&bencode_str(component));
        }
        info.extend_from_slice(b"ee");
    }
    info.push(b'e');
    info.extend_from_slice(&bencode_str("name"));
    info.extend_from_slice(&bencode_str(name));
    info.extend_from_slice(format!("12:piece lengthi{piece_length}e").as_bytes());
    info.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
    info.extend_from_slice(pieces);
    info.push(b'e');
    info
}

/// A zero-length single-file `info` with extra keys appended.
fn minimal_info(extra: &[u8]) -> Vec<u8> {
    let mut info = info_with(0, 16384, b"");
    info.pop();
    info.extend_from_slice(extra);
    info.push(b'e');
    info
}

#[test]
fn parses_single_file_torrent() {
    let torrent = wrap(SINGLE_INFO, &announce("http://tracker.test/announce"));
    let info = MetaInfo::parse(&torrent).unwrap();

    assert_eq!(info.info_hash.to_string(), SINGLE_INFO_HASH);
    assert_eq!(info.info_hash.as_bytes().len(), PIECE_HASH_LENGTH);
    assert_eq!(info.mode, Mode::Single);
    assert_eq!(info.name, "file.txt");
    assert_eq!(info.total_length, 5);
    assert_eq!(info.piece_length, 4);
    assert_eq!(
        info.files,
        vec![FileInfo {
            path: vec!["file.txt".to_owned()],
            length: 5,
        }]
    );
    assert_eq!(
        info.announce.as_deref(),
        Some("http://tracker.test/announce")
    );
    assert!(info.announce_list.is_empty());
    assert!(!info.private);
    assert_eq!(info.piece_hashes.len(), 2);
    assert_eq!(
        info.piece_hashes[0],
        *b"\xa5\xce\xc7\xaf_z\xabv\x9c\xf0\xd4\xaaD\x0e\x01\xc7\xbf\xc3q\xb2"
    );
    assert_eq!(
        info.piece_hashes[1],
        *b"z\x81\xaf>Y\x1a\xc7\x13\xf8\x1e\xa1\xef\xe9=\xcf6\x15}\x83v"
    );
}

#[test]
fn parses_multi_file_torrent() {
    let tiers: &[&[&str]] = &[
        &["udp://tracker.test:6969"],
        &["http://backup.test/announce"],
    ];
    let mut extra = announce("udp://tracker.test:6969");
    extra.extend_from_slice(&bencode_announce_list(tiers));

    let info = MetaInfo::parse(&wrap(MULTI_INFO, &extra)).unwrap();

    assert_eq!(info.info_hash.to_string(), MULTI_INFO_HASH);
    assert_eq!(info.mode, Mode::Multi);
    assert_eq!(info.name, "dir");
    assert_eq!(info.total_length, 3);
    assert_eq!(info.piece_length, 4);
    assert_eq!(
        info.files,
        vec![
            FileInfo {
                path: vec!["dir".to_owned(), "a.txt".to_owned()],
                length: 1,
            },
            FileInfo {
                path: vec!["dir".to_owned(), "b.txt".to_owned()],
                length: 2,
            },
        ]
    );
    assert_eq!(info.piece_hashes.len(), 1);
    assert_eq!(
        info.piece_hashes[0],
        *b"\xa9\x99>6G\x06\x81j\xba>%qxP\xc2l\x9c\xd0\xd8\x9d"
    );
    assert_eq!(
        info.announce_list,
        vec![
            vec!["udp://tracker.test:6969".to_owned()],
            vec!["http://backup.test/announce".to_owned()],
        ]
    );
}

#[test]
fn info_hash_uses_raw_info_bytes_with_unsorted_keys() {
    // `name` comes before `length`, so strict decoding rejects this and a
    // re-encoding would not reproduce the input.
    let raw_info = b"d4:name1:x6:lengthi0e12:piece lengthi16384e6:pieces0:e";

    let options = DecodeOptions {
        key_order: KeyOrder::Lenient,
        ..DecodeOptions::default()
    };
    let value = bencode::decode_with_options(raw_info, options).unwrap();
    assert_ne!(bencode::encode(&value).unwrap(), raw_info);

    let info = MetaInfo::parse(&wrap(raw_info, b"")).unwrap();
    assert_eq!(
        info.info_hash.to_string(),
        "fc8a1fe641fc6bd121da018b87789635fe95cd36"
    );
    assert_eq!(info.name, "x");
}

#[test]
fn announce_is_optional_and_lenient() {
    let cases: &[(&[u8], Option<&str>)] = &[
        (b"", None),
        (b"8:announce0:", None),
        (b"8:announcei5e", None),
        (b"8:announce3:\xff\xfe\xfd", None),
        (b"8:announce1:x", Some("x")),
    ];

    for &(extra, expected) in cases {
        let info = MetaInfo::parse(&wrap(SINGLE_INFO, extra)).unwrap();
        assert_eq!(info.announce.as_deref(), expected, "extra {extra:?}");
    }
}

#[test]
fn announce_list_drops_invalid_entries_and_empty_tiers() {
    let extra = b"13:announce-listll1:a1:beli5eelel3:\xff\xfe\xfdee";
    let info = MetaInfo::parse(&wrap(SINGLE_INFO, extra)).unwrap();

    assert_eq!(
        info.announce_list,
        vec![vec!["a".to_owned(), "b".to_owned()]]
    );
}

#[test]
fn announce_list_wrong_type_is_ignored() {
    let info = MetaInfo::parse(&wrap(SINGLE_INFO, b"13:announce-listi5e")).unwrap();
    assert!(info.announce_list.is_empty());
}

#[test]
fn private_is_true_only_for_integer_one() {
    let cases: &[(&[u8], bool)] = &[
        (b"", false),
        (b"7:privatei1e", true),
        (b"7:privatei0e", false),
        (b"7:privatei2e", false),
        (b"7:privatei-1e", false),
        (b"7:private1:x", false),
    ];

    for &(extra, expected) in cases {
        let torrent = wrap(&minimal_info(extra), b"");
        assert_eq!(
            MetaInfo::parse(&torrent).unwrap().private,
            expected,
            "extra {extra:?}"
        );
    }
}

#[test]
fn enforces_piece_length_limit() {
    let torrent = wrap(&info_with(0, MAX_PIECE_LENGTH, b""), b"");
    assert!(MetaInfo::parse(&torrent).is_ok());

    let torrent = wrap(&info_with(0, MAX_PIECE_LENGTH + 1, b""), b"");
    assert_eq!(
        MetaInfo::parse(&torrent),
        Err(Error::PieceLengthTooLarge {
            len: MAX_PIECE_LENGTH + 1,
            max: MAX_PIECE_LENGTH,
        })
    );
}

#[test]
fn rejects_zero_piece_length() {
    let torrent = wrap(&info_with(0, 0, b""), b"");
    assert_eq!(MetaInfo::parse(&torrent), Err(Error::ZeroPieceLength));
}

#[test]
fn accepts_zero_length_single_file_without_pieces() {
    let info = MetaInfo::parse(&wrap(&info_with(0, 16384, b""), b"")).unwrap();

    assert_eq!(info.mode, Mode::Single);
    assert_eq!(info.total_length, 0);
    assert!(info.piece_hashes.is_empty());
    assert_eq!(
        info.files,
        vec![FileInfo {
            path: vec!["x".to_owned()],
            length: 0,
        }]
    );
}

#[test]
fn piece_count_must_match_total_length() {
    let one_hash = [0u8; PIECE_HASH_LENGTH];
    let torrent = wrap(&info_with(5, 4, &one_hash), b"");
    assert_eq!(
        MetaInfo::parse(&torrent),
        Err(Error::PieceCountMismatch {
            expected: 2,
            found: 1,
        })
    );

    let three_hashes = [0u8; 3 * PIECE_HASH_LENGTH];
    let torrent = wrap(&info_with(5, 4, &three_hashes), b"");
    assert_eq!(
        MetaInfo::parse(&torrent),
        Err(Error::PieceCountMismatch {
            expected: 2,
            found: 3,
        })
    );

    // Exact multiples of the piece length need no extra piece.
    let two_hashes = [0u8; 2 * PIECE_HASH_LENGTH];
    let torrent = wrap(&info_with(8, 4, &two_hashes), b"");
    assert!(MetaInfo::parse(&torrent).is_ok());
}

#[test]
fn rejects_pieces_length_not_multiple_of_twenty() {
    let torrent = wrap(&info_with(0, 16384, &[0u8; 21]), b"");
    assert_eq!(
        MetaInfo::parse(&torrent),
        Err(Error::InvalidPiecesLength { len: 21 })
    );
}

#[test]
fn rejects_v2_only_metainfo() {
    let v2_info = b"d6:lengthi0e12:meta versioni2e4:name1:x12:piece lengthi16384ee";
    let torrent = wrap(v2_info, b"");
    assert_eq!(MetaInfo::parse(&torrent), Err(Error::UnsupportedV2));
}

#[test]
fn parses_hybrid_metainfo_as_v1() {
    let hybrid_info = b"d6:lengthi0e12:meta versioni2e4:name1:x12:piece lengthi16384e6:pieces0:e";
    let info = MetaInfo::parse(&wrap(hybrid_info, b"")).unwrap();

    assert_eq!(info.mode, Mode::Single);
    assert_eq!(info.total_length, 0);
    assert!(info.piece_hashes.is_empty());
}

#[test]
fn requires_exactly_one_layout_key() {
    let neither = b"d4:name1:x12:piece lengthi16384e6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(neither, b"")),
        Err(Error::MissingLayout)
    );

    let both = b"d5:filesle6:lengthi0e4:name1:x12:piece lengthi16384e6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(both, b"")),
        Err(Error::AmbiguousLayout)
    );
}

#[test]
fn rejects_empty_files_list() {
    let info = b"d5:filesle4:name1:x12:piece lengthi16384e6:pieces0:e";
    assert_eq!(MetaInfo::parse(&wrap(info, b"")), Err(Error::EmptyFiles));
}

#[test]
fn rejects_empty_path_list() {
    let info = b"d5:filesld6:lengthi0e4:pathleee4:name1:x12:piece lengthi16384e6:pieces0:e";
    assert_eq!(MetaInfo::parse(&wrap(info, b"")), Err(Error::EmptyPath));
}

#[test]
fn rejects_negative_lengths() {
    let single = b"d6:lengthi-1e4:name1:x12:piece lengthi16384e6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(single, b"")),
        Err(Error::NegativeLength {
            field: "length",
            value: -1,
        })
    );

    let multi = b"d5:filesld6:lengthi-1e4:pathl1:aeee4:name1:x12:piece lengthi16384e6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(multi, b"")),
        Err(Error::NegativeLength {
            field: "length",
            value: -1,
        })
    );
}

#[test]
fn rejects_total_length_overflow() {
    let mut info = Vec::from(&b"d5:filesl"[..]);
    for component in *b"abc" {
        info.extend_from_slice(b"d6:lengthi9223372036854775807e4:pathl1:");
        info.push(component);
        info.extend_from_slice(b"ee");
    }
    info.extend_from_slice(b"e4:name1:x12:piece lengthi16384e6:pieces0:e");

    assert_eq!(
        MetaInfo::parse(&wrap(&info, b"")),
        Err(Error::TotalLengthOverflow)
    );
}

#[test]
fn rejects_invalid_utf8_text_fields() {
    let name = b"d6:lengthi0e4:name2:\xff\xfe12:piece lengthi16384e6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(name, b"")),
        Err(Error::InvalidUtf8 { field: "name" })
    );

    let path =
        b"d5:filesld6:lengthi0e4:pathl2:\xff\xfeeee4:name1:x12:piece lengthi16384e6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(path, b"")),
        Err(Error::InvalidUtf8 { field: "path" })
    );
}

#[test]
fn rejects_missing_or_mistyped_fields() {
    let missing_name = b"d6:lengthi0e12:piece lengthi16384e6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(missing_name, b"")),
        Err(Error::MissingField { field: "name" })
    );

    let mistyped_name = b"d6:lengthi0e4:namei1e12:piece lengthi16384e6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(mistyped_name, b"")),
        Err(Error::WrongType { field: "name" })
    );

    let mistyped_pieces = b"d6:lengthi0e4:name1:x12:piece lengthi16384e6:piecesi1ee";
    assert_eq!(
        MetaInfo::parse(&wrap(mistyped_pieces, b"")),
        Err(Error::WrongType { field: "pieces" })
    );

    let missing_piece_length = b"d6:lengthi0e4:name1:x6:pieces0:e";
    assert_eq!(
        MetaInfo::parse(&wrap(missing_piece_length, b"")),
        Err(Error::MissingField {
            field: "piece length",
        })
    );
}

#[test]
fn rejects_malformed_top_level_structure() {
    assert_eq!(MetaInfo::parse(b"le"), Err(Error::NotADictionary));
    assert_eq!(MetaInfo::parse(b"de"), Err(Error::MissingInfo));
    assert_eq!(
        MetaInfo::parse(b"d4:infoi1ee"),
        Err(Error::InfoNotADictionary)
    );
    assert!(matches!(
        MetaInfo::parse(b"x"),
        Err(Error::Bencode(bencode::Error::InvalidToken { .. }))
    ));
}

#[test]
fn rejects_duplicate_info_keys() {
    let info = b"d6:lengthi0e6:lengthi0e4:name1:x12:piece lengthi16384e6:pieces0:e";
    assert!(matches!(
        MetaInfo::parse(&wrap(info, b"")),
        Err(Error::Bencode(bencode::Error::DuplicateDictKey { .. }))
    ));
}

#[test]
fn rejects_unsafe_name_components() {
    let cases = ["", ".", "..", "a/b", "a\0b", "a\nb", "a\u{7f}b"];

    for name in cases {
        let torrent = wrap(&info_with_name(name, 0, 16384, b""), b"");
        let result = MetaInfo::parse(&torrent);
        assert!(
            matches!(
                result,
                Err(Error::EmptyPathComponent { .. } | Error::UnsafePathComponent { .. })
            ),
            "name {name:?} parsed as {result:?}"
        );
    }
}

#[test]
fn rejects_unsafe_path_components() {
    let cases = ["", ".", "..", "a/b", "a\0b", "a\tb", "a\u{7f}b"];

    for component in cases {
        let path: &[&str] = &[component];
        let info = multi_info_with("dir", &[(path, 0)], 16384, b"");
        let result = MetaInfo::parse(&wrap(&info, b""));
        assert!(
            matches!(
                result,
                Err(Error::EmptyPathComponent { .. } | Error::UnsafePathComponent { .. })
            ),
            "component {component:?} parsed as {result:?}"
        );
    }
}

#[test]
fn accepts_components_that_storage_will_sanitize() {
    let cases = [
        "..\\..\\x",
        "C:\\x",
        "C:x",
        "con",
        "a.",
        "a ",
        "a:b",
        "a*b",
        "a?b",
        "a\"b",
        "a<b",
        "a>b",
        "a|b",
    ];

    for component in cases {
        let torrent = wrap(&info_with_name(component, 0, 16384, b""), b"");
        assert!(MetaInfo::parse(&torrent).is_ok(), "name {component:?}");

        let path: &[&str] = &[component];
        let info = multi_info_with("dir", &[(path, 0)], 16384, b"");
        assert!(
            MetaInfo::parse(&wrap(&info, b"")).is_ok(),
            "path {component:?}"
        );
    }
}

#[test]
fn rejects_duplicate_file_paths() {
    let a: &[&str] = &["a.txt"];
    let b: &[&str] = &["a.txt"];
    let info = multi_info_with("dir", &[(a, 1), (b, 2)], 16384, &[0u8; 20]);

    assert_eq!(
        MetaInfo::parse(&wrap(&info, b"")),
        Err(Error::DuplicatePath {
            path: "dir/a.txt".to_owned(),
        })
    );
}

#[test]
fn duplicate_detection_is_exact_and_case_sensitive() {
    let lower: &[&str] = &["a.txt"];
    let upper: &[&str] = &["A.txt"];
    let info = multi_info_with("dir", &[(lower, 1), (upper, 2)], 16384, &[0u8; 20]);
    assert!(MetaInfo::parse(&wrap(&info, b"")).is_ok());

    let first: &[&str] = &["sub", "x"];
    let second: &[&str] = &["sub", "x"];
    let info = multi_info_with("dir", &[(first, 1), (second, 1)], 16384, &[0u8; 20]);
    assert_eq!(
        MetaInfo::parse(&wrap(&info, b"")),
        Err(Error::DuplicatePath {
            path: "dir/sub/x".to_owned(),
        })
    );
}
