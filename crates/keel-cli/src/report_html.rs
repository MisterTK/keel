//! `ReportData` → the self-contained page: `assets/report/report.html` with
//! the stylesheet, script, and JSON blob inlined at build time
//! (`include_str!`), so the file works from `file://`, can be moved or
//! attached to a ticket, and — per its CSP meta — never loads anything
//! external. The blob is sorted-key JSON (`render::to_json`), so under a
//! fixed `now_ms` the page bytes are a pure function of the evidence.

use crate::render::to_json;
use crate::report::ReportData;

const TEMPLATE: &str = include_str!("../assets/report/report.html");
const CSS: &str = include_str!("../assets/report/report.css");
const JS: &str = include_str!("../assets/report/report.js");

/// The whole page as a string. Every `<`, `>`, and `&` in the serialized JSON
/// is escaped to its `\uXXXX` form (all three are legal JSON escapes, so
/// `JSON.parse` round-trips the original values byte-for-byte). Escaping only
/// `</` is not enough: a value containing `<!--<script` drives the HTML5
/// tokenizer into the "script data double escaped" state, which then treats
/// the template's own real `</script>` closing tag as an internal state
/// transition rather than an end tag — silently swallowing the rest of the
/// document (the page's own `<script>` block included) as inert data, so the
/// page renders blank. Escaping every `<` (not just `</`) keeps the tokenizer
/// out of that state entirely, and escaping `>`/`&` too is the standard
/// belt-and-suspenders for JSON embedded in HTML. Data is substituted last, so
/// nothing inside it is ever re-expanded.
pub fn render(data: &ReportData) -> String {
    let json = serde_json::to_string(&to_json(data))
        .expect("report data serializes")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    TEMPLATE
        .replacen("{{CSS}}", CSS, 1)
        .replacen("{{JS}}", JS, 1)
        .replacen("{{DATA}}", &json, 1)
}
