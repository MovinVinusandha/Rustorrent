use super::*;

type ErrorPredicate = fn(&Error) -> bool;

fn lenient_options() -> DecodeOptions {
    DecodeOptions {
        key_order: KeyOrder::Lenient,
        ..DecodeOptions::default()
    }
}

#[test]
fn decodes_spec_examples() {
    assert_eq!(decode(b"4:spam").unwrap().as_bytes(), Some(&b"spam"[..]));
    assert_eq!(decode(b"i3e").unwrap().as_int(), Some(3));
    assert_eq!(decode(b"i-3e").unwrap().as_int(), Some(-3));
    assert_eq!(decode(b"i0e").unwrap().as_int(), Some(0));

    let list = decode(b"l4:spam4:eggse").unwrap();
    let items = list.as_list().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_bytes(), Some(&b"spam"[..]));
    assert_eq!(items[1].as_bytes(), Some(&b"eggs"[..]));

    let dict = decode(b"d3:cow3:moo4:spam4:eggse").unwrap();
    assert_eq!(dict.dict_get(b"cow").unwrap().as_bytes(), Some(&b"moo"[..]));
    assert_eq!(
        dict.dict_get(b"spam").unwrap().as_bytes(),
        Some(&b"eggs"[..])
    );

    let nested = decode(b"d4:spaml1:a1:bee").unwrap();
    let spam = nested.dict_get(b"spam").unwrap().as_list().unwrap();
    assert_eq!(spam.len(), 2);
    assert_eq!(spam[0].as_bytes(), Some(&b"a"[..]));
    assert_eq!(spam[1].as_bytes(), Some(&b"b"[..]));
}

#[test]
fn decodes_empty_values() {
    assert_eq!(decode(b"0:").unwrap().as_bytes(), Some(&b""[..]));
    assert!(decode(b"le").unwrap().as_list().unwrap().is_empty());
    assert!(decode(b"de").unwrap().as_dict().unwrap().is_empty());
}

#[test]
fn bencode_strings_are_arbitrary_bytes() {
    let value = decode(b"2:\xff\xfe").unwrap();
    assert_eq!(value.as_bytes(), Some(&b"\xff\xfe"[..]));
}

#[test]
fn decodes_integer_boundaries() {
    assert_eq!(
        decode(b"i9223372036854775807e").unwrap().as_int(),
        Some(i64::MAX)
    );
    assert_eq!(
        decode(b"i-9223372036854775808e").unwrap().as_int(),
        Some(i64::MIN)
    );
}

#[test]
fn records_exact_spans() {
    let value = decode(b"li1ee").unwrap();
    assert_eq!(value.span(), 0..5);

    let items = value.as_list().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].span(), 1..4);
}

#[test]
fn value_equality_ignores_spans() {
    let standalone = decode(b"i1e").unwrap();
    let nested = decode(b"li1ee").unwrap();
    let nested_int = nested.as_list().unwrap()[0].clone();

    assert_eq!(standalone, nested_int);
    assert_ne!(standalone.span(), nested_int.span());
    assert_ne!(standalone, decode(b"i2e").unwrap());
}

#[test]
fn strict_and_lenient_dict_key_order() {
    let unsorted = b"d1:b1:a1:a1:ae";
    assert!(matches!(
        decode(unsorted),
        Err(Error::UnsortedDictKeys { .. })
    ));

    let lenient = decode_with_options(unsorted, lenient_options()).unwrap();
    assert_eq!(lenient.dict_get(b"a").unwrap().as_bytes(), Some(&b"a"[..]));
    assert_eq!(lenient.dict_get(b"b").unwrap().as_bytes(), Some(&b"a"[..]));

    let sorted = decode(b"d1:a1:a1:b1:be").unwrap();
    assert_eq!(sorted.dict_get(b"a").unwrap().as_bytes(), Some(&b"a"[..]));
    assert_eq!(sorted.dict_get(b"b").unwrap().as_bytes(), Some(&b"b"[..]));
}

#[test]
fn duplicate_dict_keys_are_rejected_in_both_modes() {
    let duplicate = b"d1:a1:a1:a1:ae";

    assert!(matches!(
        decode(duplicate),
        Err(Error::DuplicateDictKey { .. })
    ));
    assert!(matches!(
        decode_with_options(duplicate, lenient_options()),
        Err(Error::DuplicateDictKey { .. })
    ));
}

#[test]
fn malformed_inputs_are_rejected() {
    let cases: &[(&[u8], ErrorPredicate)] = &[
        (b"", |error| matches!(error, Error::Truncated { .. })),
        (b"i", |error| matches!(error, Error::Truncated { .. })),
        (b"i3", |error| matches!(error, Error::Truncated { .. })),
        (b"i-", |error| matches!(error, Error::Truncated { .. })),
        (b"ie", |error| matches!(error, Error::InvalidInteger { .. })),
        (b"i-e", |error| {
            matches!(error, Error::InvalidInteger { .. })
        }),
        (b"i-0e", |error| {
            matches!(error, Error::IntegerNegativeZero { .. })
        }),
        (b"i03e", |error| {
            matches!(error, Error::IntegerLeadingZero { .. })
        }),
        (b"i-03e", |error| {
            matches!(error, Error::IntegerLeadingZero { .. })
        }),
        (b"i+3e", |error| {
            matches!(error, Error::InvalidInteger { .. })
        }),
        (b"i3.0e", |error| {
            matches!(error, Error::InvalidInteger { .. })
        }),
        (b"i9223372036854775808e", |error| {
            matches!(error, Error::IntegerOverflow { .. })
        }),
        (b"i-9223372036854775809e", |error| {
            matches!(error, Error::IntegerOverflow { .. })
        }),
        (b"4:spa", |error| matches!(error, Error::Truncated { .. })),
        (b"5:spam", |error| matches!(error, Error::Truncated { .. })),
        (b"1:", |error| matches!(error, Error::Truncated { .. })),
        (b"01:a", |error| {
            matches!(error, Error::InvalidLength { .. })
        }),
        (b"00:", |error| matches!(error, Error::InvalidLength { .. })),
        (b":a", |error| matches!(error, Error::InvalidToken { .. })),
        (b"-1:a", |error| matches!(error, Error::InvalidToken { .. })),
        (b"99999999999999999999:", |error| {
            matches!(error, Error::InvalidLength { .. })
        }),
        (b"l", |error| matches!(error, Error::Truncated { .. })),
        (b"li1e", |error| matches!(error, Error::Truncated { .. })),
        (b"d", |error| matches!(error, Error::Truncated { .. })),
        (b"d1:a", |error| matches!(error, Error::Truncated { .. })),
        (b"d1:a1", |error| matches!(error, Error::Truncated { .. })),
        (b"d1:b1:a1:a1:ae", |error| {
            matches!(error, Error::UnsortedDictKeys { .. })
        }),
        (b"d1:a1:a1:a1:ae", |error| {
            matches!(error, Error::DuplicateDictKey { .. })
        }),
        (b"di1e1:ae", |error| {
            matches!(error, Error::NonStringDictKey { .. })
        }),
        (b"i1eX", |error| matches!(error, Error::TrailingData { .. })),
        (b"i1ei2e", |error| {
            matches!(error, Error::TrailingData { .. })
        }),
        (b"0:0:", |error| matches!(error, Error::TrailingData { .. })),
        (b"e", |error| {
            matches!(error, Error::UnexpectedEndMarker { .. })
        }),
        (b"x", |error| matches!(error, Error::InvalidToken { .. })),
        (b"l e", |error| matches!(error, Error::InvalidToken { .. })),
    ];

    for &(input, expected) in cases {
        let error = decode(input).expect_err("input should be rejected");
        assert!(
            expected(&error),
            "unexpected error for {:?}: {error:?}",
            input
        );
    }
}

#[test]
fn rejects_overlong_integer_literal() {
    let input = format!("i{}e", "1".repeat(MAX_INT_BYTES + 1));
    assert!(matches!(
        decode(input.as_bytes()),
        Err(Error::IntegerTooLong { .. })
    ));
}

#[test]
fn rejects_overlong_string_length() {
    let input = format!("{}:", MAX_STRING_BYTES + 1);
    assert!(matches!(
        decode(input.as_bytes()),
        Err(Error::StringTooLong { .. })
    ));
}

#[test]
fn enforces_limits() {
    let short_input = Limits {
        max_input_bytes: 2,
        ..Limits::default()
    };
    assert!(matches!(
        decode_with_limits(b"i1e", short_input),
        Err(Error::InputTooLarge { .. })
    ));

    let shallow = Limits {
        max_depth: 2,
        ..Limits::default()
    };
    assert!(decode_with_limits(b"llee", shallow).is_ok());
    assert!(matches!(
        decode_with_limits(b"llleee", shallow),
        Err(Error::DepthLimit { .. })
    ));

    let few_nodes = Limits {
        max_nodes: 0,
        ..Limits::default()
    };
    assert!(matches!(
        decode_with_limits(b"i1e", few_nodes),
        Err(Error::TooManyNodes { .. })
    ));

    let few_items = Limits {
        max_container_items: 1,
        ..Limits::default()
    };
    assert!(decode_with_limits(b"li1ee", few_items).is_ok());
    assert!(matches!(
        decode_with_limits(b"li1ei1ee", few_items),
        Err(Error::TooManyItems { .. })
    ));

    let short_strings = Limits {
        max_string_bytes: 4,
        ..Limits::default()
    };
    assert!(decode_with_limits(b"4:spam", short_strings).is_ok());
    assert!(matches!(
        decode_with_limits(b"5:spam", short_strings),
        Err(Error::StringTooLong { .. })
    ));

    let short_ints = Limits {
        max_int_bytes: 4,
        ..Limits::default()
    };
    assert!(decode_with_limits(b"i1234e", short_ints).is_ok());
    assert!(matches!(
        decode_with_limits(b"i12345e", short_ints),
        Err(Error::IntegerTooLong { .. })
    ));
}

#[test]
fn default_limits_and_options_match_constants() {
    let limits = Limits::default();
    assert_eq!(limits.max_input_bytes, MAX_INPUT_BYTES);
    assert_eq!(limits.max_string_bytes, MAX_STRING_BYTES);
    assert_eq!(limits.max_int_bytes, MAX_INT_BYTES);
    assert_eq!(limits.max_depth, MAX_DEPTH);
    assert_eq!(limits.max_container_items, MAX_CONTAINER_ITEMS);
    assert_eq!(limits.max_nodes, MAX_NODES);
    assert_eq!(DecodeOptions::default().key_order, KeyOrder::Strict);
}

fn raw_value<'a>(kind: Kind<'a>) -> Value<'a> {
    Value { kind, span: 0..0 }
}

#[test]
fn encodes_canonical_input_byte_for_byte() {
    let inputs: &[&[u8]] = &[
        b"0:",
        b"4:spam",
        b"i3e",
        b"i-3e",
        b"i0e",
        b"i9223372036854775807e",
        b"i-9223372036854775808e",
        b"le",
        b"de",
        b"l4:spam4:eggse",
        b"d3:cow3:moo4:spam4:eggse",
        b"d4:spaml1:a1:bee",
        b"2:\xff\xfe",
    ];

    for &input in inputs {
        let value = decode(input).unwrap();
        let encoded = encode(&value).unwrap();
        assert_eq!(encoded.as_slice(), input, "round-trip failed for {input:?}");
    }
}

#[test]
fn constructors_encode_canonical_values() {
    let value = Value::dict(vec![
        (b"z", Value::int(1)),
        (b"a", Value::bytes(b"x")),
        (b"m", Value::list(vec![Value::int(2), Value::int(3)])),
    ])
    .expect("dictionary");
    let encoded = encode(&value).expect("encode");

    assert_eq!(encoded, b"d1:a1:x1:mli2ei3ee1:zi1ee");
    assert_eq!(decode(&encoded).expect("decode"), value);
}

#[test]
fn constructors_sort_unsorted_keys() {
    let value = Value::dict(vec![
        (b"z", Value::int(1)),
        (b"a", Value::int(2)),
        (b"m", Value::int(3)),
    ])
    .expect("dictionary");
    let encoded = encode(&value).expect("encode");

    assert_eq!(encoded, b"d1:ai2e1:mi3e1:zi1ee");
    assert_eq!(decode(&encoded).expect("decode"), value);
}

#[test]
fn constructors_reject_duplicate_keys() {
    let result = Value::dict(vec![(b"a", Value::int(1)), (b"a", Value::int(2))]);

    assert!(matches!(result, Err(Error::DuplicateDictKey { offset: 0 })));
}

#[test]
fn constructors_have_empty_spans() {
    let value = Value::dict(vec![(b"a", Value::int(1))]).expect("dictionary");

    assert_eq!(value.span(), 0..0);
    assert_eq!(value.as_dict().expect("dict").len(), 1);
}

#[test]
fn encode_into_appends() {
    let value = decode(b"i1e").unwrap();
    let mut out = b"prefix".to_vec();

    encode_into(&value, &mut out).unwrap();

    assert_eq!(&out[..6], b"prefix");
    assert_eq!(&out[6..], b"i1e");
}

#[test]
fn encoder_enforces_limits_and_is_transactional() {
    let long_string = vec![b'a'; 5];
    let mut out = Vec::new();

    let value = raw_value(Kind::Bytes(long_string.as_slice()));
    assert!(matches!(
        encode_with_limits(
            &value,
            &mut out,
            Limits {
                max_string_bytes: 4,
                ..Limits::default()
            }
        ),
        Err(Error::EncodeStringTooLong { .. })
    ));
    assert!(out.is_empty());

    let value = raw_value(Kind::List(vec![
        raw_value(Kind::Int(1)),
        raw_value(Kind::Int(2)),
    ]));
    assert!(matches!(
        encode_with_limits(
            &value,
            &mut out,
            Limits {
                max_container_items: 1,
                ..Limits::default()
            }
        ),
        Err(Error::EncodeTooManyItems { .. })
    ));
    assert!(out.is_empty());

    let value = raw_value(Kind::List(vec![raw_value(Kind::List(vec![raw_value(
        Kind::List(vec![raw_value(Kind::Int(1))]),
    )]))]));
    assert!(matches!(
        encode_with_limits(
            &value,
            &mut out,
            Limits {
                max_depth: 2,
                ..Limits::default()
            }
        ),
        Err(Error::EncodeDepthLimit { .. })
    ));
    assert!(out.is_empty());

    let value = raw_value(Kind::Bytes(long_string.as_slice()));
    assert!(matches!(
        encode_with_limits(
            &value,
            &mut out,
            Limits {
                max_input_bytes: 4,
                ..Limits::default()
            }
        ),
        Err(Error::OutputTooLarge { .. })
    ));
    assert!(out.is_empty());
}

#[test]
fn encoder_restores_output_after_error() {
    let value = raw_value(Kind::List(vec![raw_value(Kind::List(vec![raw_value(
        Kind::List(vec![raw_value(Kind::Int(1))]),
    )]))]));
    let mut out = b"prefix".to_vec();

    let result = encode_with_limits(
        &value,
        &mut out,
        Limits {
            max_depth: 1,
            ..Limits::default()
        },
    );

    assert!(result.is_err());
    assert_eq!(out, b"prefix");
}
