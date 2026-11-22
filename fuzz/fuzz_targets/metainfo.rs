#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(info) = bt_engine::metainfo::MetaInfo::parse(data) {
        assert!(!info.files.is_empty());
        assert_eq!(
            info.piece_hashes.len() as u64,
            info.total_length.div_ceil(info.piece_length)
        );
    }
});
