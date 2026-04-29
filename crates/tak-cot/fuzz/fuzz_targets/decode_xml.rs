//! Fuzz the CoT XML decoder.
//!
//! Inputs that aren't valid UTF-8 short-circuit (`decode_xml` takes
//! `&str`, so the boundary check is the language, not the harness).
//! Anything past that should either decode to a `CotEventView` or
//! return an `Err` — but never panic, never read past `input.len()`,
//! never reach `unreachable!()`.
//!
//! Seed corpus: the canonical fixtures under
//! `crates/tak-cot/tests/fixtures/`. Drop additional captures into
//! `corpus/decode_xml/` to extend coverage.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tak_cot::xml::decode_xml;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let _ = decode_xml(s);
});
