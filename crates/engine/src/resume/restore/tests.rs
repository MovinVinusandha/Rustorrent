use super::*;
use crate::bitfield::Bitfield;
use crate::metainfo::InfoHash;
use crate::resume::{ResumeDocument, ResumeFile, layout_fingerprint};
use crate::storage::ObservedFile;
use crate::storage::layout::{FileMap, MappedFile};

const INFO: [u8; 20] = [0x33; 20];

fn map_with(files: &[(&[&str], u64)], piece_length: u64) -> FileMap {
    let mut offset = 0u64;
    let mut mapped = Vec::with_capacity(files.len());

    for (components, length) in files {
        let disk_components: Vec<String> = components
            .iter()
            .map(|component| (*component).to_owned())
            .collect();
        let first_piece = u32::try_from(offset / piece_length).expect("first piece");
        let pieces = if *length == 0 {
            first_piece..first_piece
        } else {
            let last_piece =
                u32::try_from((offset + *length - 1) / piece_length).expect("last piece");
            first_piece..last_piece + 1
        };
        mapped.push(MappedFile {
            original_path: disk_components.clone(),
            disk_components,
            length: *length,
            offset,
            pieces,
        });
        offset += *length;
    }

    let piece_count = u32::try_from(offset.div_ceil(piece_length)).expect("piece count");
    FileMap::from_mapped_files(mapped, offset, piece_length, piece_count)
}

fn present(size: u64, mtime_ns: i64) -> ObservedFile {
    ObservedFile::Present {
        size,
        mtime_ns: Some(mtime_ns),
    }
}

fn document_for(map: &FileMap, bits: Bitfield, files: Vec<ResumeFile>) -> ResumeDocument {
    ResumeDocument::new(
        INFO,
        map.piece_length(),
        map.total_length(),
        layout_fingerprint(map),
        bits,
        files,
        0,
    )
}

fn info_hash() -> InfoHash {
    InfoHash::from_bytes(INFO)
}

#[test]
fn validate_accepts_a_matching_document() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let document = document_for(
        &map,
        Bitfield::new(2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    assert_eq!(validate_document(&document, &info_hash(), &map), Ok(()));
}

#[test]
fn validate_rejects_a_different_info_hash() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let document = document_for(
        &map,
        Bitfield::new(2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    assert_eq!(
        validate_document(&document, &InfoHash::from_bytes([0x44; 20]), &map),
        Err(ValidationError::InfoHash)
    );
}

#[test]
fn validate_rejects_piece_length_total_length_and_piece_count() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let mut document = document_for(
        &map,
        Bitfield::new(2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    document.piece_length = 11;
    assert_eq!(
        validate_document(&document, &info_hash(), &map),
        Err(ValidationError::PieceLength {
            expected: 10,
            found: 11,
        })
    );

    document.piece_length = 10;
    document.total_length = 21;
    assert_eq!(
        validate_document(&document, &info_hash(), &map),
        Err(ValidationError::TotalLength {
            expected: 20,
            found: 21,
        })
    );

    document.total_length = 20;
    document.piece_count = 3;
    assert_eq!(
        validate_document(&document, &info_hash(), &map),
        Err(ValidationError::PieceCount {
            expected: 2,
            found: 3,
        })
    );
}

#[test]
fn validate_rejects_a_bitfield_with_the_wrong_piece_count() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let mut document = document_for(
        &map,
        Bitfield::new(1).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );
    document.piece_count = 2;

    assert_eq!(
        validate_document(&document, &info_hash(), &map),
        Err(ValidationError::BitfieldCount {
            expected: 2,
            found: 1,
        })
    );
}

#[test]
fn validate_rejects_file_count_and_file_length() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let mut document = document_for(
        &map,
        Bitfield::new(2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    document.files.clear();
    assert_eq!(
        validate_document(&document, &info_hash(), &map),
        Err(ValidationError::FileCount {
            expected: 1,
            found: 0,
        })
    );

    document.files.push(ResumeFile::new(21, 20, 100));
    assert_eq!(
        validate_document(&document, &info_hash(), &map),
        Err(ValidationError::FileLength {
            file_index: 0,
            expected: 20,
            found: 21,
        })
    );
}

#[test]
fn validate_rejects_a_different_layout_fingerprint() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let other = map_with(&[(&["b.bin"], 20)], 10);
    let document = document_for(
        &other,
        Bitfield::new(2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    assert_eq!(
        validate_document(&document, &info_hash(), &map),
        Err(ValidationError::Layout)
    );
}

#[test]
fn plan_spot_checks_every_piece_of_a_tiny_torrent() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let observed = vec![present(20, 100)];
    let document = document_for(
        &map,
        Bitfield::from_bytes(&[0b11], 2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    let plan = plan_restore(&map, &info_hash(), Some(&document), &observed).expect("plan");

    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0, 1]);
}

#[test]
fn plan_clears_and_rechecks_a_changed_file() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let observed = vec![present(20, 200)];
    let document = document_for(
        &map,
        Bitfield::from_bytes(&[0b11], 2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    let plan = plan_restore(&map, &info_hash(), Some(&document), &observed).expect("plan");

    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0, 1]);
}

#[test]
fn plan_clears_a_missing_file_without_rechecking_it() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let observed = vec![ObservedFile::Missing];
    let document = document_for(
        &map,
        Bitfield::from_bytes(&[0b11], 2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    let plan = plan_restore(&map, &info_hash(), Some(&document), &observed).expect("plan");

    assert_eq!(plan.bits().count_verified(), 0);
    assert!(plan.recheck().is_empty());
}

#[test]
fn plan_rechecks_only_the_readable_piece_of_a_short_stale_file() {
    let map = map_with(&[(&["a.bin"], 20)], 10);
    let observed = vec![present(15, 100)];
    let document = document_for(
        &map,
        Bitfield::from_bytes(&[0b11], 2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );

    let plan = plan_restore(&map, &info_hash(), Some(&document), &observed).expect("plan");

    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0]);
}

#[test]
fn plan_treats_unavailable_mtimes_as_stale() {
    let map = map_with(&[(&["a.bin"], 20)], 10);

    let saved_unknown = document_for(
        &map,
        Bitfield::from_bytes(&[0b11], 2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, i64::MIN)],
    );
    let plan = plan_restore(
        &map,
        &info_hash(),
        Some(&saved_unknown),
        &[present(20, 100)],
    )
    .expect("plan");
    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0, 1]);

    let document = document_for(
        &map,
        Bitfield::from_bytes(&[0b11], 2).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );
    let observed = vec![ObservedFile::Present {
        size: 20,
        mtime_ns: None,
    }];
    let plan = plan_restore(&map, &info_hash(), Some(&document), &observed).expect("plan");
    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0, 1]);
}

#[test]
fn plan_spot_checks_five_spread_pieces() {
    let map = map_with(&[(&["a.bin"], 100)], 10);
    let observed = vec![present(100, 100)];
    let document = document_for(
        &map,
        Bitfield::from_bytes(&[0xff, 0b0000_0011], 10).expect("bitfield"),
        vec![ResumeFile::new(100, 100, 100)],
    );

    let plan = plan_restore(&map, &info_hash(), Some(&document), &observed).expect("plan");

    assert_eq!(plan.recheck(), &[0, 2, 4, 6, 9]);
    for piece in [0, 2, 4, 6, 9] {
        assert!(!plan.bits().get(piece).expect("get"), "piece {piece}");
    }
    for piece in [1, 3, 5, 7, 8] {
        assert!(plan.bits().get(piece).expect("get"), "piece {piece}");
    }
}

#[test]
fn plan_rechecks_every_readable_piece_without_a_document() {
    let map = map_with(&[(&["a.bin"], 20)], 10);

    let plan = plan_restore(&map, &info_hash(), None, &[present(20, 100)]).expect("plan");
    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0, 1]);

    let plan = plan_restore(&map, &info_hash(), None, &[present(15, 100)]).expect("plan");
    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0]);
}

#[test]
fn plan_skips_pieces_shared_with_a_missing_file() {
    let map = map_with(&[(&["a.bin"], 15), (&["b.bin"], 15)], 10);
    let observed = vec![present(15, 100), ObservedFile::Missing];

    let plan = plan_restore(&map, &info_hash(), None, &observed).expect("plan");

    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0]);
}

#[test]
fn plan_dedups_a_piece_shared_by_two_stale_files() {
    let map = map_with(&[(&["a.bin"], 15), (&["b.bin"], 15)], 10);
    let observed = vec![present(15, 200), present(15, 200)];
    let document = document_for(
        &map,
        Bitfield::from_bytes(&[0b111], 3).expect("bitfield"),
        vec![ResumeFile::new(15, 15, 100), ResumeFile::new(15, 15, 100)],
    );

    let plan = plan_restore(&map, &info_hash(), Some(&document), &observed).expect("plan");

    assert_eq!(plan.bits().count_verified(), 0);
    assert_eq!(plan.recheck(), &[0, 1, 2]);
}

#[test]
fn spot_check_returns_every_verified_piece_below_the_limit() {
    let bits = Bitfield::from_bytes(&[0b0000_0101], 3).expect("bitfield");

    assert_eq!(spot_check_pieces(&bits, 5), vec![0, 2]);
    assert!(spot_check_pieces(&bits, 0).is_empty());
}

#[test]
fn spot_check_skips_unverified_pieces_and_keeps_the_spread() {
    let bits = Bitfield::from_bytes(&[0b1000_0101], 8).expect("bitfield");

    assert_eq!(spot_check_pieces(&bits, 2), vec![0, 7]);
}

#[test]
fn plan_rejects_mismatched_inputs() {
    let map = map_with(&[(&["a.bin"], 20)], 10);

    assert_eq!(
        plan_restore(&map, &info_hash(), None, &[]),
        Err(RestoreError::ObservedFileCount {
            expected: 1,
            found: 0,
        })
    );

    let no_files = document_for(&map, Bitfield::new(2).expect("bitfield"), Vec::new());
    assert_eq!(
        plan_restore(&map, &info_hash(), Some(&no_files), &[present(20, 100)]),
        Err(RestoreError::Invalid(ValidationError::FileCount {
            expected: 1,
            found: 0,
        }))
    );

    let wrong_length = document_for(
        &map,
        Bitfield::new(2).expect("bitfield"),
        vec![ResumeFile::new(21, 20, 100)],
    );
    assert_eq!(
        plan_restore(&map, &info_hash(), Some(&wrong_length), &[present(20, 100)]),
        Err(RestoreError::Invalid(ValidationError::FileLength {
            file_index: 0,
            expected: 20,
            found: 21,
        }))
    );

    let wrong_bits = document_for(
        &map,
        Bitfield::new(1).expect("bitfield"),
        vec![ResumeFile::new(20, 20, 100)],
    );
    assert_eq!(
        plan_restore(&map, &info_hash(), Some(&wrong_bits), &[present(20, 100)]),
        Err(RestoreError::Invalid(ValidationError::PieceCount {
            expected: 2,
            found: 1,
        }))
    );
}
