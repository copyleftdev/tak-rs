//! Round-trip tests for `tak_cot::xml::encode_xml`.
//!
//! Every fixture is decoded, re-encoded, and decoded again. The two views
//! must compare equal on all observable fields. This is the C1 round-trip
//! invariant restricted to the canonical fixture corpus; the fully
//! generative proptest is issue #15.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tak_cot::xml::{CotEventView, decode_xml, encode_xml, encode_xml_to_string};

const FIXTURES: &[(&str, &str)] = &[
    ("01_pli", include_str!("fixtures/01_pli.xml")),
    ("02_chat", include_str!("fixtures/02_chat.xml")),
    ("03_geofence", include_str!("fixtures/03_geofence.xml")),
    ("04_route", include_str!("fixtures/04_route.xml")),
    ("05_drawing", include_str!("fixtures/05_drawing.xml")),
];

#[test]
fn round_trip_via_string_is_lossless_per_fixture() {
    for (name, src) in FIXTURES {
        let view1 = decode_xml(src).unwrap_or_else(|e| panic!("{name}: first decode: {e}"));
        let re_encoded =
            encode_xml_to_string(&view1).unwrap_or_else(|e| panic!("{name}: encode: {e}"));
        let view2 = decode_xml(&re_encoded)
            .unwrap_or_else(|e| panic!("{name}: re-decode failed:\n{re_encoded}\nerror: {e}"));
        assert_views_equivalent(name, &view1, &view2);
    }
}

#[test]
fn round_trip_via_writer() {
    let (name, src) = FIXTURES[0];
    let view = decode_xml(src).unwrap();
    let mut buf = Vec::with_capacity(1024);
    encode_xml(&view, &mut buf).unwrap();
    let s = std::str::from_utf8(&buf).unwrap();
    assert!(s.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
    assert!(
        s.contains("uid=\"ANDROID-deadbeef0001\""),
        "{name}: encoded missing uid"
    );
    assert!(
        s.contains("type=\"a-f-G-U-C\""),
        "{name}: encoded missing type"
    );
    assert!(
        s.contains("<takv "),
        "{name}: encoded detail missing takv child"
    );
}

#[test]
fn encoded_event_attrs_are_in_canonical_order() {
    let (_, src) = FIXTURES[0];
    let view = decode_xml(src).unwrap();
    let s = encode_xml_to_string(&view).unwrap();

    // Canonical order: version, uid, type, time, start, stale, how, ...
    let pos_version = s.find(" version=").unwrap();
    let pos_uid = s.find(" uid=").unwrap();
    let pos_type = s.find(" type=").unwrap();
    let pos_time = s.find(" time=").unwrap();
    let pos_start = s.find(" start=").unwrap();
    let pos_stale = s.find(" stale=").unwrap();
    let pos_how = s.find(" how=").unwrap();
    assert!(pos_version < pos_uid);
    assert!(pos_uid < pos_type);
    assert!(pos_type < pos_time);
    assert!(pos_time < pos_start);
    assert!(pos_start < pos_stale);
    assert!(pos_stale < pos_how);
}

#[test]
fn missing_uid_refuses_to_encode() {
    let mut view = CotEventView::default();
    view.event.kind = "a-f-G-U-C";
    let mut buf = Vec::new();
    let err = encode_xml(&view, &mut buf).unwrap_err();
    assert!(err.to_string().contains("uid"), "got: {err}");
}

#[test]
fn special_char_in_value_refuses_to_encode() {
    let mut view = CotEventView::default();
    view.event.uid = "evil<uid";
    view.event.kind = "a-f";
    let mut buf = Vec::new();
    let err = encode_xml(&view, &mut buf).unwrap_err();
    assert!(
        err.to_string().contains("special character") || err.to_string().contains("<"),
        "got: {err}"
    );
}

#[test]
fn detail_raw_round_trips_byte_equivalent() {
    // The DetailView::raw slice is the lossless preservation channel for
    // xmlDetail content. After encode→decode, the raw should equal the
    // original (modulo whitespace trim) per fixture.
    for (name, src) in FIXTURES {
        let view1 = decode_xml(src).unwrap();
        let s = encode_xml_to_string(&view1).unwrap();
        let view2 = decode_xml(&s).unwrap();
        assert_eq!(
            view1.detail.raw, view2.detail.raw,
            "{name}: detail.raw drifted across round-trip"
        );
    }
}

#[test]
fn no_point_fixture_still_encodes() {
    let no_point = r#"<?xml version="1.0" encoding="UTF-8"?>
<event uid="x" type="a-f" time="t" start="s" stale="z" how="m-g">
  <detail/>
</event>"#;
    let view = decode_xml(no_point).expect("decodes without point");
    assert!(view.point.is_none());
    let s = encode_xml_to_string(&view).expect("encodes without point");
    assert!(!s.contains("<point"), "encoder emitted phantom point: {s}");
}

fn assert_views_equivalent(label: &str, a: &CotEventView<'_>, b: &CotEventView<'_>) {
    assert_eq!(a.event.version, b.event.version, "{label}: version");
    assert_eq!(a.event.uid, b.event.uid, "{label}: uid");
    assert_eq!(a.event.kind, b.event.kind, "{label}: kind");
    assert_eq!(a.event.time, b.event.time, "{label}: time");
    assert_eq!(a.event.start, b.event.start, "{label}: start");
    assert_eq!(a.event.stale, b.event.stale, "{label}: stale");
    assert_eq!(a.event.how, b.event.how, "{label}: how");
    assert_eq!(a.event.access, b.event.access, "{label}: access");
    assert_eq!(a.event.qos, b.event.qos, "{label}: qos");
    assert_eq!(a.event.opex, b.event.opex, "{label}: opex");
    assert_eq!(a.event.caveat, b.event.caveat, "{label}: caveat");
    assert_eq!(
        a.event.releaseable_to, b.event.releaseable_to,
        "{label}: releaseable_to"
    );

    match (&a.point, &b.point) {
        (Some(p1), Some(p2)) => {
            assert_eq!(p1.lat, p2.lat, "{label}: lat");
            assert_eq!(p1.lon, p2.lon, "{label}: lon");
            assert_eq!(p1.hae, p2.hae, "{label}: hae");
            assert_eq!(p1.ce, p2.ce, "{label}: ce");
            assert_eq!(p1.le, p2.le, "{label}: le");
        }
        (None, None) => {}
        _ => panic!("{label}: point presence mismatch"),
    }

    let a_names: Vec<&str> = a.detail.children.iter().map(|c| c.name).collect();
    let b_names: Vec<&str> = b.detail.children.iter().map(|c| c.name).collect();
    assert_eq!(a_names, b_names, "{label}: detail children list");
}
