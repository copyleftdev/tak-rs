//! XML ↔ TAK Protocol v1 protobuf bridges.
//!
//! [`view_to_takmessage`] takes a borrowed [`CotEventView`] (output of
//! [`crate::xml::decode_xml`]) and produces an owned `TakMessage` protobuf.
//!
//! Per upstream rule (`detail.proto`):
//!
//! > if [a typed sub-message's fields] are missing on send, the conversion
//! > to the message format will be rejected and fall back to opaque XML
//! > representation
//!
//! So a `<contact callsign="X"/>` element with no `endpoint` does NOT
//! populate `Detail.contact` — it ends up verbatim in `Detail.xmlDetail`.
//! The same logic applies to all six typed sub-messages.
//!
//! Unconsumed detail children are concatenated (newline-separated) into
//! `Detail.xmlDetail`. This gives the receiver back the same well-formed
//! XML fragments, in document order; whitespace between children is
//! normalized to a single newline (CoT XML is whitespace-insensitive
//! between elements, so this is semantically lossless).

use crate::xml::{CotEventView, DetailChild, DetailView};
use crate::{Error, Result, xml};
use tak_proto::v1::{
    Contact, CotEvent, Detail, Group, PrecisionLocation, Status, TakMessage, Takv, Track,
};

/// Convert a borrowed CoT view into an owned `TakMessage`.
///
/// `submission_time` and `creation_time` are left at `0` — they are
/// server-stamped fields that the messaging layer fills in, not the codec.
///
/// # Errors
///
/// - [`Error::Xml`] if a required time attribute fails to parse as
///   ISO-8601 / RFC 3339.
/// - [`Error::MissingEventAttr`] if a required event attribute is empty.
pub fn view_to_takmessage(view: &CotEventView<'_>) -> Result<TakMessage> {
    if view.event.uid.is_empty() {
        return Err(Error::MissingEventAttr("uid"));
    }
    if view.event.kind.is_empty() {
        return Err(Error::MissingEventAttr("type"));
    }

    let send_time = parse_iso8601_ms(view.event.time)?;
    let start_time = parse_iso8601_ms(view.event.start)?;
    let stale_time = parse_iso8601_ms(view.event.stale)?;

    let (lat, lon, hae, ce, le) = view.point.as_ref().map_or((0.0, 0.0, 0.0, 0.0, 0.0), |p| {
        (
            parse_f64(p.lat),
            parse_f64(p.lon),
            parse_f64(p.hae),
            parse_f64(p.ce),
            parse_f64(p.le),
        )
    });

    let cot_event = CotEvent {
        r#type: view.event.kind.to_owned(),
        access: view.event.access.unwrap_or_default().to_owned(),
        qos: view.event.qos.unwrap_or_default().to_owned(),
        opex: view.event.opex.unwrap_or_default().to_owned(),
        caveat: view.event.caveat.unwrap_or_default().to_owned(),
        releaseable_to: view.event.releaseable_to.unwrap_or_default().to_owned(),
        uid: view.event.uid.to_owned(),
        send_time,
        start_time,
        stale_time,
        how: view.event.how.to_owned(),
        lat,
        lon,
        hae,
        ce,
        le,
        detail: Some(build_detail(&view.detail)?),
    };

    Ok(TakMessage {
        tak_control: None,
        cot_event: Some(cot_event),
        submission_time: 0,
        creation_time: 0,
    })
}

/// Build a [`Detail`] proto from the view, populating typed sub-messages
/// where required fields are present and concatenating the rest into
/// `xmlDetail`.
fn build_detail(view: &DetailView<'_>) -> Result<Detail> {
    let mut detail = Detail::default();
    let mut consumed = vec![false; view.children.len()];

    for (i, child) in view.children.iter().enumerate() {
        match child.name {
            "contact" if detail.contact.is_none() => {
                if let Some(c) = try_contact(child)? {
                    detail.contact = Some(c);
                    consumed[i] = true;
                }
            }
            "__group" if detail.group.is_none() => {
                if let Some(g) = try_group(child)? {
                    detail.group = Some(g);
                    consumed[i] = true;
                }
            }
            "precisionlocation" if detail.precision_location.is_none() => {
                if let Some(p) = try_precision_location(child)? {
                    detail.precision_location = Some(p);
                    consumed[i] = true;
                }
            }
            "status" if detail.status.is_none() => {
                if let Some(s) = try_status(child)? {
                    detail.status = Some(s);
                    consumed[i] = true;
                }
            }
            "takv" if detail.takv.is_none() => {
                if let Some(t) = try_takv(child)? {
                    detail.takv = Some(t);
                    consumed[i] = true;
                }
            }
            "track" if detail.track.is_none() => {
                if let Some(t) = try_track(child)? {
                    detail.track = Some(t);
                    consumed[i] = true;
                }
            }
            _ => {}
        }
    }

    let mut xml_detail = String::new();
    for (i, child) in view.children.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        if !xml_detail.is_empty() {
            xml_detail.push('\n');
        }
        xml_detail.push_str(child.raw);
    }
    detail.xml_detail = xml_detail;

    Ok(detail)
}

// ---------------------------------------------------------------------------
// Per-element converters. Each returns `Ok(None)` when a required attribute
// is missing — the upstream "fall back to opaque XML" rule.
// ---------------------------------------------------------------------------

fn try_contact(child: &DetailChild<'_>) -> Result<Option<Contact>> {
    let mut endpoint: Option<String> = None;
    let mut callsign: Option<String> = None;
    walk_child_attrs(child, |k, v| {
        match k {
            "endpoint" => endpoint = Some(v.to_owned()),
            "callsign" => callsign = Some(v.to_owned()),
            _ => {}
        }
        Ok(())
    })?;
    Ok(match (endpoint, callsign) {
        (Some(e), Some(c)) => Some(Contact {
            endpoint: e,
            callsign: c,
        }),
        _ => None,
    })
}

fn try_group(child: &DetailChild<'_>) -> Result<Option<Group>> {
    let mut name: Option<String> = None;
    let mut role: Option<String> = None;
    walk_child_attrs(child, |k, v| {
        match k {
            "name" => name = Some(v.to_owned()),
            "role" => role = Some(v.to_owned()),
            _ => {}
        }
        Ok(())
    })?;
    Ok(match (name, role) {
        (Some(n), Some(r)) => Some(Group { name: n, role: r }),
        _ => None,
    })
}

fn try_precision_location(child: &DetailChild<'_>) -> Result<Option<PrecisionLocation>> {
    let mut geopointsrc: Option<String> = None;
    let mut altsrc: Option<String> = None;
    walk_child_attrs(child, |k, v| {
        match k {
            "geopointsrc" => geopointsrc = Some(v.to_owned()),
            "altsrc" => altsrc = Some(v.to_owned()),
            _ => {}
        }
        Ok(())
    })?;
    Ok(match (geopointsrc, altsrc) {
        (Some(g), Some(a)) => Some(PrecisionLocation {
            geopointsrc: g,
            altsrc: a,
        }),
        _ => None,
    })
}

fn try_status(child: &DetailChild<'_>) -> Result<Option<Status>> {
    let mut battery: Option<u32> = None;
    walk_child_attrs(child, |k, v| {
        if k == "battery" {
            battery = v.parse::<u32>().ok();
        }
        Ok(())
    })?;
    Ok(battery.map(|b| Status { battery: b }))
}

fn try_takv(child: &DetailChild<'_>) -> Result<Option<Takv>> {
    let mut device: Option<String> = None;
    let mut platform: Option<String> = None;
    let mut os: Option<String> = None;
    let mut version: Option<String> = None;
    walk_child_attrs(child, |k, v| {
        match k {
            "device" => device = Some(v.to_owned()),
            "platform" => platform = Some(v.to_owned()),
            "os" => os = Some(v.to_owned()),
            "version" => version = Some(v.to_owned()),
            _ => {}
        }
        Ok(())
    })?;
    Ok(match (device, platform, os, version) {
        (Some(d), Some(p), Some(o), Some(v)) => Some(Takv {
            device: d,
            platform: p,
            os: o,
            version: v,
        }),
        _ => None,
    })
}

fn try_track(child: &DetailChild<'_>) -> Result<Option<Track>> {
    let mut speed: Option<f64> = None;
    let mut course: Option<f64> = None;
    walk_child_attrs(child, |k, v| {
        match k {
            "speed" => speed = v.parse::<f64>().ok(),
            "course" => course = v.parse::<f64>().ok(),
            _ => {}
        }
        Ok(())
    })?;
    Ok(match (speed, course) {
        (Some(s), Some(c)) => Some(Track {
            speed: s,
            course: c,
        }),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walk attributes of a detail child by isolating the element header
/// (`<name attrs ...>` or `<name attrs/>`) and reusing [`xml::walk_attrs`].
fn walk_child_attrs<F>(child: &DetailChild<'_>, f: F) -> Result<()>
where
    F: FnMut(&str, &str) -> Result<()>,
{
    let header = header_of(child.raw);
    xml::walk_attrs(header, f)
}

fn header_of(raw: &str) -> &str {
    raw.find('>').map_or(raw, |i| &raw[..=i])
}

/// Parse an ISO-8601 / RFC 3339 timestamp into milliseconds since the epoch.
fn parse_iso8601_ms(s: &str) -> Result<u64> {
    let ts: jiff::Timestamp = s
        .parse()
        .map_err(|e: jiff::Error| Error::Xml(format!("timestamp `{s}`: {e}")))?;
    let ms = ts.as_millisecond();
    u64::try_from(ms).map_err(|_| Error::Xml(format!("timestamp `{s}` predates epoch")))
}

/// Parse a string to f64; on parse failure returns 0.0 (matches upstream's
/// behavior of treating malformed numerics as missing data).
fn parse_f64(s: &str) -> f64 {
    s.parse().unwrap_or(0.0)
}
