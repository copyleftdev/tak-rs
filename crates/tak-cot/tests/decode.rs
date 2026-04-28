//! Integration tests for `tak_cot::xml::decode_xml` against the canonical
//! fixture corpus.
//!
//! These exercise the borrowed-view design end-to-end: every assertion that a
//! field has a specific value is implicitly an assertion that we can read it
//! out as a `&str` slice into the input.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tak_cot::xml::{CotEventView, decode_xml};

const PLI: &str = include_str!("fixtures/01_pli.xml");
const CHAT: &str = include_str!("fixtures/02_chat.xml");
const GEOFENCE: &str = include_str!("fixtures/03_geofence.xml");
const ROUTE: &str = include_str!("fixtures/04_route.xml");
const DRAWING: &str = include_str!("fixtures/05_drawing.xml");

#[test]
fn pli_decodes_with_full_detail() {
    let view = decode_xml(PLI).expect("pli decodes");
    assert_eq!(view.event.kind, "a-f-G-U-C");
    assert_eq!(view.event.uid, "ANDROID-deadbeef0001");
    assert_eq!(view.detail.children.len(), 7);

    let names: Vec<&str> = view.detail.children.iter().map(|c| c.name).collect();
    assert!(names.contains(&"takv"));
    assert!(names.contains(&"__group"));
    assert!(names.contains(&"track"));
}

#[test]
fn chat_decodes_with_nested_chatgrp() {
    let view = decode_xml(CHAT).expect("chat decodes");
    assert_eq!(view.event.kind, "b-t-f");
    assert!(view.event.uid.starts_with("GeoChat."));

    // The __chat element is a top-level child; chatgrp is its child and
    // should NOT appear at detail's top level (depth-3 only).
    let names: Vec<&str> = view.detail.children.iter().map(|c| c.name).collect();
    assert!(names.contains(&"__chat"));
    assert!(names.contains(&"remarks"));
    assert!(
        !names.contains(&"chatgrp"),
        "chatgrp is nested, not top-level"
    );
}

#[test]
fn geofence_with_self_closing_links() {
    let view = decode_xml(GEOFENCE).expect("geofence decodes");
    assert_eq!(view.event.kind, "u-d-r");

    // 4 corner <link/> elements — all self-closing — should be captured.
    let link_count = view
        .detail
        .children
        .iter()
        .filter(|c| c.name == "link")
        .count();
    assert_eq!(link_count, 4, "expected 4 corner links, got {link_count}");
}

#[test]
fn route_with_five_waypoints() {
    let view = decode_xml(ROUTE).expect("route decodes");
    assert_eq!(view.event.kind, "b-m-r");

    let link_count = view
        .detail
        .children
        .iter()
        .filter(|c| c.name == "link")
        .count();
    assert_eq!(link_count, 5);
}

#[test]
fn drawing_with_seven_freehand_points() {
    let view = decode_xml(DRAWING).expect("drawing decodes");
    assert_eq!(view.event.kind, "u-d-f");

    let link_count = view
        .detail
        .children
        .iter()
        .filter(|c| c.name == "link")
        .count();
    assert_eq!(link_count, 7);
}

#[test]
fn detail_raw_is_lossless_slice_of_input() {
    // The raw field on DetailView must be a verbatim slice of the input
    // (modulo whitespace trim). Round-trip C1 will use this to reconstruct
    // xmlDetail on the protobuf side.
    for (name, src) in [
        ("pli", PLI),
        ("chat", CHAT),
        ("geofence", GEOFENCE),
        ("route", ROUTE),
        ("drawing", DRAWING),
    ] {
        let view = decode_xml(src).unwrap_or_else(|e| panic!("{name}: {e}"));
        let raw = view.detail.raw;
        assert!(!raw.is_empty(), "{name}: detail raw should be non-empty");
        assert!(
            ptr_in(raw, src),
            "{name}: detail.raw is not a slice into the input — borrowing broken"
        );
        assert!(
            raw.contains('<'),
            "{name}: detail.raw should contain element markup"
        );
    }
}

#[test]
fn missing_uid_errors() {
    let bad = r#"<?xml version="1.0"?><event type="a-f" time="t" start="t" stale="t" how="h"><point lat="0" lon="0" hae="0" ce="0" le="0"/></event>"#;
    let err = decode_xml(bad).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("uid"), "expected uid error, got: {msg}");
}

#[test]
fn entity_in_attribute_rejected() {
    // CoT in production never uses entities; we reject for invariant H2.
    let bad = r#"<?xml version="1.0"?><event uid="a&amp;b" type="a-f" time="t" start="t" stale="t" how="h"><point lat="0" lon="0" hae="0" ce="0" le="0"/></event>"#;
    let err = decode_xml(bad).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("entity"),
        "expected entity-not-supported error, got: {msg}"
    );
}

fn ptr_in(s: &str, base: &str) -> bool {
    let s_start = s.as_ptr() as usize;
    let base_start = base.as_ptr() as usize;
    let base_end = base_start + base.len();
    s_start >= base_start && s_start + s.len() <= base_end
}

// Sanity: decode and forget; ensures all 5 fixtures are valid CoT and
// don't trigger any paths the per-fixture tests miss.
#[test]
fn all_fixtures_decode() {
    for src in [PLI, CHAT, GEOFENCE, ROUTE, DRAWING] {
        let _: CotEventView<'_> = decode_xml(src).unwrap();
    }
}
