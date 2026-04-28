//! Integration tests for `tak_cot::proto::view_to_takmessage`.
//!
//! Verify the upstream rule: typed sub-messages populate when all required
//! fields are present; unconsumed children fall through verbatim to
//! `Detail.xmlDetail`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tak_cot::proto::{takmessage_to_xml, takmessage_to_xml_string, view_to_takmessage};
use tak_cot::xml::decode_xml;
use tak_proto::v1::{Detail, TakMessage};

const PLI: &str = include_str!("fixtures/01_pli.xml");
const CHAT: &str = include_str!("fixtures/02_chat.xml");
const GEOFENCE: &str = include_str!("fixtures/03_geofence.xml");
const ROUTE: &str = include_str!("fixtures/04_route.xml");
const DRAWING: &str = include_str!("fixtures/05_drawing.xml");

#[test]
fn pli_populates_all_six_typed_subs() {
    let view = decode_xml(PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let cot = msg.cot_event.expect("cot_event present");
    let detail = cot.detail.expect("detail present");

    let contact = detail.contact.as_ref().expect("contact populated");
    assert_eq!(contact.callsign, "VIPER01");
    assert_eq!(contact.endpoint, "*:-1:stcp");

    let group = detail.group.as_ref().expect("group populated");
    assert_eq!(group.name, "Cyan");
    assert_eq!(group.role, "Team Member");

    let pl = detail
        .precision_location
        .as_ref()
        .expect("precision_location populated");
    assert_eq!(pl.geopointsrc, "GPS");
    assert_eq!(pl.altsrc, "GPS");

    let status = detail.status.as_ref().expect("status populated");
    assert_eq!(status.battery, 78);

    let takv = detail.takv.as_ref().expect("takv populated");
    assert_eq!(takv.platform, "ATAK-CIV");
    assert_eq!(takv.device, "GOOGLE PIXEL 6");
    assert_eq!(takv.os, "29");
    assert_eq!(takv.version, "4.10.0.4");

    let track = detail.track.as_ref().expect("track populated");
    assert!((track.course - 180.0).abs() < 1e-9);
    assert!((track.speed - 0.512345).abs() < 1e-9);
}

#[test]
fn pli_uid_element_falls_through_to_xml_detail() {
    let view = decode_xml(PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let detail = msg.cot_event.unwrap().detail.unwrap();
    // The <uid Droid="VIPER01"/> element has no typed counterpart; it must
    // appear verbatim in xmlDetail.
    assert!(
        detail.xml_detail.contains(r#"<uid Droid="VIPER01"/>"#),
        "xmlDetail missing uid element:\n{}",
        detail.xml_detail
    );
    // The other elements WERE consumed and must NOT appear in xmlDetail.
    for consumed in &[
        "callsign=\"VIPER01\"",
        "name=\"Cyan\"",
        "battery=\"78\"",
        "platform=\"ATAK-CIV\"",
    ] {
        assert!(
            !detail.xml_detail.contains(consumed),
            "xmlDetail still contains consumed text `{consumed}`:\n{}",
            detail.xml_detail
        );
    }
}

#[test]
fn chat_falls_entirely_through_to_xml_detail() {
    let view = decode_xml(CHAT).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let detail = msg.cot_event.unwrap().detail.unwrap();
    // Chat events have no typed sub-messages.
    assert!(detail.contact.is_none());
    assert!(detail.group.is_none());
    assert!(detail.takv.is_none());
    assert!(detail.status.is_none());
    assert!(detail.precision_location.is_none());
    assert!(detail.track.is_none());
    // Everything in detail must appear verbatim in xmlDetail.
    for needle in &["__chat", "remarks", "__serverdestination", "marti"] {
        assert!(
            detail.xml_detail.contains(needle),
            "{needle} missing from xmlDetail"
        );
    }
}

#[test]
fn geofence_route_drawing_all_in_xml_detail() {
    for (name, src) in [
        ("geofence", GEOFENCE),
        ("route", ROUTE),
        ("drawing", DRAWING),
    ] {
        let view = decode_xml(src).unwrap();
        let msg = view_to_takmessage(&view).unwrap();
        let detail = msg.cot_event.unwrap().detail.unwrap();
        assert!(
            detail.contact.is_none(),
            "{name}: should have no typed contact"
        );
        assert!(
            detail.xml_detail.contains("<link"),
            "{name}: xmlDetail missing link elements"
        );
        // Geofence has precisionlocation present in source, but only with
        // altsrc="???" — let's verify it still populates because both fields
        // are non-empty strings.
        if name == "geofence" {
            assert!(detail.precision_location.is_some());
        }
    }
}

#[test]
fn partial_typed_attrs_fall_back_to_xml_detail() {
    // Hand-crafted case: contact with only callsign (missing endpoint).
    // Must NOT populate Detail.contact; the element stays in xmlDetail.
    let bad = r#"<?xml version="1.0" encoding="UTF-8"?>
<event uid="x" type="a-f" time="2026-04-27T05:00:00Z" start="2026-04-27T05:00:00Z" stale="2026-04-28T05:00:00Z" how="m-g">
  <point lat="0" lon="0" hae="0" ce="0" le="0"/>
  <detail>
    <contact callsign="ALPHA"/>
  </detail>
</event>"#;
    let view = decode_xml(bad).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let detail = msg.cot_event.unwrap().detail.unwrap();
    assert!(
        detail.contact.is_none(),
        "incomplete contact should NOT populate Detail.contact"
    );
    assert!(
        detail.xml_detail.contains(r#"callsign="ALPHA""#),
        "incomplete contact should appear in xmlDetail"
    );
}

#[test]
fn timestamps_round_trip_to_milliseconds() {
    // 2026-04-27T05:00:00.000Z = 1_777_266_000_000 ms since epoch.
    // (56 years × 365 days + 14 leap days + 116 days into 2026 + 5h)
    let view = decode_xml(PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let cot = msg.cot_event.unwrap();
    assert_eq!(cot.send_time, 1_777_266_000_000);
    assert_eq!(cot.start_time, 1_777_266_000_000);
    // Stale is 90s later: 2026-04-27T05:01:30.000Z
    assert_eq!(cot.stale_time, 1_777_266_090_000);
}

#[test]
fn coords_parse_to_f64() {
    let view = decode_xml(PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let cot = msg.cot_event.unwrap();
    assert!((cot.lat - 34.225_700).abs() < 1e-9);
    assert!((cot.lon - -118.573_900).abs() < 1e-9);
    assert!((cot.hae - 245.0).abs() < 1e-9);
    assert!((cot.ce - 9.0).abs() < 1e-9);
}

#[test]
fn cot_event_type_field_populated() {
    // Sanity check that we wrote the keyword-clashing field correctly.
    let view = decode_xml(PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let cot = msg.cot_event.unwrap();
    assert_eq!(cot.r#type, "a-f-G-U-C");
}

#[test]
fn unconsumed_children_preserve_document_order() {
    // Build a synthetic event with a typed contact + several untyped
    // siblings. xmlDetail should preserve the document order of the
    // untyped ones with the contact stripped out.
    let src = r#"<?xml version="1.0" encoding="UTF-8"?>
<event uid="x" type="a-f" time="2026-04-27T05:00:00Z" start="2026-04-27T05:00:00Z" stale="2026-04-28T05:00:00Z" how="m-g">
  <point lat="0" lon="0" hae="0" ce="0" le="0"/>
  <detail>
    <foo a="1"/>
    <contact endpoint="*:-1:stcp" callsign="X"/>
    <bar b="2"/>
    <baz c="3"/>
  </detail>
</event>"#;
    let view = decode_xml(src).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let detail = msg.cot_event.unwrap().detail.unwrap();
    assert!(detail.contact.is_some());
    let xd = &detail.xml_detail;
    let foo = xd.find("<foo").expect("foo present");
    let bar = xd.find("<bar").expect("bar present");
    let baz = xd.find("<baz").expect("baz present");
    assert!(foo < bar && bar < baz, "document order broken: {xd}");
    assert!(!xd.contains("<contact"), "contact leaked into xmlDetail");
}

// ===========================================================================
// TakMessage → XML round-trips (issue #14)
// ===========================================================================

fn round_trip_takmessage(label: &str, src: &str) {
    let view = decode_xml(src).unwrap_or_else(|e| panic!("{label}: first decode: {e}"));
    let msg1 = view_to_takmessage(&view).unwrap_or_else(|e| panic!("{label}: convert 1: {e}"));
    let xml = takmessage_to_xml_string(&msg1)
        .unwrap_or_else(|e| panic!("{label}: re-encode: {e}\nview: {view:#?}"));
    let view2 = decode_xml(&xml)
        .unwrap_or_else(|e| panic!("{label}: re-decode failed:\n{xml}\nerror: {e}"));
    let msg2 =
        view_to_takmessage(&view2).unwrap_or_else(|e| panic!("{label}: convert 2: {e}\n{xml}"));

    let cot1 = msg1.cot_event.as_ref().expect("cot1 present");
    let cot2 = msg2.cot_event.as_ref().expect("cot2 present");
    let det1 = cot1.detail.as_ref().expect("det1 present");
    let det2 = cot2.detail.as_ref().expect("det2 present");

    assert_eq!(cot1.uid, cot2.uid, "{label}: uid");
    assert_eq!(cot1.r#type, cot2.r#type, "{label}: type");
    assert_eq!(cot1.send_time, cot2.send_time, "{label}: send_time");
    assert_eq!(cot1.start_time, cot2.start_time, "{label}: start_time");
    assert_eq!(cot1.stale_time, cot2.stale_time, "{label}: stale_time");
    assert_eq!(cot1.how, cot2.how, "{label}: how");
    assert!((cot1.lat - cot2.lat).abs() < 1e-9, "{label}: lat");
    assert!((cot1.lon - cot2.lon).abs() < 1e-9, "{label}: lon");
    assert_eq!(det1.contact, det2.contact, "{label}: contact");
    assert_eq!(det1.group, det2.group, "{label}: group");
    assert_eq!(
        det1.precision_location, det2.precision_location,
        "{label}: precision_location"
    );
    assert_eq!(det1.status, det2.status, "{label}: status");
    assert_eq!(det1.takv, det2.takv, "{label}: takv");
    assert_eq!(det1.track, det2.track, "{label}: track");

    // Detail children equivalence: same SET of element names. Order changes
    // because the encoder emits typed subs first then xml_detail, regardless
    // of the original document order. CoT XML doesn't require detail-child
    // ordering, so this is semantically lossless.
    let mut names1: Vec<&str> = view.detail.children.iter().map(|c| c.name).collect();
    let mut names2: Vec<&str> = view2.detail.children.iter().map(|c| c.name).collect();
    names1.sort_unstable();
    names2.sort_unstable();
    assert_eq!(names1, names2, "{label}: detail children set differs");
}

#[test]
fn pli_takmessage_round_trip() {
    round_trip_takmessage("pli", PLI);
}

#[test]
fn chat_takmessage_round_trip() {
    round_trip_takmessage("chat", CHAT);
}

#[test]
fn geofence_takmessage_round_trip() {
    round_trip_takmessage("geofence", GEOFENCE);
}

#[test]
fn route_takmessage_round_trip() {
    round_trip_takmessage("route", ROUTE);
}

#[test]
fn drawing_takmessage_round_trip() {
    round_trip_takmessage("drawing", DRAWING);
}

#[test]
fn takmessage_xml_format_has_iso8601_milliseconds() {
    let view = decode_xml(PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let xml = takmessage_to_xml_string(&msg).unwrap();
    // Round-trip from u64 ms → ISO-8601 with 3-decimal seconds.
    assert!(
        xml.contains(r#"time="2026-04-27T05:00:00.000Z""#),
        "expected formatted ISO-8601 time in:\n{xml}"
    );
}

#[test]
fn takmessage_xml_emits_typed_subs_as_xml() {
    let view = decode_xml(PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let xml = takmessage_to_xml_string(&msg).unwrap();
    assert!(xml.contains("<contact"), "missing <contact>:\n{xml}");
    assert!(xml.contains("<__group"), "missing <__group>:\n{xml}");
    assert!(xml.contains("<takv"), "missing <takv>:\n{xml}");
    assert!(xml.contains("<status"), "missing <status>:\n{xml}");
    assert!(xml.contains("<track"), "missing <track>:\n{xml}");
    assert!(
        xml.contains("<precisionlocation"),
        "missing <precisionlocation>:\n{xml}"
    );
}

#[test]
fn takmessage_xml_appends_xml_detail_after_typed_subs() {
    let view = decode_xml(PLI).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let xml = takmessage_to_xml_string(&msg).unwrap();
    // The <uid Droid="VIPER01"/> element fell to xml_detail in #13;
    // it should reappear in the encoded XML.
    assert!(
        xml.contains(r#"<uid Droid="VIPER01"/>"#),
        "xml_detail content missing from emitted XML:\n{xml}"
    );
}

#[test]
fn takmessage_xml_partial_detail_emits_only_present_subs() {
    // Build a TakMessage with only `contact` populated; verify only
    // <contact> is emitted (not all six typed subs).
    let mut msg = TakMessage::default();
    let mut cot = tak_proto::v1::CotEvent {
        r#type: "a-f".to_owned(),
        uid: "x".to_owned(),
        how: "m-g".to_owned(),
        send_time: 1_777_266_000_000,
        start_time: 1_777_266_000_000,
        stale_time: 1_777_266_090_000,
        ..Default::default()
    };
    cot.detail = Some(Detail {
        contact: Some(tak_proto::v1::Contact {
            endpoint: "*:-1:stcp".to_owned(),
            callsign: "ALPHA".to_owned(),
        }),
        ..Default::default()
    });
    msg.cot_event = Some(cot);

    let xml = takmessage_to_xml_string(&msg).unwrap();
    assert!(xml.contains("<contact"));
    assert!(!xml.contains("<__group"));
    assert!(!xml.contains("<takv"));
    assert!(!xml.contains("<status"));
    assert!(!xml.contains("<track"));
    assert!(!xml.contains("<precisionlocation"));
}

#[test]
fn takmessage_xml_missing_cot_event_errors() {
    let msg = TakMessage::default();
    let mut buf = Vec::new();
    let err = takmessage_to_xml(&msg, &mut buf).unwrap_err();
    assert!(err.to_string().contains("cot_event"), "got: {err}");
}

#[test]
fn empty_detail_yields_empty_xml_detail() {
    let src = r#"<?xml version="1.0" encoding="UTF-8"?>
<event uid="x" type="a-f" time="2026-04-27T05:00:00Z" start="2026-04-27T05:00:00Z" stale="2026-04-28T05:00:00Z" how="m-g">
  <point lat="0" lon="0" hae="0" ce="0" le="0"/>
  <detail/>
</event>"#;
    let view = decode_xml(src).unwrap();
    let msg = view_to_takmessage(&view).unwrap();
    let detail = msg.cot_event.unwrap().detail.unwrap();
    assert_eq!(detail.xml_detail, "");
}
