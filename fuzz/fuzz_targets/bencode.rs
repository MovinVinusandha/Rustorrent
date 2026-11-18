#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = bt_engine::bencode::decode(data) {
        let encoded = bt_engine::bencode::encode(&value).expect("decoded value must encode");
        assert_eq!(encoded.as_slice(), data);
    }
});
