//! Tiny Prometheus text-format parser, scoped to what we need.
//!
//! We don't pull a full prom client crate because we read three
//! counters and ignore everything else; a 30-line parser is the
//! right shape.

use std::collections::HashMap;

/// Parse the prom text format and return a map of
/// `metric_name -> last numeric value`. Lines starting with `#`
/// are skipped (HELP / TYPE comments). Histogram + summary
/// composite types collapse to whatever the last bucket / sum
/// line says — fine for our counter use case.
#[must_use]
pub fn parse_text(body: &str) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // metric_name{labels} value [timestamp]
        // We don't care about labels here — the counters we read
        // are unlabeled. Strip a `{...}` block if present.
        let line = if let Some(brace) = line.find('{') {
            // Find the matching `}` and skip past it.
            if let Some(end) = line[brace..].find('}') {
                let mut s = String::with_capacity(line.len());
                s.push_str(&line[..brace]);
                s.push_str(&line[brace + end + 1..]);
                s
            } else {
                continue; // malformed; skip
            }
        } else {
            line.to_owned()
        };

        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(value_str) = parts.next() else {
            continue;
        };
        let Ok(value) = value_str.parse::<f64>() else {
            continue;
        };
        out.insert(name.to_owned(), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unlabeled_counters() {
        let raw = r#"
# HELP foo a thing
# TYPE foo counter
foo 42
tak_bus_delivered 1234
tak_bus_dropped_full 7
        "#;
        let m = parse_text(raw);
        assert_eq!(m.get("foo"), Some(&42.0));
        assert_eq!(m.get("tak_bus_delivered"), Some(&1234.0));
        assert_eq!(m.get("tak_bus_dropped_full"), Some(&7.0));
    }

    #[test]
    fn strips_labels() {
        let raw = r#"
# TYPE labelled gauge
labelled{instance="x",job="y"} 99.5
        "#;
        let m = parse_text(raw);
        assert_eq!(m.get("labelled"), Some(&99.5));
    }

    #[test]
    fn ignores_malformed() {
        let raw = "broken {no closing\nstill_works 1";
        let m = parse_text(raw);
        assert_eq!(m.get("still_works"), Some(&1.0));
        assert_eq!(m.get("broken"), None);
    }
}
