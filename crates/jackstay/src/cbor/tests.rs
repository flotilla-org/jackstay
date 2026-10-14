use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Setup {
    Offer { tracks: Vec<Track>, clock: f64 },
    Selection { track_ids: Vec<u64>, writer: Option<u64> },
}

impl Message for Setup {
    const TYPES: &'static [&'static str] = &["offer", "selection"];
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Track {
    id: u64,
    kind: String,
    gain: f32,
    descriptor: Bytes,
}

fn offer() -> Setup {
    Setup::Offer {
        tracks: vec![Track {
            id: u64::MAX,
            kind: "org.example.custom".into(),
            gain: 0.5,
            descriptor: Bytes(vec![0, 1, 2]),
        }],
        clock: 1.0,
    }
}

#[test]
fn typed_message_round_trips_and_sorts_keys() {
    let bytes = encode_message(&offer(), &SETUP_LIMITS).unwrap();
    // Shorter encoded keys sort first.
    let value = decode_value(&bytes, &SETUP_LIMITS).unwrap();
    let Value::Map(entries) = &value else { panic!() };
    let keys: Vec<_> = entries.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["type", "clock", "tracks"]);
    // The integral clock is reduced to an integer, and reads back as a float.
    assert_eq!(entries[1].1, Value::Integer(1));
    assert_eq!(decode_message::<Setup>(&bytes, &SETUP_LIMITS).unwrap(), offer());
    assert_eq!(encode_value(&value, &SETUP_LIMITS).unwrap(), bytes);
}

#[test]
fn unknown_keys_are_ignored_and_unknown_types_rejected() {
    let value = Value::Map(vec![
        ("type".into(), Value::Text("selection".into())),
        ("future".into(), Value::Array(vec![Value::Null])),
        ("track_ids".into(), Value::Array(vec![Value::Integer(7)])),
        ("writer".into(), Value::Null),
    ]);
    let bytes = encode_value(&value, &SETUP_LIMITS).unwrap();
    assert_eq!(
        decode_message::<Setup>(&bytes, &SETUP_LIMITS).unwrap(),
        Setup::Selection {
            track_ids: vec![7],
            writer: None
        }
    );
    let unknown = encode_value(&Value::Map(vec![("type".into(), Value::Text("grant".into()))]), &SETUP_LIMITS).unwrap();
    assert!(matches!(decode_message::<Setup>(&unknown, &SETUP_LIMITS), Err(Error::UnknownType(t)) if t == "grant"));
}

#[test]
fn encoding_checks_the_message_type() {
    #[derive(Serialize, Deserialize)]
    struct Untyped {
        a: u8,
    }
    impl Message for Untyped {
        const TYPES: &'static [&'static str] = &["x"];
    }
    assert!(matches!(encode_message(&Untyped { a: 1 }, &SETUP_LIMITS), Err(Error::MissingType)));
}

#[test]
fn floats_keep_their_exact_value() {
    for f in [
        -0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.1,
        1e300,
        -1e19,
        18_446_744_073_709_551_616.0,
        5e-324,
    ] {
        let bytes = to_vec(&f, &DEFAULT_LIMITS).unwrap();
        let back: f64 = from_slice(&bytes, &DEFAULT_LIMITS).unwrap();
        assert_eq!(back.to_bits(), f.to_bits(), "{f}");
    }
    assert!(matches!(to_vec(&f64::NAN, &DEFAULT_LIMITS), Err(Error::Nan)));
    // An f32 field takes only what binary32 holds exactly.
    let bytes = to_vec(&0.1f64, &DEFAULT_LIMITS).unwrap();
    assert!(matches!(from_slice::<f32>(&bytes, &DEFAULT_LIMITS), Err(Error::Schema(_))));
    let bytes = to_vec(&0.1f32, &DEFAULT_LIMITS).unwrap();
    assert_eq!(from_slice::<f32>(&bytes, &DEFAULT_LIMITS).unwrap(), 0.1f32);
    // An integer that no f64 holds exactly is not read as one.
    let bytes = to_vec(&u64::MAX, &DEFAULT_LIMITS).unwrap();
    assert!(matches!(from_slice::<f64>(&bytes, &DEFAULT_LIMITS), Err(Error::Schema(_))));
}

#[test]
fn integers_cover_the_cbor_range() {
    for n in [i128::from(u64::MAX), -(1 << 64), i128::from(i64::MIN), -1] {
        let bytes = to_vec(&n, &DEFAULT_LIMITS).unwrap();
        assert_eq!(from_slice::<i128>(&bytes, &DEFAULT_LIMITS).unwrap(), n);
    }
    assert!(matches!(to_vec(&(1i128 << 64), &DEFAULT_LIMITS), Err(Error::IntegerOutOfRange)));
    assert!(matches!(
        to_vec(&(-(1i128 << 64) - 1), &DEFAULT_LIMITS),
        Err(Error::IntegerOutOfRange)
    ));
    let bytes = to_vec(&-1i64, &DEFAULT_LIMITS).unwrap();
    assert!(matches!(from_slice::<u64>(&bytes, &DEFAULT_LIMITS), Err(Error::Schema(_))));
}

#[test]
fn externally_tagged_enums_and_options() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Shape {
        Empty,
        Circle(f64),
        Pair(u8, u8),
        Rect { w: u32, h: u32 },
    }
    let shapes = vec![Shape::Empty, Shape::Circle(1.5), Shape::Pair(1, 2), Shape::Rect { w: 3, h: 4 }];
    let bytes = to_vec(&shapes, &DEFAULT_LIMITS).unwrap();
    assert_eq!(from_slice::<Vec<Shape>>(&bytes, &DEFAULT_LIMITS).unwrap(), shapes);
    let maybe: Vec<Option<u8>> = vec![None, Some(3)];
    let bytes = to_vec(&maybe, &DEFAULT_LIMITS).unwrap();
    assert_eq!(bytes, [0x82, 0xf6, 0x03]);
    assert_eq!(from_slice::<Vec<Option<u8>>>(&bytes, &DEFAULT_LIMITS).unwrap(), maybe);
}

#[test]
fn map_keys_must_be_text() {
    let map = BTreeMap::from([(1u32, 2u32)]);
    assert!(matches!(to_vec(&map, &DEFAULT_LIMITS), Err(Error::NonTextKey)));
}

#[test]
fn serializer_duplicates_are_rejected() {
    #[derive(Serialize)]
    struct Inner {
        a: u8,
    }
    #[derive(Serialize)]
    struct Outer {
        a: u8,
        #[serde(flatten)]
        inner: Inner,
    }
    let result = to_vec(
        &Outer {
            a: 1,
            inner: Inner { a: 2 },
        },
        &DEFAULT_LIMITS,
    );
    assert!(matches!(result, Err(Error::DuplicateKey(k)) if k == "a"));
}

#[test]
fn byte_strings_need_the_bytes_wrapper() {
    let bytes = to_vec(&Bytes(vec![1, 2]), &DEFAULT_LIMITS).unwrap();
    assert_eq!(bytes, [0x42, 1, 2]);
    // An array of integers is not a byte string.
    let array = to_vec(&vec![1u8, 2], &DEFAULT_LIMITS).unwrap();
    assert!(from_slice::<Bytes>(&array, &DEFAULT_LIMITS).is_err());
}

#[test]
fn encoding_enforces_limits() {
    let descriptor = Bytes(vec![0; SETUP_MAX_DESCRIPTOR + 1]);
    assert!(matches!(to_vec(&descriptor, &SETUP_LIMITS), Err(Error::ByteStringLimit { .. })));
    let mut deep = Value::Array(vec![]);
    for _ in 0..64 {
        deep = Value::Array(vec![deep]);
    }
    assert!(matches!(encode_value(&deep, &DEFAULT_LIMITS), Err(Error::DepthLimit { limit: 64 })));
    let text = "a".repeat(SETUP_LIMITS.max_message);
    assert!(matches!(to_vec(&text, &SETUP_LIMITS), Err(Error::MessageLimit { .. })));
}

#[test]
fn a_map_at_the_element_limit_decodes() {
    // 65,536 distinct keys, in canonical order: length first, then bytewise.
    let mut map = BTreeMap::new();
    for n in 0..DEFAULT_LIMITS.max_elements {
        map.insert(format!("{n:05}"), ());
    }
    let bytes = to_vec(&map, &DEFAULT_LIMITS).unwrap();
    assert_eq!(&bytes[..5], [0xba, 0x00, 0x01, 0x00, 0x00]);
    assert_eq!(from_slice::<BTreeMap<String, ()>>(&bytes, &DEFAULT_LIMITS).unwrap(), map);
    map.insert("x".into(), ());
    assert!(matches!(to_vec(&map, &DEFAULT_LIMITS), Err(Error::ElementLimit { .. })));
}

#[test]
fn framing_reads_and_writes_streams() {
    let framing = Framing::new(*b"JSTEST01", SETUP_LIMITS);
    let mut wire = Vec::new();
    framing.write(&mut wire, &offer()).unwrap();
    framing
        .write(
            &mut wire,
            &Setup::Selection {
                track_ids: vec![],
                writer: Some(3),
            },
        )
        .unwrap();
    let mut reader = wire.as_slice();
    assert_eq!(framing.read::<Setup, _>(&mut reader).unwrap(), offer());
    assert!(matches!(
        framing.read::<Setup, _>(&mut reader).unwrap(),
        Setup::Selection { writer: Some(3), .. }
    ));
    assert!(matches!(framing.read::<Setup, _>(&mut reader), Err(Error::Io(_))));
    // A header over the limit fails before any body is read or allocated.
    let mut header = b"JSTEST01".to_vec();
    header.extend_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        framing.read::<Setup, _>(&mut header.as_slice()),
        Err(Error::MessageLimit { .. })
    ));
}
