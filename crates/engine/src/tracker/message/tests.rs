use crate::tracker::{MAX_URL_LEN, TrackerUrl};

use super::{MAX_MESSAGE_LEN, TrackerMessage};

fn url(input: &str) -> TrackerUrl {
    TrackerUrl::parse(input).expect("valid tracker URL")
}

fn root() -> TrackerUrl {
    url("https://tracker.example/")
}

fn message(bytes: &[u8]) -> TrackerMessage {
    TrackerMessage::from_bytes(bytes)
}

#[test]
fn messages_are_capped_at_256_bytes_on_a_char_boundary() {
    let ascii = message(&[b'a'; 300]);
    assert_eq!(ascii.len(), MAX_MESSAGE_LEN);
    assert_eq!(ascii.sanitized_text(&root()), "a".repeat(MAX_MESSAGE_LEN));

    let mut bytes = vec![b'a'; 255];
    bytes.extend("é".as_bytes());
    assert_eq!(message(&bytes).len(), 255);

    let mut bytes = vec![b'a'; 254];
    bytes.extend("é".as_bytes());
    let message = message(&bytes);
    assert_eq!(message.len(), 256);
    assert!(message.sanitized_text(&root()).ends_with('é'));
}

#[test]
fn control_characters_are_stripped() {
    let text = "a\nb\rc\0d\u{7f}e\u{85}f";
    assert_eq!(message(text.as_bytes()).sanitized_text(&root()), "abcdef");
}

#[test]
fn invalid_utf8_is_replaced_lossily() {
    let text = message(&[0xff, b'a']).sanitized_text(&root());
    assert_eq!(text, "\u{fffd}a");
}

#[test]
fn diagnostics_show_only_the_length() {
    let message = message(b"SECRET passkey");

    assert_eq!(message.len(), 14);
    assert!(!message.is_empty());
    assert_eq!(format!("{message:?}"), "TrackerMessage(14 bytes)");
    assert_eq!(message.to_string(), "TrackerMessage(14 bytes)");
    assert!(!format!("{message:?}").contains("SECRET"));
    assert!(!message.to_string().contains("passkey"));
}

#[test]
fn query_names_and_values_are_redacted() {
    let url = url("https://tracker.example/announce?passkey=SECRET");
    let message = message(b"bad passkey SECRET");

    assert_eq!(message.sanitized_text(&url), "bad <redacted> <redacted>");
}

#[test]
fn path_segments_and_fragments_are_redacted() {
    let url = url("https://tracker.example/announce/SECRET#FRAGMENT");
    let message = message(b"SECRET FRAGMENT");

    assert_eq!(message.sanitized_text(&url), "<redacted> <redacted>");
}

#[test]
fn tokens_are_decoded_before_matching() {
    let url = url("https://tracker.example/announce?passkey=SEC%52ET");
    let message = message(b"SECRET");

    assert_eq!(message.sanitized_text(&url), "<redacted>");
}

#[test]
fn percent_encoded_slashes_stay_in_one_token() {
    let url = url("https://tracker.example/a%2Fb%2Fc");
    let message = message(b"a/b/c");

    assert_eq!(message.sanitized_text(&url), "<redacted>");
}

#[test]
fn matching_is_ascii_case_insensitive() {
    let url = url("https://tracker.example/announce?passkey=SeCrEt");
    let message = message(b"secret SECRET");

    assert_eq!(message.sanitized_text(&url), "<redacted> <redacted>");
}

#[test]
fn three_byte_tokens_are_not_redacted() {
    let url = url("https://tracker.example/abc/abcd");
    let message = message(b"abc abcd");

    assert_eq!(message.sanitized_text(&url), "abc <redacted>");
}

#[test]
fn the_threshold_is_measured_after_decoding() {
    let short = url("https://tracker.example/%41%42%43");
    assert_eq!(message(b"ABC").sanitized_text(&short), "ABC");

    let long = url("https://tracker.example/%41%42%43%44");
    assert_eq!(message(b"ABCD").sanitized_text(&long), "<redacted>");
}

#[test]
fn the_root_path_has_no_tokens() {
    assert_eq!(
        message(b"anything at all").sanitized_text(&root()),
        "anything at all"
    );

    let empty = url("https://tracker.example/?#");
    assert_eq!(
        message(b"anything at all").sanitized_text(&empty),
        "anything at all"
    );
}

#[test]
fn a_token_longer_than_the_message_is_ignored() {
    let url = url("https://tracker.example/abcdefgh");
    assert_eq!(message(b"abc").sanitized_text(&url), "abc");
}

#[test]
fn a_query_name_without_a_value_is_a_token() {
    let url = url("https://tracker.example/announce?passkey");
    assert_eq!(message(b"passkey").sanitized_text(&url), "<redacted>");
}

#[test]
fn plus_is_not_decoded_to_a_space() {
    let url = url("https://tracker.example/announce?key=aa+bb");
    let message = message(b"aa+bb aa bb");

    assert_eq!(message.sanitized_text(&url), "<redacted> aa bb");
}

#[test]
fn unicode_tokens_are_redacted_without_splitting_utf8() {
    let url = url("https://tracker.example/announce?name=%F0%9F%98%80");
    let message = message("a😀b".as_bytes());

    assert_eq!(message.sanitized_text(&url), "a<redacted>b");
}

#[test]
fn placeholders_are_not_rescanned() {
    let url = url("https://tracker.example/abcd/redacted");
    let message = message(b"abcd redacted");

    assert_eq!(message.sanitized_text(&url), "<redacted> <redacted>");
}

#[test]
fn overlapping_matches_are_replaced_once() {
    let url = url("https://tracker.example/abcd/bcde");
    let message = message(b"abcde");

    assert_eq!(message.sanitized_text(&url), "<redacted>e");
}

#[test]
fn the_longest_url_and_message_stay_bounded() {
    let prefix = "https://tracker.example/SECRET/";
    let mut text = String::from(prefix);
    while text.len() + 5 <= MAX_URL_LEN {
        text.push_str("aaaa/");
    }
    while text.len() < MAX_URL_LEN {
        text.push('a');
    }
    assert_eq!(text.len(), MAX_URL_LEN);

    let mut bytes = b"SECRET".to_vec();
    bytes.extend(vec![b'x'; MAX_MESSAGE_LEN - bytes.len()]);
    let message = message(&bytes);
    assert_eq!(message.len(), MAX_MESSAGE_LEN);

    let sanitized = message.sanitized_text(&url(&text));
    assert!(!sanitized.contains("SECRET"));
    assert!(sanitized.len() <= 640);
}
