use std::collections::HashMap;

use metor_proto_wkt::MsgMetadata;
use postcard_schema::Schema;
use postcard_schema::schema::owned::OwnedNamedType;

use super::*;

fn ts(n: i64) -> Timestamp {
    Timestamp(n)
}

/// `in_range` keeps only the events inside the range and returns them ascending,
/// regardless of input order.
#[test]
fn in_range_filters_and_sorts() {
    let items = [
        (ts(30), 'c'),
        (ts(5), 'a'),  // before range
        (ts(10), 'b'), // range start is inclusive
        (ts(25), 'd'),
        (ts(40), 'e'), // range end is exclusive
    ];
    let kept = in_range(items.into_iter(), &(ts(10)..ts(40)));
    let seen: Vec<(i64, char)> = kept.iter().map(|(t, c)| (t.0, *c)).collect();
    assert_eq!(seen, vec![(10, 'b'), (25, 'd'), (30, 'c')]);
}

/// Over the cap, only the newest [`MAX_EVENTS`] survive, still ascending.
#[test]
fn in_range_caps_to_newest() {
    let total = MAX_EVENTS as i64 + 50;
    let items = (0..total).map(|n| (ts(n), n));
    let kept = in_range(items, &(ts(0)..ts(total)));
    assert_eq!(kept.len(), MAX_EVENTS);
    // The oldest 50 were dropped; the first kept is index 50 and order ascends.
    assert_eq!(kept.first().unwrap().1, 50);
    assert_eq!(kept.last().unwrap().1, total - 1);
    assert!(kept.windows(2).all(|w| w[0].0 <= w[1].0));
}

/// Kind keys round-trip through their saved-layout string form, and message
/// ids survive as lowercase hex.
#[test]
fn kind_key_string_round_trip() {
    for key in [
        EventKindKey::Logs,
        EventKindKey::Alarms,
        EventKindKey::Sequences,
        EventKindKey::Msg([0xe0, 0x3c]),
    ] {
        let s = kind_key_to_string(key);
        assert_eq!(kind_key_from_string(&s), Some(key));
    }
    // Exact wire form for a message id.
    assert_eq!(kind_key_to_string(EventKindKey::Msg([0xe0, 0x3c])), "msg:e03c");
    assert_eq!(
        kind_key_from_string("msg:e03c"),
        Some(EventKindKey::Msg([0xe0, 0x3c]))
    );
    // Malformed ids and unknown tags drop.
    assert_eq!(kind_key_from_string("msg:zz"), None);
    assert_eq!(kind_key_from_string("msg:e03"), None);
    assert_eq!(kind_key_from_string("bogus"), None);
}

/// With a schema present, a record decodes to JSON; without one, or on a decode
/// failure, it falls back to the raw byte length.
#[test]
fn decode_payload_json_and_raw() {
    let schema: OwnedNamedType = u32::SCHEMA.into();
    let meta = MsgMetadata {
        name: "widget".into(),
        schema,
        metadata: HashMap::new(),
    };
    let bytes = postcard::to_allocvec(&7u32).unwrap();

    match decode_payload(Some(&meta), &bytes) {
        EventDetail::Json(value) => assert_eq!(value.as_u64(), Some(7)),
        other => panic!("expected Json, got {other:?}"),
    }

    // No schema announced yet: raw length only.
    assert!(matches!(
        decode_payload(None, &bytes),
        EventDetail::Raw(len) if len == bytes.len()
    ));

    // Schema present but bytes don't satisfy it (a u32 needs more than one byte):
    // decode fails and we fall back to raw.
    assert!(matches!(
        decode_payload(Some(&meta), &[]),
        EventDetail::Raw(0)
    ));
}
