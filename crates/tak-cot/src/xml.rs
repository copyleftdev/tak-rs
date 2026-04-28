//! Borrowed-mode CoT XML decoder.
//!
//! `decode_xml` walks the input via `quick-xml`'s streaming reader for
//! tokenization and byte-position tracking, then extracts element names and
//! attribute values directly from the input slice. Every `&str` field on
//! [`CotEventView`] borrows from the original input — no `String::from`, no
//! DOM allocation. Invariant **H2** holds.
//!
//! Why bypass quick-xml's attribute iterator: its public API binds attribute
//! `Cow` values to `&self` rather than to the underlying `'a` of the
//! `BytesStart`, so values can't be returned with the input's lifetime
//! through that API. We use quick-xml for what it's best at (correct
//! tokenization, comment/CDATA handling, position tracking) and walk the
//! attribute slice ourselves — about 30 lines of straight-line bytestring
//! parsing, no entities.
//!
//! Entity-decoded attribute values are rejected with
//! [`Error::EntityNotSupported`]. CoT in production never uses entities.

use crate::{Error, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use smallvec::SmallVec;

/// A borrowed view over a parsed CoT event.
///
/// All `&str` fields are slices into the original input; the view itself owns
/// only a small inline vector of detail-child pointers.
#[derive(Debug, Clone, Default)]
pub struct CotEventView<'a> {
    /// Attributes from the `<event>` element.
    pub event: EventAttrs<'a>,
    /// `<point>` element, if present.
    pub point: Option<PointAttrs<'a>>,
    /// `<detail>` element children + the verbatim inner XML for round-trip.
    pub detail: DetailView<'a>,
}

/// Attributes from the `<event>` element.
///
/// Required (per CoT spec): `uid`, `type`, `time`, `start`, `stale`, `how`.
/// Remaining are optional under MIL-STD-6090 / TAK extensions.
#[derive(Debug, Clone, Default)]
pub struct EventAttrs<'a> {
    /// CoT version string (`"2.0"` typically).
    pub version: Option<&'a str>,
    /// Event UID. Required.
    pub uid: &'a str,
    /// CoT type code (e.g. `"a-f-G-U-C"`). XML attribute is named `type` but
    /// `type` is reserved in Rust; we use `kind` here.
    pub kind: &'a str,
    /// Send time (ISO-8601 UTC). Required.
    pub time: &'a str,
    /// Validity start (ISO-8601 UTC). Required.
    pub start: &'a str,
    /// Stale time (ISO-8601 UTC). After this, the event is expired.
    pub stale: &'a str,
    /// How the position was produced (e.g. `"m-g"` machine-derived GPS).
    pub how: &'a str,
    /// Access classification (optional under MIL-STD-6090).
    pub access: Option<&'a str>,
    /// Quality-of-service hint.
    pub qos: Option<&'a str>,
    /// Operational/exercise marker.
    pub opex: Option<&'a str>,
    /// Caveat string.
    pub caveat: Option<&'a str>,
    /// Releaseable-to designator.
    pub releaseable_to: Option<&'a str>,
}

/// Attributes from the `<point>` element. All required when point is present.
#[derive(Debug, Clone, Copy, Default)]
pub struct PointAttrs<'a> {
    /// Latitude (WGS-84, decimal degrees). String form preserves source precision.
    pub lat: &'a str,
    /// Longitude (WGS-84, decimal degrees).
    pub lon: &'a str,
    /// Height above ellipsoid (meters). `999999` indicates unknown.
    pub hae: &'a str,
    /// Circular error 1-sigma (meters). `999999` indicates unknown.
    pub ce: &'a str,
    /// Linear error 1-sigma (meters). `999999` indicates unknown.
    pub le: &'a str,
}

/// View over the `<detail>` element.
#[derive(Debug, Clone, Default)]
pub struct DetailView<'a> {
    /// Verbatim XML between `<detail>` and `</detail>` (exclusive of the tags
    /// themselves), trimmed of surrounding whitespace. Suitable for
    /// `Detail.xmlDetail` after stripping typed sub-elements.
    pub raw: &'a str,
    /// Top-level children of `<detail>`, in document order.
    pub children: SmallVec<[DetailChild<'a>; 8]>,
}

/// A single immediate child element of `<detail>`.
#[derive(Debug, Clone, Copy)]
pub struct DetailChild<'a> {
    /// Element local name (e.g. `"contact"`, `"__group"`, `"link"`).
    pub name: &'a str,
    /// Verbatim XML of this element, including its own start/end tags.
    pub raw: &'a str,
}

/// Decode a CoT XML document into a borrowed view.
///
/// # Errors
///
/// - [`Error::Xml`] on malformed XML or non-UTF-8 input.
/// - [`Error::EntityNotSupported`] if an attribute value contains an XML entity reference.
/// - [`Error::MissingEventAttr`] if a required `<event>` attribute is missing.
/// - [`Error::MissingPointAttr`] if a required `<point>` attribute is missing.
pub fn decode_xml(input: &str) -> Result<CotEventView<'_>> {
    let mut reader = Reader::from_str(input);
    let cfg = reader.config_mut();
    cfg.trim_text(false);
    cfg.expand_empty_elements = false;

    let mut view = CotEventView::default();
    let mut depth = 0u32;
    let mut in_detail = false;
    let mut detail_inner_start = 0usize;
    let mut child_start: Option<usize> = None;

    loop {
        let pos_before = position(&reader);
        let event = reader
            .read_event()
            .map_err(|err| Error::Xml(err.to_string()))?;
        let pos_after = position(&reader);

        match event {
            Event::Eof => break,
            Event::Start(_) => {
                depth = depth.saturating_add(1);
                let name = element_name(input, pos_before)?;
                let header = &input[pos_before..pos_after];
                match (depth, name) {
                    (1, "event") => parse_event_attrs(header, &mut view.event)?,
                    (2, "detail") => {
                        in_detail = true;
                        detail_inner_start = pos_after;
                    }
                    (3, _) if in_detail => child_start = Some(pos_before),
                    _ => {}
                }
            }
            Event::Empty(_) => {
                let name = element_name(input, pos_before)?;
                let header = &input[pos_before..pos_after];
                let virtual_depth = depth.saturating_add(1);
                match (virtual_depth, name) {
                    (1, "event") => parse_event_attrs(header, &mut view.event)?,
                    (2, "point") => view.point = Some(parse_point_attrs(header)?),
                    (3, _) if in_detail => view.detail.children.push(DetailChild {
                        name,
                        raw: &input[pos_before..pos_after],
                    }),
                    _ => {}
                }
            }
            Event::End(_) => {
                if depth == 3 && in_detail {
                    if let Some(start) = child_start.take() {
                        let name = element_name(input, start)?;
                        view.detail.children.push(DetailChild {
                            name,
                            raw: &input[start..pos_after],
                        });
                    }
                } else if depth == 2 && in_detail {
                    view.detail.raw = input[detail_inner_start..pos_before].trim_matches(is_ws);
                    in_detail = false;
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    if view.event.uid.is_empty() {
        return Err(Error::MissingEventAttr("uid"));
    }
    if view.event.kind.is_empty() {
        return Err(Error::MissingEventAttr("type"));
    }
    Ok(view)
}

#[inline]
#[allow(clippy::cast_possible_truncation)]
fn position<R>(reader: &Reader<R>) -> usize {
    reader.buffer_position() as usize
}

#[inline]
fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

#[inline]
fn is_name_break(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
}

/// Extract the local element name starting at the `<` byte at `lt_pos`.
/// Handles both start tags (`<name ...>`, `<name/>`) and end tags (`</name>`).
fn element_name(input: &str, lt_pos: usize) -> Result<&str> {
    let bytes = input.as_bytes();
    if bytes.get(lt_pos) != Some(&b'<') {
        return Err(Error::Xml(format!("expected '<' at byte {lt_pos}")));
    }
    let start = if bytes.get(lt_pos.saturating_add(1)) == Some(&b'/') {
        lt_pos.saturating_add(2)
    } else {
        lt_pos.saturating_add(1)
    };
    let mut end = start;
    while end < bytes.len() && !is_name_break(bytes[end]) {
        end = end.saturating_add(1);
    }
    Ok(&input[start..end])
}

fn parse_event_attrs<'a>(header: &'a str, out: &mut EventAttrs<'a>) -> Result<()> {
    walk_attrs(header, |key, value| {
        match key.as_bytes() {
            b"version" => out.version = Some(value),
            b"uid" => out.uid = value,
            b"type" => out.kind = value,
            b"time" => out.time = value,
            b"start" => out.start = value,
            b"stale" => out.stale = value,
            b"how" => out.how = value,
            b"access" => out.access = Some(value),
            b"qos" => out.qos = Some(value),
            b"opex" => out.opex = Some(value),
            b"caveat" => out.caveat = Some(value),
            b"releaseableTo" => out.releaseable_to = Some(value),
            _ => {}
        }
        Ok(())
    })
}

fn parse_point_attrs(header: &str) -> Result<PointAttrs<'_>> {
    let mut out = PointAttrs::default();
    walk_attrs(header, |key, value| {
        match key.as_bytes() {
            b"lat" => out.lat = value,
            b"lon" => out.lon = value,
            b"hae" => out.hae = value,
            b"ce" => out.ce = value,
            b"le" => out.le = value,
            _ => {}
        }
        Ok(())
    })?;
    if out.lat.is_empty() {
        return Err(Error::MissingPointAttr("lat"));
    }
    if out.lon.is_empty() {
        return Err(Error::MissingPointAttr("lon"));
    }
    Ok(out)
}

/// Walk `attr1="val1" attr2="val2"` style attributes from a `<name attrs ...>`
/// element-header slice. Yields each (key, value) pair as `&'a str` slices.
/// Returns [`Error::EntityNotSupported`] if any value contains `&`.
fn walk_attrs<'a, F>(header: &'a str, mut f: F) -> Result<()>
where
    F: FnMut(&'a str, &'a str) -> Result<()>,
{
    let bytes = header.as_bytes();
    let mut i = 0usize;

    // Skip the leading `<` and the element name.
    if bytes.first() == Some(&b'<') {
        i = 1;
    }
    while i < bytes.len() && !is_name_break(bytes[i]) {
        i = i.saturating_add(1);
    }

    loop {
        // Skip whitespace.
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
            i = i.saturating_add(1);
        }
        if i >= bytes.len() || matches!(bytes[i], b'>' | b'/') {
            break;
        }

        // Read key.
        let key_start = i;
        while i < bytes.len()
            && !matches!(bytes[i], b'=' | b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
        {
            i = i.saturating_add(1);
        }
        if i == key_start {
            break;
        }
        let key = &header[key_start..i];

        // Skip `=` and surrounding whitespace.
        while i < bytes.len() && matches!(bytes[i], b'=' | b' ' | b'\t' | b'\n' | b'\r') {
            i = i.saturating_add(1);
        }
        if i >= bytes.len() {
            break;
        }

        // Quoted value.
        let quote = bytes[i];
        if !matches!(quote, b'"' | b'\'') {
            return Err(Error::Xml("attribute value must be quoted".into()));
        }
        i = i.saturating_add(1);
        let val_start = i;
        while i < bytes.len() && bytes[i] != quote {
            if bytes[i] == b'&' {
                return Err(Error::EntityNotSupported);
            }
            i = i.saturating_add(1);
        }
        let value = &header[val_start..i];
        if i < bytes.len() {
            i = i.saturating_add(1); // consume closing quote
        }

        f(key, value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const PLI: &str = include_str!("../tests/fixtures/01_pli.xml");

    #[test]
    fn pli_event_attrs_borrow_into_input() {
        let view = decode_xml(PLI).expect("decode pli");
        assert_eq!(view.event.uid, "ANDROID-deadbeef0001");
        assert_eq!(view.event.kind, "a-f-G-U-C");
        assert_eq!(view.event.how, "m-g");
        assert_eq!(view.event.version, Some("2.0"));

        // Pointer math: the uid slice must lie within PLI.
        assert!(slice_within(view.event.uid, PLI));
        assert!(slice_within(view.event.kind, PLI));
    }

    #[test]
    fn pli_point_attrs() {
        let view = decode_xml(PLI).unwrap();
        let p = view.point.expect("point present");
        assert_eq!(p.lat, "34.225700");
        assert_eq!(p.lon, "-118.573900");
        assert_eq!(p.hae, "245.000000");
    }

    #[test]
    fn pli_detail_children_in_document_order() {
        let view = decode_xml(PLI).unwrap();
        let names: Vec<&str> = view.detail.children.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "takv",
                "contact",
                "uid",
                "__group",
                "status",
                "track",
                "precisionlocation",
            ]
        );
        for child in &view.detail.children {
            assert!(slice_within(child.name, PLI));
            assert!(slice_within(child.raw, PLI));
        }
    }

    fn slice_within(s: &str, base: &str) -> bool {
        let s_start = s.as_ptr() as usize;
        let base_start = base.as_ptr() as usize;
        let base_end = base_start + base.len();
        s_start >= base_start && s_start.saturating_add(s.len()) <= base_end
    }
}
