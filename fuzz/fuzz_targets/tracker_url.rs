#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let Ok(url) = bt_engine::tracker::TrackerUrl::parse(text.as_ref()) else {
        return;
    };

    let target = url.request_target();
    assert!(target.starts_with('/'));
    assert!(target.is_ascii());
    assert!(!target.contains('#'));
    assert!(target.len() <= 2048);
    assert!(url.path().is_ascii());
    if let Some(query) = url.query() {
        assert!(query.is_ascii());
    }
    if let Some(fragment) = url.fragment() {
        assert!(fragment.is_ascii());
    }

    let display = url.to_string();
    let debug = format!("{url:?}");
    assert!(display.len() <= bt_engine::tracker::MAX_REDACTED_URL_LEN);
    assert!(debug.len() <= bt_engine::tracker::MAX_REDACTED_URL_LEN + "TrackerUrl()".len());
    for text in [&display, &debug] {
        assert!(!text.contains('?'));
        assert!(!text.contains('#'));
        assert!(text.chars().all(|character| !character.is_control()));
    }
});
