//! Fuzz the TAK Protocol v1 streaming framing decoder.
//!
//! Goal: any byte sequence (junk, truncated frame, malicious varint)
//! must yield Ok((total, payload)) or Err — never panic, never
//! infinite-loop, never read past the input slice.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tak_cot::framing::decode_stream;

fuzz_target!(|data: &[u8]| {
    let _ = decode_stream(data);
});
