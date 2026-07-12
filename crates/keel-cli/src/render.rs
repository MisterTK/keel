//! Output rendering: the one place JSON is serialized, so every command's
//! `--json` twin is byte-deterministic the same way.
//!
//! Reports are always serialized *through* [`serde_json::Value`], whose map is a
//! `BTreeMap` (serde_json's default, no `preserve_order`) — so keys sort
//! alphabetically regardless of struct field order, and an agent can diff two
//! runs and see only real change (dx-spec §5, "determinism as courtesy").

use std::io::Write;

use serde::Serialize;

use crate::Rendered;

/// Serialize any report to canonical JSON: sorted keys, pretty-printed, one
/// trailing newline. Panics only on a report that cannot serialize, which is a
/// programming error (all report structs are plain `Serialize`).
pub fn to_json(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).expect("report value is serializable")
}

/// Render a [`serde_json::Value`] to the exact bytes the `--json` twin prints.
pub fn json_string(value: &serde_json::Value) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("json value serializes");
    s.push('\n');
    s
}

/// Print a [`Rendered`] result to the right stream and return its exit code.
/// `json` selects the machine twin; otherwise the human prose is printed.
/// Errors and diagnostics ([`Rendered::to_stderr`]) go to stderr regardless.
pub fn emit(result: &Rendered, json: bool) -> i32 {
    let text = if json {
        json_string(&result.json)
    } else {
        let mut t = result.human.clone();
        if !t.ends_with('\n') {
            t.push('\n');
        }
        t
    };
    if result.to_stderr {
        let _ = std::io::stderr().write_all(text.as_bytes());
    } else {
        let _ = std::io::stdout().write_all(text.as_bytes());
    }
    result.exit
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_keys_sort_alphabetically_regardless_of_input_order() {
        #[derive(Serialize)]
        struct Out {
            zebra: u8,
            alpha: u8,
            mango: u8,
        }
        let v = to_json(&Out {
            zebra: 1,
            alpha: 2,
            mango: 3,
        });
        assert_eq!(
            json_string(&v),
            "{\n  \"alpha\": 2,\n  \"mango\": 3,\n  \"zebra\": 1\n}\n"
        );
    }

    #[test]
    fn nested_maps_also_sort() {
        let v = json!({ "b": { "y": 1, "x": 2 }, "a": 3 });
        // Round-trip through to_value to normalize (json! preserves order here).
        let v = to_json(&v);
        assert_eq!(
            json_string(&v),
            "{\n  \"a\": 3,\n  \"b\": {\n    \"x\": 2,\n    \"y\": 1\n  }\n}\n"
        );
    }
}
