#![no_main]

use libfuzzer_sys::fuzz_target;

use bt_engine::resume::{MAX_RESUME_BYTES, decode_document, encode_document};

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
    }
});
