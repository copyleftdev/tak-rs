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
use std::io::{self, Write};
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
// Per-element converters. Each returns `Ok(None)` when:
// - a required attribute is missing, OR
// - the element carries any attribute we don't recognize for the typed
//   sub-message (per upstream `detail.proto`: "WHOLE ELEMENTS MUST BE
//   CONVERTED TO MESSAGES" — splitting attrs across typed + xmlDetail
//   is forbidden).
// In either case the whole element falls through verbatim into xmlDetail.
// ---------------------------------------------------------------------------

fn try_contact(child: &DetailChild<'_>) -> Result<Option<Contact>> {
    let mut endpoint: Option<String> = None;
    let mut callsign: Option<String> = None;
    let mut has_extra = false;
    walk_child_attrs(child, |k, v| {
        match k {
            "endpoint" => endpoint = Some(v.to_owned()),
            "callsign" => callsign = Some(v.to_owned()),
            _ => has_extra = true,
        }
        Ok(())
    })?;
    if has_extra {
        return Ok(None);
    }
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
    let mut has_extra = false;
    walk_child_attrs(child, |k, v| {
        match k {
            "name" => name = Some(v.to_owned()),
            "role" => role = Some(v.to_owned()),
            _ => has_extra = true,
        }
        Ok(())
    })?;
    if has_extra {
        return Ok(None);
    }
    Ok(match (name, role) {
        (Some(n), Some(r)) => Some(Group { name: n, role: r }),
        _ => None,
    })
}

fn try_precision_location(child: &DetailChild<'_>) -> Result<Option<PrecisionLocation>> {
    let mut geopointsrc: Option<String> = None;
    let mut altsrc: Option<String> = None;
    let mut has_extra = false;
    walk_child_attrs(child, |k, v| {
        match k {
            "geopointsrc" => geopointsrc = Some(v.to_owned()),
            "altsrc" => altsrc = Some(v.to_owned()),
            _ => has_extra = true,
        }
        Ok(())
    })?;
    if has_extra {
        return Ok(None);
    }
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
    let mut has_extra = false;
    walk_child_attrs(child, |k, v| {
        if k == "battery" {
            battery = v.parse::<u32>().ok();
        } else {
            has_extra = true;
        }
        Ok(())
    })?;
    if has_extra {
        return Ok(None);
    }
    Ok(battery.map(|b| Status { battery: b }))
}

fn try_takv(child: &DetailChild<'_>) -> Result<Option<Takv>> {
    let mut device: Option<String> = None;
    let mut platform: Option<String> = None;
    let mut os: Option<String> = None;
    let mut version: Option<String> = None;
    let mut has_extra = false;
    walk_child_attrs(child, |k, v| {
        match k {
            "device" => device = Some(v.to_owned()),
            "platform" => platform = Some(v.to_owned()),
            "os" => os = Some(v.to_owned()),
            "version" => version = Some(v.to_owned()),
            _ => has_extra = true,
        }
        Ok(())
    })?;
    if has_extra {
        return Ok(None);
    }
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
    let mut has_extra = false;
    walk_child_attrs(child, |k, v| {
        match k {
            "speed" => speed = v.parse::<f64>().ok(),
            "course" => course = v.parse::<f64>().ok(),
            _ => has_extra = true,
        }
        Ok(())
    })?;
    if has_extra {
        return Ok(None);
    }
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

// ===========================================================================
// TakMessage → XML — symmetric inverse of `view_to_takmessage`.
// ===========================================================================

/// Encode a [`TakMessage`] as CoT XML, writing into `out`.
///
/// Typed sub-messages are emitted as their XML equivalents:
/// `<contact>`, `<__group>`, `<precisionlocation>`, `<status>`, `<takv>`,
/// `<track>`. After those, `Detail.xml_detail` is appended verbatim — the
/// caller is responsible for ensuring it is well-formed XML (which our own
/// `view_to_takmessage` guarantees by construction).
///
/// Round-trip property: `view_to_takmessage(decode_xml(s)) → takmessage_to_xml`
/// produces XML that decodes back to a TakMessage equal to the original
/// (modulo numeric formatting and the ordering of unconsumed children).
///
/// # Errors
///
/// - [`Error::Xml`] if `cot_event` is missing or a string field contains an
///   XML-special character (`<`, `>`, `&`, `"`, `'`).
/// - [`Error::Io`] if the writer fails.
pub fn takmessage_to_xml<W: Write>(msg: &TakMessage, out: &mut W) -> Result<()> {
    let cot = msg
        .cot_event
        .as_ref()
        .ok_or_else(|| Error::Xml("TakMessage missing cot_event".to_owned()))?;

    out.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n")?;

    // <event ...>
    out.write_all(b"<event")?;
    write_str_attr(out, "version", "2.0")?;
    write_str_attr(out, "uid", &cot.uid)?;
    write_str_attr(out, "type", &cot.r#type)?;
    write_str_attr(out, "time", &format_iso8601(cot.send_time)?)?;
    write_str_attr(out, "start", &format_iso8601(cot.start_time)?)?;
    write_str_attr(out, "stale", &format_iso8601(cot.stale_time)?)?;
    write_str_attr(out, "how", &cot.how)?;
    write_str_attr_opt(out, "access", &cot.access)?;
    write_str_attr_opt(out, "qos", &cot.qos)?;
    write_str_attr_opt(out, "opex", &cot.opex)?;
    write_str_attr_opt(out, "caveat", &cot.caveat)?;
    write_str_attr_opt(out, "releaseableTo", &cot.releaseable_to)?;
    out.write_all(b">\n")?;

    // <point .../>
    out.write_all(b"  <point")?;
    write_f64_attr(out, "lat", cot.lat)?;
    write_f64_attr(out, "lon", cot.lon)?;
    write_f64_attr(out, "hae", cot.hae)?;
    write_f64_attr(out, "ce", cot.ce)?;
    write_f64_attr(out, "le", cot.le)?;
    out.write_all(b"/>\n")?;

    // <detail>
    out.write_all(b"  <detail>\n")?;
    if let Some(d) = &cot.detail {
        write_typed_subs(out, d)?;
        if !d.xml_detail.is_empty() {
            out.write_all(b"    ")?;
            // xml_detail is markup, not an attribute value — emit verbatim.
            out.write_all(d.xml_detail.as_bytes())?;
            out.write_all(b"\n")?;
        }
    }
    out.write_all(b"  </detail>\n")?;
    out.write_all(b"</event>\n")?;
    Ok(())
}

/// Convenience: encode a TakMessage into a fresh `String`.
pub fn takmessage_to_xml_string(msg: &TakMessage) -> Result<String> {
    let mut buf = Vec::with_capacity(512);
    takmessage_to_xml(msg, &mut buf)?;
    String::from_utf8(buf).map_err(|e| Error::Xml(e.to_string()))
}

fn write_typed_subs<W: Write>(out: &mut W, d: &Detail) -> Result<()> {
    if let Some(t) = &d.takv {
        out.write_all(b"    <takv")?;
        write_str_attr(out, "device", &t.device)?;
        write_str_attr(out, "platform", &t.platform)?;
        write_str_attr(out, "os", &t.os)?;
        write_str_attr(out, "version", &t.version)?;
        out.write_all(b"/>\n")?;
    }
    if let Some(c) = &d.contact {
        out.write_all(b"    <contact")?;
        write_str_attr(out, "endpoint", &c.endpoint)?;
        write_str_attr(out, "callsign", &c.callsign)?;
        out.write_all(b"/>\n")?;
    }
    if let Some(g) = &d.group {
        out.write_all(b"    <__group")?;
        write_str_attr(out, "name", &g.name)?;
        write_str_attr(out, "role", &g.role)?;
        out.write_all(b"/>\n")?;
    }
    if let Some(s) = &d.status {
        out.write_all(b"    <status")?;
        write!(out, " battery=\"{}\"", s.battery)?;
        out.write_all(b"/>\n")?;
    }
    if let Some(t) = &d.track {
        out.write_all(b"    <track")?;
        write_f64_attr(out, "speed", t.speed)?;
        write_f64_attr(out, "course", t.course)?;
        out.write_all(b"/>\n")?;
    }
    if let Some(p) = &d.precision_location {
        out.write_all(b"    <precisionlocation")?;
        write_str_attr(out, "geopointsrc", &p.geopointsrc)?;
        write_str_attr(out, "altsrc", &p.altsrc)?;
        out.write_all(b"/>\n")?;
    }
    Ok(())
}

#[inline]
fn write_str_attr<W: Write>(out: &mut W, key: &str, value: &str) -> Result<()> {
    if let Some(c) = value
        .chars()
        .find(|c| matches!(c, '<' | '>' | '&' | '"' | '\''))
    {
        return Err(Error::SpecialCharInValue(c));
    }
    out.write_all(b" ")?;
    out.write_all(key.as_bytes())?;
    out.write_all(b"=\"")?;
    out.write_all(value.as_bytes())?;
    out.write_all(b"\"")?;
    Ok(())
}

/// Emit `key="value"` only when value is non-empty (proto3 default `String`).
#[inline]
fn write_str_attr_opt<W: Write>(out: &mut W, key: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        Ok(())
    } else {
        write_str_attr(out, key, value)
    }
}

#[inline]
fn write_f64_attr<W: Write>(out: &mut W, key: &str, value: f64) -> io::Result<()> {
    // Rust's default Display for f64 picks the shortest representation that
    // round-trips back to the same f64 (Grisu / Ryu). Good enough for CoT.
    write!(out, " {key}=\"{value}\"")
}

/// Format an ms-since-epoch `u64` as ISO-8601 / RFC 3339 with 3 decimal seconds.
fn format_iso8601(ms: u64) -> Result<String> {
    let signed = i64::try_from(ms).map_err(|_| Error::Xml("timestamp overflow".to_owned()))?;
    let ts = jiff::Timestamp::from_millisecond(signed)
        .map_err(|e| Error::Xml(format!("timestamp from_millisecond({ms}): {e}")))?;
    Ok(ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}
