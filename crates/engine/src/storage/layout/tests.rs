use super::*;
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
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.as_bytes()[3].is_ascii_digit()
            && upper.as_bytes()[3] != b'0')
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
        "CON", "con", "PRN", "aux", "NUL", "COM1", "com9", "LPT1", "lpt9",
    ] {
        let sanitized = sanitize_component(name);
        assert_eq!(sanitized, format!("_{name}"));
        assert_windows_safe(&sanitized);
    }
    assert_eq!(sanitize_component("CON.txt"), "_CON.txt");
    assert_eq!(sanitize_component("con.tar.gz"), "_con.tar.gz");
    assert_eq!(sanitize_component("COM0"), "COM0");
    assert_eq!(sanitize_component("COM10"), "COM10");
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
