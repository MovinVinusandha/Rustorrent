use super::*;
use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::metainfo::{FileInfo, InfoHash, MetaInfo, Mode};
use proptest::prelude::*;

const ADVERSARIAL_CHARS: &[char] = &[
    '.', ' ', ':', '\\', '\0', '/', '*', '?', '"', '<', '>', '|', '_', '~', 'a', 'Z', '0', '9',
    'é', '\u{301}', '😀', '\n', '\t', '\u{7f}',
];

fn adversarial_component() -> impl Strategy<Value = String> {
    let chars = prop::collection::vec(prop::sample::select(ADVERSARIAL_CHARS.to_vec()), 0..=300)
        .prop_map(|chars| chars.into_iter().collect::<String>());
    let tokens = prop_oneof![
        Just("CON".to_owned()),
        Just("con.txt".to_owned()),
        Just("COM1".to_owned()),
        Just("COM¹".to_owned()),
        Just("lpt³".to_owned()),
        Just("CONIN$".to_owned()),
        Just("CLOCK$".to_owned()),
        Just("lpt9.tar.gz".to_owned()),
        Just("..".to_owned()),
        Just(".".to_owned()),
        Just("a~1.txt".to_owned()),
    ];
    prop_oneof![chars, tokens]
}

fn is_windows_reserved(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let stem = stem.trim_end_matches([' ', '.']);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || is_windows_device_name(&upper)
}

fn is_windows_device_name(upper: &str) -> bool {
    let Some(rest) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };

    let mut characters = rest.chars();
    matches!(
        (characters.next(), characters.next()),
        (Some('1'..='9' | '\u{00b9}' | '\u{00b2}' | '\u{00b3}'), None)
    )
}

fn assert_windows_safe(component: &str) {
    assert!(!component.is_empty());
    assert!(component.len() <= MAX_COMPONENT_BYTES);
    assert_ne!(component, ".");
    assert_ne!(component, "..");
    assert!(!component.ends_with('.'));
    assert!(!component.ends_with(' '));
    assert!(!is_windows_reserved(component));
    for character in component.chars() {
        assert!(
            !character.is_ascii_control(),
            "control char in {component:?}"
        );
        assert!(
            !matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            ),
            "invalid char {character:?} in {component:?}"
        );
    }
}

#[test]
fn sanitizes_traversal_and_drive_components() {
    assert_eq!(sanitize_component("..\\..\\x"), ".._.._x");
    assert_eq!(sanitize_component("C:\\x"), "C__x");
    assert_eq!(sanitize_component("C:x"), "C_x");
    assert_eq!(sanitize_component(".."), "_");
    assert_eq!(sanitize_component("."), "_");
    assert_eq!(sanitize_component(""), "_");
}

#[test]
fn replaces_invalid_and_control_characters() {
    assert_eq!(sanitize_component("a/b"), "a_b");
    assert_eq!(sanitize_component("a\\b"), "a_b");
    assert_eq!(sanitize_component("a:b"), "a_b");
    assert_eq!(sanitize_component("a*b?c\"d<e>f|g"), "a_b_c_d_e_f_g");
    assert_eq!(sanitize_component("a\0b\nc\td\u{7f}"), "a_b_c_d_");
}

#[test]
fn prefixes_windows_reserved_names() {
    for name in [
        "CON", "con", "PRN", "aux", "NUL", "COM1", "com9", "LPT1", "lpt9", "CONIN$", "conin$",
        "CONOUT$", "CLOCK$", "clock$",
    ] {
        let sanitized = sanitize_component(name);
        assert_eq!(sanitized, format!("_{name}"));
        assert_windows_safe(&sanitized);
    }
    for name in ["COM¹", "COM²", "COM³", "lpt¹", "LPT³"] {
        let sanitized = sanitize_component(name);
        assert_eq!(sanitized, format!("_{name}"));
        assert_windows_safe(&sanitized);
    }
    assert_eq!(sanitize_component("CON.txt"), "_CON.txt");
    assert_eq!(sanitize_component("con.tar.gz"), "_con.tar.gz");
    assert_eq!(sanitize_component("CONIN$.txt"), "_CONIN$.txt");
    assert_eq!(sanitize_component("COM0"), "COM0");
    assert_eq!(sanitize_component("COM10"), "COM10");
    assert_eq!(sanitize_component("COM⁴"), "COM⁴");
    assert_eq!(sanitize_component("COM⁰"), "COM⁰");
}

#[test]
fn replaces_trailing_dots_and_spaces() {
    assert_eq!(sanitize_component("foo."), "foo_");
    assert_eq!(sanitize_component("foo "), "foo_");
    assert_eq!(sanitize_component("foo..  "), "foo_");
    assert_eq!(sanitize_component("..."), "_");
    assert_windows_safe(&sanitize_component("foo..  "));
}

#[test]
fn preserves_unicode_and_emoji() {
    assert_eq!(sanitize_component("é"), "é");
    assert_eq!(sanitize_component("e\u{301}"), "e\u{301}");
    assert_eq!(sanitize_component("😀"), "😀");
}

#[test]
fn truncates_long_components_at_a_utf8_boundary() {
    let input = "é".repeat(200);
    let sanitized = sanitize_component(&input);
    assert!(sanitized.len() <= MAX_COMPONENT_BYTES);
    assert!(sanitized.len() > MAX_COMPONENT_BYTES - 4);
    assert_windows_safe(&sanitized);
    assert_eq!(sanitize_component(&sanitized), sanitized);
}

#[test]
fn sanitization_is_idempotent() {
    let long = "a".repeat(300);
    for input in [
        "..\\..\\x",
        "C:\\x",
        "C:x",
        "CON.txt",
        "CONIN$",
        "CLOCK$",
        "COM¹",
        "lpt³",
        "foo..  ",
        "é",
        "e\u{301}",
        "😀",
        "",
        ".",
        "..",
        long.as_str(),
    ] {
        let once = sanitize_component(input);
        assert_eq!(sanitize_component(&once), once, "input {input:?}");
    }
}

#[test]
fn collision_keys_fold_case_and_post_sanitization_duplicates() {
    let upper = vec![sanitize_component("A.txt")];
    let lower = vec![sanitize_component("a.txt")];
    assert_eq!(collision_key(&upper), collision_key(&lower));

    let colon = vec![sanitize_component("a:b")];
    let question = vec![sanitize_component("a?b")];
    assert_eq!(colon, question);
    assert_eq!(collision_key(&colon), collision_key(&question));

    let first = vec![sanitize_component("Dir"), sanitize_component("A.txt")];
    let second = vec![sanitize_component("dir"), sanitize_component("a.txt")];
    assert_eq!(collision_key(&first), collision_key(&second));
}

#[test]
fn collision_keys_do_not_normalize_nfc_or_nfd() {
    assert_ne!(
        collision_key(&[sanitize_component("é")]),
        collision_key(&[sanitize_component("e\u{301}")])
    );
}

#[test]
fn collision_suffix_goes_before_the_extension() {
    assert_eq!(suffix_component("a.txt", 1), "a~1.txt");
    assert_eq!(suffix_component("a.tar.gz", 1), "a.tar~1.gz");
    assert_eq!(suffix_component(".gitignore", 1), ".gitignore~1");
    assert_eq!(suffix_component("a", 1), "a~1");
    assert_eq!(suffix_component("a.verylongext", 1), "a.verylongext~1");
    assert_eq!(suffix_component("a~1.txt", 1), "a~1~1.txt");
    assert_eq!(suffix_component("a.", 1), "a.~1");
    assert_eq!(suffix_component("a ", 1), "a ~1");
}

#[test]
fn collision_suffix_stays_within_the_component_cap() {
    let input = format!("{}.txt", "a".repeat(251));
    let sanitized = sanitize_component(&input);
    let suffixed = suffix_component(&sanitized, 12);
    assert!(suffixed.len() <= MAX_COMPONENT_BYTES);
    assert!(suffixed.ends_with("~12.txt"));
    assert_windows_safe(&suffixed);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sanitized_components_are_windows_safe(input in adversarial_component()) {
        let output = sanitize_component(&input);
        assert_windows_safe(&output);
        prop_assert_eq!(sanitize_component(&output), output);
    }

    #[test]
    fn collision_suffixes_are_windows_safe(input in adversarial_component(), attempt in 1u32..=20) {
        let sanitized = sanitize_component(&input);
        let output = suffix_component(&sanitized, attempt);
        assert_windows_safe(&output);
        prop_assert!(output.len() <= MAX_COMPONENT_BYTES);
    }
}

fn meta_owned(files: Vec<(Vec<String>, u64)>, piece_length: u64) -> MetaInfo {
    let total_length = files
        .iter()
        .fold(0u64, |acc, (_, length)| acc.saturating_add(*length));
    let piece_count = if piece_length == 0 {
        0
    } else {
        total_length.div_ceil(piece_length)
    };

    MetaInfo {
        info_hash: InfoHash::from_bytes([0; 20]),
        name: "root".to_owned(),
        mode: Mode::Multi,
        files: files
            .into_iter()
            .map(|(path, length)| FileInfo { path, length })
            .collect(),
        total_length,
        piece_length,
        piece_hashes: vec![[0u8; 20]; piece_count as usize],
        announce: None,
        announce_list: Vec::new(),
        private: false,
    }
}

fn meta(files: &[(&[&str], u64)], piece_length: u64) -> MetaInfo {
    meta_owned(
        files
            .iter()
            .map(|(path, length)| {
                (
                    path.iter().map(|component| component.to_string()).collect(),
                    *length,
                )
            })
            .collect(),
        piece_length,
    )
}

fn build(files: &[(&[&str], u64)], piece_length: u64) -> FileMap {
    FileMap::build(&meta(files, piece_length)).unwrap()
}

#[test]
fn maps_single_file() {
    let map = build(&[(&["file.txt"][..], 5)], 4);

    assert_eq!(map.files().len(), 1);
    let file = &map.files()[0];
    assert_eq!(file.offset, 0);
    assert_eq!(file.pieces, 0..2);
    assert_eq!(file.disk_components, vec!["file.txt"]);
    assert_eq!(file.to_path(), PathBuf::from("file.txt"));
    assert_eq!(map.total_length(), 5);
    assert_eq!(map.piece_length(), 4);
    assert_eq!(map.piece_count(), 2);
    assert_eq!(map.piece_byte_range(0), Some(0..4));
    assert_eq!(map.piece_byte_range(1), Some(4..5));
    assert_eq!(map.piece_byte_range(2), None);
}

#[test]
fn maps_multiple_files_and_spans() {
    let map = build(
        &[
            (&["root", "a"][..], 3),
            (&["root", "b"][..], 4),
            (&["root", "c"][..], 5),
        ],
        4,
    );

    assert_eq!(map.files()[0].offset, 0);
    assert_eq!(map.files()[1].offset, 3);
    assert_eq!(map.files()[2].offset, 7);
    assert_eq!(map.files()[0].pieces, 0..1);
    assert_eq!(map.files()[1].pieces, 0..2);
    assert_eq!(map.files()[2].pieces, 1..3);
    assert_eq!(
        map.segments(2..9).collect::<Vec<_>>(),
        vec![
            Segment {
                file_index: 0,
                offset_in_file: 2,
                len: 1,
            },
            Segment {
                file_index: 1,
                offset_in_file: 0,
                len: 4,
            },
            Segment {
                file_index: 2,
                offset_in_file: 0,
                len: 2,
            },
        ]
    );
    assert_eq!(map.files_for_piece(0).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(map.files_for_piece(1).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(map.files_for_piece(2).collect::<Vec<_>>(), vec![2]);
}

#[test]
fn skips_zero_length_files() {
    let map = build(
        &[
            (&["root", "a"][..], 2),
            (&["root", "empty"][..], 0),
            (&["root", "c"][..], 3),
        ],
        4,
    );

    assert_eq!(map.files()[1].offset, 2);
    assert_eq!(map.files()[1].pieces, 0..0);
    assert_eq!(map.files()[2].offset, 2);
    assert_eq!(
        map.segments(0..5).collect::<Vec<_>>(),
        vec![
            Segment {
                file_index: 0,
                offset_in_file: 0,
                len: 2,
            },
            Segment {
                file_index: 2,
                offset_in_file: 0,
                len: 3,
            },
        ]
    );
    assert_eq!(
        map.segments(2..5).collect::<Vec<_>>(),
        vec![Segment {
            file_index: 2,
            offset_in_file: 0,
            len: 3,
        }]
    );
    assert!(map.segments(2..2).next().is_none());
    assert_eq!(map.files_for_piece(0).collect::<Vec<_>>(), vec![0, 2]);
    assert_eq!(map.files_for_piece(1).collect::<Vec<_>>(), vec![2]);
    assert_eq!(
        map.file_for_offset(2)
            .map(|file| file.disk_components.clone()),
        Some(vec!["root".to_owned(), "c".to_owned()])
    );
    assert!(map.file_for_offset(5).is_none());
}

#[test]
fn respects_exact_file_boundaries() {
    let map = build(&[(&["a"][..], 4), (&["b"][..], 4)], 4);

    assert_eq!(map.files()[0].pieces, 0..1);
    assert_eq!(map.files()[1].pieces, 1..2);
    assert_eq!(
        map.segments(0..4).collect::<Vec<_>>(),
        vec![Segment {
            file_index: 0,
            offset_in_file: 0,
            len: 4,
        }]
    );
    assert_eq!(
        map.segments(4..8).collect::<Vec<_>>(),
        vec![Segment {
            file_index: 1,
            offset_in_file: 0,
            len: 4,
        }]
    );
    assert_eq!(map.files_for_piece(0).collect::<Vec<_>>(), vec![0]);
    assert_eq!(map.files_for_piece(1).collect::<Vec<_>>(), vec![1]);
}

#[test]
fn sanitizes_and_disambiguates_paths() {
    let map = build(
        &[
            (&["A.txt"][..], 0),
            (&["a.txt"][..], 0),
            (&["a:b"][..], 0),
            (&["a?b"][..], 0),
        ],
        1,
    );

    assert_eq!(
        map.files()
            .iter()
            .map(|file| file.disk_components.clone())
            .collect::<Vec<_>>(),
        vec![
            vec!["A.txt".to_owned()],
            vec!["a~1.txt".to_owned()],
            vec!["a_b".to_owned()],
            vec!["a_b~1".to_owned()],
        ]
    );
}

#[test]
fn real_later_suffix_name_is_preserved() {
    let map = build(&[(&["A~1.txt"][..], 0), (&["a~1.txt"][..], 0)], 1);

    assert_eq!(map.files()[0].disk_components, vec!["A~1.txt"]);
    assert_eq!(map.files()[1].disk_components, vec!["a~1~1.txt"]);
}

#[test]
fn disambiguates_file_vs_directory() {
    let file_then_dir = build(&[(&["a"][..], 1), (&["a", "b"][..], 1)], 1);
    assert_eq!(file_then_dir.files()[0].disk_components, vec!["a"]);
    assert_eq!(file_then_dir.files()[1].disk_components, vec!["a~1", "b"]);

    let dir_then_file = build(&[(&["a", "b"][..], 1), (&["a"][..], 1)], 1);
    assert_eq!(dir_then_file.files()[0].disk_components, vec!["a", "b"]);
    assert_eq!(dir_then_file.files()[1].disk_components, vec!["a~1"]);
}

#[test]
fn to_path_joins_components() {
    let map = build(&[(&["root", "a"][..], 0)], 1);
    assert_eq!(map.files()[0].to_path(), PathBuf::from("root").join("a"));
}

#[test]
fn rejects_invalid_metadata() {
    let empty = meta_owned(Vec::new(), 1);
    assert_eq!(FileMap::build(&empty), Err(LayoutError::EmptyFiles));

    let zero = meta_owned(vec![(vec!["x".to_owned()], 0)], 0);
    assert_eq!(FileMap::build(&zero), Err(LayoutError::ZeroPieceLength));

    let mut mismatch = meta_owned(vec![(vec!["x".to_owned()], 1)], 4);
    mismatch.total_length = 2;
    assert_eq!(
        FileMap::build(&mismatch),
        Err(LayoutError::LengthMismatch {
            declared: 2,
            computed: 1,
        })
    );

    let mut pieces = meta_owned(vec![(vec!["x".to_owned()], 4)], 4);
    pieces.piece_hashes.clear();
    assert_eq!(
        FileMap::build(&pieces),
        Err(LayoutError::PieceCountMismatch {
            expected: 1,
            found: 0,
        })
    );

    let empty_path = meta_owned(vec![(Vec::new(), 0)], 1);
    assert_eq!(
        FileMap::build(&empty_path),
        Err(LayoutError::EmptyPath { file_index: 0 })
    );

    let overflow = MetaInfo {
        info_hash: InfoHash::from_bytes([0; 20]),
        name: "x".to_owned(),
        mode: Mode::Multi,
        files: vec![
            FileInfo {
                path: vec!["x".to_owned()],
                length: u64::MAX,
            },
            FileInfo {
                path: vec!["y".to_owned()],
                length: 1,
            },
        ],
        total_length: u64::MAX,
        piece_length: 1,
        piece_hashes: Vec::new(),
        announce: None,
        announce_list: Vec::new(),
        private: false,
    };
    assert_eq!(
        FileMap::build(&overflow),
        Err(LayoutError::TotalLengthOverflow)
    );
}

#[test]
fn rejects_path_too_long() {
    let components = vec!["a".repeat(250); 5];
    let meta = meta_owned(vec![(components, 0)], 1);
    assert!(matches!(
        FileMap::build(&meta),
        Err(LayoutError::PathTooLong {
            max: MAX_PATH_BYTES,
            ..
        })
    ));
}

fn piece_lengths() -> impl Strategy<Value = u64> {
    prop_oneof![Just(1u64), Just(2), Just(4), Just(8)]
}

fn file_sets() -> impl Strategy<Value = Vec<(Vec<String>, u64)>> {
    prop::collection::vec(
        (
            prop::collection::vec(adversarial_component(), 1..=4),
            0u64..=20,
        ),
        1..=4,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn file_map_invariants(files in file_sets(), piece_length in piece_lengths()) {
        let meta = meta_owned(files, piece_length);
        let map = FileMap::build(&meta).unwrap();

        prop_assert_eq!(map.total_length(), meta.total_length);
        prop_assert_eq!(map.piece_count(), meta.piece_hashes.len() as u32);

        let mut offset = 0u64;
        let mut keys = BTreeSet::new();
        for file in map.files() {
            prop_assert_eq!(file.offset, offset);
            offset += file.length;

            for component in &file.disk_components {
                assert_windows_safe(component);
            }
            let key = collision_key(&file.disk_components);
            prop_assert!(keys.insert(key));

            if file.length == 0 {
                prop_assert_eq!(file.pieces.start, file.pieces.end);
            } else {
                prop_assert_eq!(
                    file.pieces.start,
                    (file.offset / piece_length) as u32
                );
                prop_assert_eq!(
                    file.pieces.end,
                    ((file.offset + file.length - 1) / piece_length + 1) as u32
                );
            }
        }
        prop_assert_eq!(offset, meta.total_length);

        let segments: Vec<_> = map.segments(0..map.total_length()).collect();
        let covered: u64 = segments.iter().map(|segment| segment.len).sum();
        prop_assert_eq!(covered, map.total_length());

        let mut expected_offset = 0u64;
        for segment in &segments {
            let file = &map.files()[segment.file_index];
            prop_assert_eq!(file.offset + segment.offset_in_file, expected_offset);
            expected_offset += segment.len;
            prop_assert!(segment.len > 0);
            prop_assert!(segment.offset_in_file + segment.len <= file.length);
        }

        for offset in 0..map.total_length() {
            let file = map.file_for_offset(offset).unwrap();
            prop_assert!(offset >= file.offset && offset < file.offset + file.length);
        }
        prop_assert!(map.file_for_offset(map.total_length()).is_none());

        for piece in 0..map.piece_count() {
            let range = map.piece_byte_range(piece).unwrap();
            let from_segments: Vec<usize> =
                map.segments(range).map(|segment| segment.file_index).collect();
            let from_piece: Vec<usize> = map.files_for_piece(piece).collect();
            prop_assert_eq!(from_piece, from_segments);
        }
    }

    #[test]
    fn segment_ranges_are_clamped(
        files in file_sets(),
        piece_length in piece_lengths(),
        start in 0u64..=100,
        end in 0u64..=100,
    ) {
        let meta = meta_owned(files, piece_length);
        let map = FileMap::build(&meta).unwrap();

        let clamped_end = end.min(map.total_length());
        let clamped_start = start.min(clamped_end);
        let segments: Vec<_> = map.segments(start..end).collect();
        let covered: u64 = segments.iter().map(|segment| segment.len).sum();

        prop_assert_eq!(covered, clamped_end - clamped_start);
        for segment in segments {
            let file = &map.files()[segment.file_index];
            prop_assert!(segment.len > 0);
            prop_assert!(segment.offset_in_file + segment.len <= file.length);
        }
    }

    #[test]
    fn build_is_deterministic(files in file_sets(), piece_length in piece_lengths()) {
        let meta = meta_owned(files, piece_length);
        let first = FileMap::build(&meta).unwrap();
        let second = FileMap::build(&meta).unwrap();
        prop_assert_eq!(first, second);
    }
}
