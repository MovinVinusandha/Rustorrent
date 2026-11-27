#![no_main]

//! Fuzzes HTTP tracker response decoding with a secret-bearing announce URL.
//!
//! The URL carries a passkey-shaped query value. Every body built here also
//! contains that value, so a leaked diagnostic is caught by the assertions.
//! `Debug`, `Display` and `sanitized_text` are all checked.

use libfuzzer_sys::fuzz_target;

use bt_engine::tracker::{
    MAX_RESPONSE_BYTES, TrackerError, TrackerMessage, TrackerUrl, decode_response,
};

const SECRET: &str = "SECRET-PASSKEY-1234";
const URL: &str = "https://tracker.example/announce?passkey=SECRET-PASSKEY-1234";

fn push_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend(bytes.len().to_string().as_bytes());
    output.push(b':');
    output.extend(bytes);
}

/// Builds a canonical bencode dictionary from already sorted entries.
fn body(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut output = vec![b'd'];
    for (key, value) in entries {
        push_bytes(&mut output, key);
        push_bytes(&mut output, value);
    }
    output.push(b'e');
    output
}

fn assert_no_secret(text: &str) {
    assert!(
        !text.contains(SECRET),
        "diagnostic leaked the announce secret"
    );
}

fn check_message(message: &TrackerMessage, url: &TrackerUrl) {
    assert_no_secret(&message.sanitized_text(url));
    assert_no_secret(&format!("{message:?}"));
    assert_no_secret(&message.to_string());
}

fuzz_target!(|data: &[u8]| {
    let url = TrackerUrl::parse(URL).expect("fixed announce URL parses");
    assert_no_secret(&url.to_string());
    assert_no_secret(&format!("{url:?}"));

    // Arbitrary bodies must never panic, whatever they decode to.
    match decode_response(data) {
        Ok(response) => {
            assert_no_secret(&format!("{response:?}"));
            if let Some(warning) = &response.warning {
                check_message(warning, &url);
            }
        }
        Err(error) => {
            assert_no_secret(&format!("{error:?}"));
            assert_no_secret(&error.to_string());
            if let TrackerError::Rejected { message } = error {
                check_message(&message, &url);
            }
        }
    }

    let mut payload = data.to_vec();
    payload.extend(SECRET.as_bytes());

    let rejected = body(&[(b"failure reason", &payload)]);
    if rejected.len() > MAX_RESPONSE_BYTES {
        assert_eq!(decode_response(&rejected), Err(TrackerError::TooLarge));
    } else {
        match decode_response(&rejected) {
            Err(TrackerError::Rejected { message }) => check_message(&message, &url),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    let warned = body(&[
        (b"tracker id", payload.as_slice()),
        (b"warning message", payload.as_slice()),
    ]);
    if warned.len() > MAX_RESPONSE_BYTES {
        assert_eq!(decode_response(&warned), Err(TrackerError::TooLarge));
    } else {
        match decode_response(&warned) {
            Ok(response) => {
                assert_no_secret(&format!("{response:?}"));
                let warning = response.warning.expect("warning message is present");
                check_message(&warning, &url);
            }
            Err(error) => panic!("expected a response, got {error:?}"),
        }
    }
});
