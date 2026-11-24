#![no_main]

use libfuzzer_sys::fuzz_target;

use bt_engine::metainfo::{FileInfo, InfoHash, MetaInfo, Mode};
use bt_engine::resume::{
    MAX_RESUME_BYTES, ResumeDocument, decode_document, encode_document, layout_fingerprint,
    plan_restore,
};
use bt_engine::storage::ObservedFile;
use bt_engine::storage::layout::FileMap;

/// Keep synthesized maps small enough that every input stays fast.
const MAX_PLAN_FILES: usize = 32;
const MAX_PLAN_PIECES: u32 = 8192;
const MAX_PLAN_BYTES: u64 = 8 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    let decoded = decode_document(data);
    if data.len() > MAX_RESUME_BYTES {
        assert!(decoded.is_err(), "oversized input must be rejected");
        return;
    }

    if let Ok(document) = decoded {
        let encoded = encode_document(&document).expect("decoded document must encode");
        assert!(
            encoded.len() <= MAX_RESUME_BYTES,
            "encoded document exceeded the size limit"
        );
        let again = decode_document(&encoded).expect("encoded document must decode");
        assert_eq!(again, document);
        exercise_plan(&document, data);
    }
});

/// Builds a matching torrent and observed-file list, then plans a restore.
///
/// Values are capped before any allocation, so a hostile document cannot make
/// the fuzzer allocate a huge map or bitfield. The layout hash is recomputed
/// after building the map, because the decoded one is attacker-controlled.
fn exercise_plan(document: &ResumeDocument, selector: &[u8]) {
    if document.piece_length == 0
        || document.total_length > MAX_PLAN_BYTES
        || document.piece_count > MAX_PLAN_PIECES
        || document.files.is_empty()
        || document.files.len() > MAX_PLAN_FILES
    {
        return;
    }

    let mut total = 0u64;
    for file in &document.files {
        let Some(next) = total.checked_add(file.length) else {
            return;
        };
        total = next;
    }
    if total != document.total_length
        || total.div_ceil(document.piece_length) != u64::from(document.piece_count)
    {
        return;
    }

    let Ok(piece_count) = usize::try_from(document.piece_count) else {
        return;
    };
    let files = document
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| FileInfo {
            path: vec!["fuzz".to_owned(), format!("f{index}")],
            length: file.length,
        })
        .collect();

    let meta = MetaInfo {
        info_hash: InfoHash::from_bytes(document.info_hash),
        name: "fuzz".to_owned(),
        mode: Mode::Multi,
        files,
        total_length: document.total_length,
        piece_length: document.piece_length,
        piece_hashes: vec![[0u8; 20]; piece_count],
        announce: None,
        announce_list: Vec::new(),
        private: false,
    };
    let Ok(map) = FileMap::build(&meta) else {
        return;
    };

    let observed: Vec<ObservedFile> = document
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let pick = selector
                .get(index % selector.len().max(1))
                .copied()
                .unwrap_or(0);
            match pick % 3 {
                0 => ObservedFile::Present {
                    size: file.observed_size,
                    mtime_ns: Some(file.mtime_ns),
                },
                1 => ObservedFile::Present {
                    size: file.observed_size.saturating_add(1),
                    mtime_ns: Some(file.mtime_ns),
                },
                _ => ObservedFile::Missing,
            }
        })
        .collect();

    let mut adjusted = document.clone();
    adjusted.layout_hash = layout_fingerprint(&map);

    let _ = plan_restore(&map, &meta.info_hash, Some(&adjusted), &observed);
    let _ = plan_restore(&map, &meta.info_hash, None, &observed);
}
