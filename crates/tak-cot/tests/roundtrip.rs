//! Codec invariant tests.
//!
//! - **C1** lossless XML round-trip on `xmlDetail` — proptest stub; real test
//!   lands when XML parser is implemented (M0 milestone).
//! - **Framing constants** — hard-coded checks that the wire constants match
//!   the spec; protects against accidental edits.

#[test]
fn framing_constants_match_spec() {
    use tak_cot::framing::{MAGIC, MESH_HEADER, MULTICAST_GROUP, MULTICAST_PORT, ProtoVersion};

    assert_eq!(MAGIC, 0xBF);
    assert_eq!(MESH_HEADER, [0xBF, 0x01, 0xBF]);
    assert_eq!(MULTICAST_GROUP, "239.2.3.1");
    assert_eq!(MULTICAST_PORT, 6969);
    assert_eq!(ProtoVersion::Xml as u8, 0x00);
    assert_eq!(ProtoVersion::V1 as u8, 0x01);
}

#[test]
fn proto_default_roundtrips() {
    use prost::Message as _;
    use tak_proto::v1::TakMessage;

    let msg = TakMessage::default();
    let encoded = msg.encode_to_vec();
    let decoded = TakMessage::decode(&encoded[..]).expect("decode default");
    assert_eq!(decoded, msg);
}
