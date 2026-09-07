//! `keel report` — blob assembly, static export, and the serving modes
//! (design spec 2026-09-04, Part B/C). Goldens are regenerated with
//! `KEEL_UPDATE_GOLDEN=1 cargo test -p keelrun-cli --test report`.

use std::path::{Path, PathBuf};

use keel_cli::render::{json_string, to_json};
use keel_cli::report::{self, Mode, ReportOptions};
use keel_journal::{DiscoveryStore, ManualClock, TargetStats};

/// The fixed clock every golden is rendered against (same instant as cli.rs).
const T0: i64 = 1_783_728_000_000;

const JOURNAL_SCHEMA: &str = include_str!("../../../contracts/journal.sql");
const COMPLETED_FLOW: &str = include_str!("../../../conformance/fixtures/journal/completed-flow.sql");
const INTERRUPTED_FLOW: &str = include_str!("../../../conformance/fixtures/journal/interrupted-flow.sql");
const DEAD_FLOW: &str = include_str!("../../../conformance/fixtures/journal/dead-flow.sql");

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn golden_dir() -> PathBuf {
    manifest_dir().join("tests").join("golden")
}
fn check_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    if std::env::var_os("KEEL_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(actual, expected, "golden mismatch for {name}; re-run with KEEL_UPDATE_GOLDEN=1 to update");
}

fn build_journal(project: &Path) {
    let keel = project.join(".keel");
    std::fs::create_dir_all(&keel).unwrap();
    let conn = rusqlite::Connection::open(keel.join("journal.db")).unwrap();
    conn.execute_batch(JOURNAL_SCHEMA).unwrap();
    conn.execute_batch(COMPLETED_FLOW).unwrap();
    conn.execute_batch(INTERRUPTED_FLOW).unwrap();
    conn.execute_batch(DEAD_FLOW).unwrap();
}

fn build_discovery(project: &Path) {
    let keel = project.join(".keel");
    std::fs::create_dir_all(&keel).unwrap();
    let store = DiscoveryStore::open(keel.join("discovery.db"), ManualClock::new(T0)).unwrap();
    store
        .merge_report(&[
            TargetStats {
                target: "api.example.com".to_owned(),
                calls: 100, attempts: 102, retries: 12, successes: 88, failures: 2, cache_hits: 10,
                throttled: 3, breaker_opens: 1, total_latency_ms: 12_000, max_latency_ms: 300,
                first_seen_ms: T0, last_seen_ms: T0 + 120_000,
                last_error_class: Some(keel_journal::ErrorClass::Http), last_error_status: Some(503),
                not_retried: 1, unwrapped_calls: 0,
            },
            TargetStats {
                target: "llm:openai".to_owned(),
                calls: 40, attempts: 20, retries: 0, successes: 20, failures: 0, cache_hits: 20,
                throttled: 0, breaker_opens: 0, total_latency_ms: 8_000, max_latency_ms: 400,
                first_seen_ms: T0, last_seen_ms: T0 + 60_000,
                last_error_class: None, last_error_status: None,
                not_retried: 0, unwrapped_calls: 5,
            },
        ])
        .unwrap();
}

fn copy_events(project: &Path, runs: &[&str]) {
    let src = manifest_dir().join("tests").join("fixtures").join("events");
    let events = project.join(".keel").join("events");
    std::fs::create_dir_all(&events).unwrap();
    for run in runs {
        let name = format!("{run}.ndjson");
        std::fs::copy(src.join(&name), events.join(&name)).unwrap();
    }
}

/// discovery + journal + one event run: the full evidence set.
fn full_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    build_discovery(dir.path());
    copy_events(dir.path(), &["0000000f00d-0001"]);
    let p = dir.path().to_path_buf();
    (dir, p)
}

fn opts() -> ReportOptions {
    ReportOptions { out: None, open: false, watch: false, interval: report::DEFAULT_INTERVAL, serve: false, port: 0 }
}

#[test]
fn report_json_matches_golden() {
    let (_d, project) = full_project();
    let data = report::assemble(&project, T0, Mode::Static, 2000, None).unwrap().expect("evidence present");
    check_golden("report.json", &json_string(&to_json(&data)));
}

#[test]
fn blob_status_is_byte_identical_to_status_json() {
    let (_d, project) = full_project();
    let data = report::assemble(&project, T0, Mode::Static, 2000, None).unwrap().unwrap();
    assert_eq!(to_json(&data)["status"], keel_cli::status::run(&project, T0).json);
}

#[test]
fn blob_carries_the_newest_run_and_a_cursor() {
    let (_d, project) = full_project();
    let data = report::assemble(&project, T0, Mode::Static, 2000, None).unwrap().unwrap();
    assert_eq!(data.run.as_ref().unwrap().id, "0000000f00d-0001");
    assert!(!data.events.is_empty());
    assert!(data.events.len() <= report::EVENT_LIMIT);
    assert!(data.events_seq > 0);
    // since == cursor → no events, same cursor.
    let again = report::assemble(&project, T0, Mode::Serve, 0, Some(data.events_seq)).unwrap().unwrap();
    assert!(again.events.is_empty());
    assert_eq!(again.events_seq, data.events_seq);
}

#[test]
fn discovery_only_project_still_reports() {
    let dir = tempfile::TempDir::new().unwrap();
    build_discovery(dir.path());
    let data = report::assemble(dir.path(), T0, Mode::Static, 2000, None).unwrap().unwrap();
    assert!(data.run.is_none());
    assert!(data.events.is_empty());
    assert_eq!(data.events_seq, 0);
    assert_eq!(data.status.calls, 140);
}

#[test]
fn no_evidence_is_a_nudge_exit_0_and_writes_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let r = report::run_static(dir.path(), &opts(), T0, false);
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    assert_eq!(r.human, keel_cli::status::NO_EVIDENCE);
    assert!(!dir.path().join(".keel").join("report.html").exists());
}

#[test]
fn json_mode_prints_the_blob_and_writes_nothing() {
    let (_d, project) = full_project();
    let r = report::run_static(&project, &opts(), T0, true);
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    assert_eq!(r.json["v"], 1);
    assert_eq!(r.json["mode"], "static");
    assert!(!project.join(".keel").join("report.html").exists());
}

#[test]
fn json_conflicts_with_watch_and_serve() {
    let (_d, project) = full_project();
    let watch = ReportOptions { watch: true, ..opts() };
    assert_eq!(report::run_static(&project, &watch, T0, true).exit, keel_cli::EXIT_USAGE);
    let serve = ReportOptions { serve: true, ..opts() };
    assert_eq!(report::run_static(&project, &serve, T0, true).exit, keel_cli::EXIT_USAGE);
}

#[test]
fn parse_interval_accepts_seconds_and_millis() {
    use std::time::Duration;
    assert_eq!(report::parse_interval("2s").unwrap(), Duration::from_secs(2));
    assert_eq!(report::parse_interval("500ms").unwrap(), Duration::from_millis(500));
    assert_eq!(report::parse_interval("3").unwrap(), Duration::from_secs(3));
    assert!(report::parse_interval("0s").is_err());
    assert!(report::parse_interval("soon").is_err());
}

#[test]
fn static_html_matches_golden() {
    let (_d, project) = full_project();
    let data = report::assemble(&project, T0, Mode::Static, 2000, None).unwrap().unwrap();
    check_golden("report.html", &keel_cli::report_html::render(&data));
}

#[test]
fn static_html_is_self_contained() {
    let (_d, project) = full_project();
    let data = report::assemble(&project, T0, Mode::Static, 2000, None).unwrap().unwrap();
    let html = keel_cli::report_html::render(&data);
    assert!(html.contains("Content-Security-Policy"));
    assert!(html.contains("id=\"keel-data\""));
    assert!(!html.contains("http://"), "no external references");
    assert!(!html.contains("https://"), "no external references");
    assert!(!html.contains("src=\"") || html.contains("src=\"data:"), "no external script/img sources");
    // A `</script>` inside the blob would end the data element early.
    let blob_start = html.find("id=\"keel-data\"").unwrap();
    let blob_end = html[blob_start..].find("</script>").unwrap() + blob_start;
    assert!(!html[blob_start..blob_end].contains("</"), "blob escapes </");
}

/// `</` escaping alone is not enough: `<!--<script` drives the HTML5
/// tokenizer into the "script data double escaped" state, which swallows the
/// template's own real `</script>` closing tag (and everything after it,
/// including the page's script) as inert text. Only escaping every `<` (not
/// just `</`) prevents this. See task-9-review.md Finding 1.
#[test]
fn hostile_event_data_cannot_corrupt_the_page_via_the_double_escape_state() {
    let (_d, project) = full_project();
    let mut data = report::assemble(&project, T0, Mode::Static, 2000, None).unwrap().unwrap();
    let hostile = "<!--<script>alert(1)</script>";
    data.events.push(serde_json::json!({
        "v": 1,
        "seq": 9999,
        "ms": 999_999,
        "event": "call_start",
        "call": "t-hostile",
        "target": "evil.example.com",
        "op": hostile,
    }));
    let html = keel_cli::report_html::render(&data);

    let tag_start = html.find("id=\"keel-data\"").unwrap();
    let open_end = html[tag_start..].find('>').unwrap() + tag_start + 1;
    let close_start = html[open_end..].find("</script>").unwrap() + open_end;
    let blob = &html[open_end..close_start];

    assert!(!blob.contains('<'), "escaped blob must contain no bare '<'; page can otherwise render blank");
    let parsed: serde_json::Value = serde_json::from_str(blob).expect("blob round-trips as JSON");
    let hostile_event = parsed["events"]
        .as_array()
        .expect("events array")
        .iter()
        .find(|e| e["call"] == "t-hostile")
        .expect("hostile event present");
    assert_eq!(hostile_event["op"], hostile);
}

#[test]
fn run_static_writes_the_page_atomically() {
    let (_d, project) = full_project();
    let r = report::run_static(&project, &opts(), T0, false);
    assert_eq!(r.exit, keel_cli::EXIT_OK, "{}", r.human);
    let out = project.join(".keel").join("report.html");
    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("id=\"keel-data\""));
    assert!(r.human.contains("wrote"));
    // No temp file left beside the target.
    let leftovers: Vec<_> = std::fs::read_dir(project.join(".keel"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn out_flag_and_missing_parent_dir() {
    let (_d, project) = full_project();
    let custom = project.join("reports").join("nested").join("r.html");
    let o = ReportOptions { out: Some(custom.clone()), ..opts() };
    let r = report::run_static(&project, &o, T0, false);
    assert_eq!(r.exit, keel_cli::EXIT_OK, "{}", r.human);
    assert!(custom.exists());
    assert_eq!(r.json["written"], custom.display().to_string());
}

mod watch_tests {
    use super::{T0, full_project, opts};
    use keel_cli::report::{self, ReportOptions};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn a_preset_stop_flag_writes_exactly_once_and_returns() {
        let (_d, project) = full_project();
        let stop = AtomicBool::new(true);
        let mut out = Vec::new();
        let o = ReportOptions { watch: true, interval: Duration::from_millis(20), ..opts() };
        report::run_watch(&project, &o, || T0, &stop, &mut out).unwrap();
        let html = std::fs::read_to_string(project.join(".keel").join("report.html")).unwrap();
        assert!(html.contains("\"mode\":\"watch\""));
        assert!(html.contains("\"watch_interval_ms\":20"));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("watching"), "{text}");
        let leftovers: Vec<_> = std::fs::read_dir(project.join(".keel"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn rewrites_until_stopped() {
        let (_d, project) = full_project();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stopper = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            stopper.store(true, Ordering::SeqCst);
        });
        let calls = std::sync::atomic::AtomicI64::new(0);
        let now = || T0 + calls.fetch_add(1, Ordering::SeqCst); // each rewrite stamps a new generated_at_ms
        let mut out = Vec::new();
        let o = ReportOptions { watch: true, interval: Duration::from_millis(20), ..opts() };
        report::run_watch(&project, &o, now, &stop, &mut out).unwrap();
        assert!(calls.load(Ordering::SeqCst) >= 2, "rewrote more than once before stop");
        let html = std::fs::read_to_string(project.join(".keel").join("report.html")).unwrap();
        assert!(html.contains("\"mode\":\"watch\""));
    }

    #[test]
    fn no_evidence_returns_the_nudge() {
        let dir = tempfile::TempDir::new().unwrap();
        let stop = AtomicBool::new(true);
        let mut out = Vec::new();
        let o = ReportOptions { watch: true, ..opts() };
        let err = report::run_watch(dir.path(), &o, || T0, &stop, &mut out).unwrap_err();
        assert_eq!(err.exit, keel_cli::EXIT_OK);
        assert_eq!(err.human, keel_cli::status::NO_EVIDENCE);
    }
}
