//! `ReportData` → the self-contained page. Task 9 replaces this body with the
//! real template; the contract (`render(&ReportData) -> String`) is final.

use crate::report::ReportData;

pub fn render(data: &ReportData) -> String {
    let json = serde_json::to_string(&crate::render::to_json(data)).expect("report data serializes");
    format!("<!doctype html><script id=\"keel-data\" type=\"application/json\">{}</script>", json.replace("</", "<\\/"))
}
