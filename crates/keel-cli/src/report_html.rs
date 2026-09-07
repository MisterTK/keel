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

/// The whole page as a string. The blob's `</` is escaped as `<\/` (a legal
/// JSON escape) so a target or op containing `</script>` cannot end the data
/// element early. Data is substituted last, so nothing inside it is ever
/// re-expanded.
pub fn render(data: &ReportData) -> String {
    let json = serde_json::to_string(&to_json(data))
        .expect("report data serializes")
        .replace("</", "<\\/");
    TEMPLATE
        .replacen("{{CSS}}", CSS, 1)
        .replacen("{{JS}}", JS, 1)
        .replacen("{{DATA}}", &json, 1)
}
