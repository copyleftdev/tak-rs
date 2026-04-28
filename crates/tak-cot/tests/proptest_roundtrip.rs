//! Property test: invariant **C1** — lossless XML ↔ TakMessage round-trip.
//!
//! For arbitrary well-formed CoT XML, the pipeline
//! `decode_xml → view_to_takmessage → takmessage_to_xml → decode_xml →
//! view_to_takmessage` produces a `TakMessage` that compares equal to the
//! one from the first conversion. This is the property that makes the
//! TAK protocol bidirectional through tak-cot — break it and ATAK clients
//! see drift on every server hop.
//!
//! Two test variants:
//! - `cot_round_trip_stable_quick` runs under proptest's default budget
//!   (256 cases) and is part of every pre-push run.
//! - `cot_round_trip_stable_extended` runs 10_000 cases and is `#[ignore]`'d
//!   so it runs via `/check-invariants` or
//!   `cargo test -p tak-cot -- --ignored`.
//!
//! Generators:
//! - uid:       `[A-Za-z0-9_-]{5,30}`
//! - cot type:  CoT-style hyphenated codes (e.g. `a-f-G-U-C`)
//! - how:       short hyphenated codes
//! - times:     ms in `[2020-01-01, 2030-01-01)`, formatted via jiff
//! - point:     valid lat/lon ranges; bounded hae/ce/le
//! - detail:    0..=5 children, a mix of typed and untyped elements
//!
//! All string values are constrained to ASCII without XML-special chars
//! (`<`, `>`, `&`, `"`, `'`) — both the decoder and encoder reject those
//! by design (invariant H2 / `Error::EntityNotSupported` /
//! `Error::SpecialCharInValue`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value, // proptest's macro hands us owned args
    clippy::cast_possible_wrap       // ms (range-bounded u64) → i64 for jiff is sound
)]

use proptest::prelude::*;
use tak_cot::proto::{takmessage_to_xml_string, view_to_takmessage};
use tak_cot::xml::decode_xml;

#[derive(Debug, Clone)]
enum Child {
    Contact {
        endpoint: String,
        callsign: String,
    },
    Group {
        name: String,
        role: String,
    },
    PrecisionLocation {
        geopointsrc: String,
        altsrc: String,
    },
    Status {
        battery: u32,
    },
    Takv {
        device: String,
        platform: String,
        os: String,
        version: String,
    },
    Track {
        speed: f64,
        course: f64,
    },
    Untyped {
        name: String,
        attr_key: String,
        attr_val: String,
    },
}

fn render_child(c: &Child) -> String {
    match c {
        Child::Contact { endpoint, callsign } => {
            format!(r#"<contact endpoint="{endpoint}" callsign="{callsign}"/>"#)
        }
        Child::Group { name, role } => format!(r#"<__group name="{name}" role="{role}"/>"#),
        Child::PrecisionLocation {
            geopointsrc,
            altsrc,
        } => format!(r#"<precisionlocation geopointsrc="{geopointsrc}" altsrc="{altsrc}"/>"#),
        Child::Status { battery } => format!(r#"<status battery="{battery}"/>"#),
        Child::Takv {
            device,
            platform,
            os,
            version,
        } => format!(
            r#"<takv device="{device}" platform="{platform}" os="{os}" version="{version}"/>"#
        ),
        Child::Track { speed, course } => format!(r#"<track speed="{speed}" course="{course}"/>"#),
        Child::Untyped {
            name,
            attr_key,
            attr_val,
        } => format!(r#"<{name} {attr_key}="{attr_val}"/>"#),
    }
}

fn arb_safe(min: usize, max: usize) -> BoxedStrategy<String> {
    proptest::string::string_regex(&format!(r"[A-Za-z0-9 ._/\-:]{{{min},{max}}}"))
        .unwrap()
        .boxed()
}

fn arb_child() -> BoxedStrategy<Child> {
    prop_oneof![
        (arb_safe(3, 15), arb_safe(3, 15))
            .prop_map(|(endpoint, callsign)| Child::Contact { endpoint, callsign }),
        (arb_safe(3, 15), arb_safe(3, 15)).prop_map(|(name, role)| Child::Group { name, role }),
        (arb_safe(3, 10), arb_safe(3, 10)).prop_map(|(geopointsrc, altsrc)| {
            Child::PrecisionLocation {
                geopointsrc,
                altsrc,
            }
        }),
        (0u32..=100).prop_map(|battery| Child::Status { battery }),
        (
            arb_safe(3, 15),
            arb_safe(3, 15),
            arb_safe(1, 5),
            arb_safe(3, 15),
        )
            .prop_map(|(device, platform, os, version)| Child::Takv {
                device,
                platform,
                os,
                version,
            }),
        (-100.0f64..=100.0f64, -360.0f64..=360.0f64)
            .prop_map(|(speed, course)| Child::Track { speed, course }),
        (
            "[a-z][a-z_]{2,8}".prop_map(|s: String| s),
            "[a-z]{2,6}".prop_map(|s: String| s),
            arb_safe(1, 10),
        )
            .prop_map(|(name, attr_key, attr_val)| Child::Untyped {
                name,
                attr_key,
                attr_val,
            }),
    ]
    .boxed()
}

fn arb_iso8601() -> BoxedStrategy<String> {
    (1_577_836_800_000i64..1_893_456_000_000i64)
        .prop_map(|ms| {
            jiff::Timestamp::from_millisecond(ms)
                .unwrap()
                .strftime("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
        })
        .boxed()
}

fn render_cot(
    uid: &str,
    kind: &str,
    how: &str,
    time: &str,
    start: &str,
    stale: &str,
    lat: f64,
    lon: f64,
    hae: f64,
    ce: f64,
    le: f64,
    children: &[Child],
) -> String {
    let mut s = String::with_capacity(1024);
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str(&format!(
        r#"<event uid="{uid}" type="{kind}" time="{time}" start="{start}" stale="{stale}" how="{how}">"#
    ));
    s.push('\n');
    s.push_str(&format!(
        r#"  <point lat="{lat}" lon="{lon}" hae="{hae}" ce="{ce}" le="{le}"/>"#
    ));
    s.push('\n');
    s.push_str("  <detail>\n");
    for child in children {
        s.push_str("    ");
        s.push_str(&render_child(child));
        s.push('\n');
    }
    s.push_str("  </detail>\n");
    s.push_str("</event>\n");
    s
}

/// The core property: round-trip through TakMessage is stable.
fn check_round_trip(
    uid: String,
    kind: String,
    how: String,
    time: String,
    start: String,
    stale: String,
    lat: f64,
    lon: f64,
    hae: f64,
    ce: f64,
    le: f64,
    children: Vec<Child>,
) -> Result<(), TestCaseError> {
    let xml1 = render_cot(
        &uid, &kind, &how, &time, &start, &stale, lat, lon, hae, ce, le, &children,
    );

    let view1 = decode_xml(&xml1).map_err(|e| TestCaseError::reject(format!("decode1: {e}")))?;
    let msg1 =
        view_to_takmessage(&view1).map_err(|e| TestCaseError::reject(format!("convert1: {e}")))?;
    let xml2 = takmessage_to_xml_string(&msg1)
        .map_err(|e| TestCaseError::reject(format!("encode: {e}")))?;
    let view2 = decode_xml(&xml2)
        .map_err(|e| TestCaseError::fail(format!("re-decode failed:\n{xml2}\nerror: {e}")))?;
    let msg2 = view_to_takmessage(&view2)
        .map_err(|e| TestCaseError::fail(format!("convert2 failed:\n{xml2}\nerror: {e}")))?;

    prop_assert_eq!(
        &msg1,
        &msg2,
        "TakMessage drifted across round-trip. xml1:\n{}\nxml2:\n{}",
        xml1,
        xml2
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Quick round-trip property test — runs in pre-push.
    #[test]
    fn cot_round_trip_stable_quick(
        uid in "[A-Za-z0-9_-]{5,30}",
        kind in "[a-z]-[a-zA-Z]{1,5}(-[A-Za-z]{1,4}){0,3}",
        how in "[a-z](-[a-z]){0,3}",
        time in arb_iso8601(),
        start in arb_iso8601(),
        stale in arb_iso8601(),
        lat in -90.0f64..=90.0,
        lon in -180.0f64..=180.0,
        hae in -1000.0f64..=10000.0,
        ce in 0.0f64..=10000.0,
        le in 0.0f64..=10000.0,
        children in proptest::collection::vec(arb_child(), 0..6),
    ) {
        check_round_trip(uid, kind, how, time, start, stale, lat, lon, hae, ce, le, children)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// Extended round-trip — 10k cases. Run via `cargo test -- --ignored`
    /// or `/check-invariants`. Skipped in pre-push.
    #[test]
    #[ignore = "slow; runs via /check-invariants"]
    fn cot_round_trip_stable_extended(
        uid in "[A-Za-z0-9_-]{5,30}",
        kind in "[a-z]-[a-zA-Z]{1,5}(-[A-Za-z]{1,4}){0,3}",
        how in "[a-z](-[a-z]){0,3}",
        time in arb_iso8601(),
        start in arb_iso8601(),
        stale in arb_iso8601(),
        lat in -90.0f64..=90.0,
        lon in -180.0f64..=180.0,
        hae in -1000.0f64..=10000.0,
        ce in 0.0f64..=10000.0,
        le in 0.0f64..=10000.0,
        children in proptest::collection::vec(arb_child(), 0..6),
    ) {
        check_round_trip(uid, kind, how, time, start, stale, lat, lon, hae, ce, le, children)?;
    }
}
