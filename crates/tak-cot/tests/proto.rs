//! Integration tests for `tak_cot::proto::view_to_takmessage`.
//!
//! Verify the upstream rule: typed sub-messages populate when all required
//! fields are present; unconsumed children fall through verbatim to
//! `Detail.xmlDetail`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tak_cot::proto::view_to_takmessage;
use tak_cot::xml::decode_xml;

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
