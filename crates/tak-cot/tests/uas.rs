#![cfg(feature = "uas")]
//! The `uas` feature turns a Remote ID broadcast into a correctly typed CoT
//! aircraft event, which tak-cot's own decoder then round-trips. Before this
//! feature there was no fixture or code path for a drone event; the existing
//! fixtures cover only PLI, chat, geofence, route and drawing.

use tak_cot::uas::remote_id_to_cot;
use tak_cot::uas::remoteid::{BasicId, IdType, Location, Message, UaType, UasData};
use tak_cot::xml::decode_xml;

#[test]
fn remote_id_becomes_typed_uas_cot_event() {
    let mut uas = UasData::default();
    let id = BasicId::new(
        IdType::SerialNumber,
        UaType::HelicopterOrMultirotor,
        "1596F35074529ABCDEFG",
    )
    .unwrap();
    uas.update(&Message::BasicId(id));
    let mut loc = Location::default();
    loc.latitude = 45.5470818;
    loc.longitude = -122.9668346;
    loc.altitude_geo = Some(237.0);
    uas.update(&Message::Location(loc));

    let events = remote_id_to_cot(&uas, None, 1_700_000_000_000_000);
    assert!(
        !events.is_empty(),
        "a located aircraft must produce a CoT event"
    );

    let view = decode_xml(&events[0]).expect("uas-cot output decodes with tak-cot's own codec");
    assert_eq!(view.event.kind, "a-u-A-M-F-Q");
    assert_eq!(view.event.uid, "rid-1596F35074529ABCDEFG");
    let point = view.point.expect("aircraft event carries a point");
    assert_eq!(point.hae, "237.0");
}
