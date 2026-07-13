//! `keel tail` integration tests: golden-pinned `--no-follow` snapshots over
//! fixture feeds (human + NDJSON twins), run selection, and a live-engine
//! parity leg — the real `keel-core` engine writes a feed through its event
//! sink and `tail` must render it byte-exactly (virtual clock, fixed run id),
//! so the CLI's tolerant JSON renderer can never drift from the engine's
//! vocabulary unnoticed.

use std::path::{Path, PathBuf};

use keel_cli::tail::{self, TailOptions, Ticker};

/// A never-ticking ticker: `--no-follow` paths must not poll.
struct NoTick;

impl Ticker for NoTick {
    fn tick(&mut self) -> bool {
        false
    }
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn golden_dir() -> PathBuf {
    manifest_dir().join("tests").join("golden")
}

/// Compare `actual` to the named golden file, or rewrite it under
/// `KEEL_UPDATE_GOLDEN` (same protocol as tests/cli.rs).
fn check_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    if std::env::var_os("KEEL_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "golden mismatch for {name}; re-run with KEEL_UPDATE_GOLDEN=1 to update"
    );
}

/// A project dir whose `.keel/events/` holds the named fixture feeds.
fn project_with_fixture_runs(runs: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let src = manifest_dir().join("tests").join("fixtures").join("events");
    let dir = tempfile::TempDir::new().unwrap();
    let events = dir.path().join(".keel").join("events");
    std::fs::create_dir_all(&events).unwrap();
    for run in runs {
        let name = format!("{run}.ndjson");
        std::fs::copy(src.join(&name), events.join(&name)).unwrap();
    }
    let project = dir.path().to_path_buf();
    (dir, project)
}

fn snapshot(project: &Path, json: bool, run: Option<&str>) -> String {
    let opts = TailOptions {
        color: false,
        follow: false,
        json,
        run: run.map(str::to_owned),
    };
    let mut out = Vec::new();
    tail::run(project, &opts, &mut out, &mut NoTick).expect("tail snapshot succeeds");
    String::from_utf8(out).unwrap()
}

#[test]
fn tail_no_follow_renders_the_story_feed() {
    let (_d, project) = project_with_fixture_runs(&["0000000f00d-0001"]);
    check_golden("tail_story.txt", &snapshot(&project, false, None));
}

#[test]
fn tail_json_is_ndjson_passthrough_with_sorted_keys() {
    let (_d, project) = project_with_fixture_runs(&["0000000f00d-0001"]);
    let out = snapshot(&project, true, None);
    check_golden("tail_story.ndjson", &out);
    // Every emitted line is standalone JSON with sorted keys.
    for line in out.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid NDJSON line");
        assert_eq!(serde_json::to_string(&v).unwrap(), line);
    }
}

#[test]
fn tail_follows_the_newest_run_unless_pinned() {
    let (_d, project) = project_with_fixture_runs(&["0000000f00d-0001", "0000000f00e-0002"]);
    // Newest run (by name sort — run ids are hex epoch-ms) wins.
    let newest = snapshot(&project, false, None);
    assert!(newest.contains("run 0000000f00e-0002"));
    assert!(!newest.contains("run 0000000f00d-0001"));
    // --run pins the older feed.
    let pinned = snapshot(&project, false, Some("0000000f00d-0001"));
    assert!(pinned.contains("run 0000000f00d-0001"));
}

#[test]
fn tail_unknown_run_and_missing_dir_guide_the_user() {
    let (_d, project) = project_with_fixture_runs(&["0000000f00d-0001"]);
    let opts = TailOptions {
        color: false,
        follow: false,
        json: false,
        run: Some("nope".to_owned()),
    };
    let mut out = Vec::new();
    let err = tail::run(&project, &opts, &mut out, &mut NoTick).unwrap_err();
    assert_eq!(err.exit, keel_cli::EXIT_FAILURE);
    assert!(err.to_stderr);
    assert!(err.human.contains("run \"nope\" not found"));
    assert!(err.human.contains("0000000f00d-0001"));

    let bare = tempfile::TempDir::new().unwrap();
    let mut out = Vec::new();
    let opts = TailOptions {
        color: false,
        follow: false,
        json: false,
        run: None,
    };
    let err = tail::run(bare.path(), &opts, &mut out, &mut NoTick).unwrap_err();
    assert!(err.human.starts_with("keel \u{25b8} nothing to tail"));
    assert!(err.human.contains("next: run `keel init`"));
}

/// The parity leg: the real engine writes the feed, `keel tail` renders it.
/// Deterministic end to end — tokio's paused clock stamps virtual `ms`, the
/// sink gets a fixed run id, and the schedule is jitter-free.
#[tokio::test(start_paused = true)]
async fn tail_renders_the_real_engines_feed_byte_exactly() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().to_path_buf();
    let events_dir = project.join(".keel").join("events");
    std::fs::create_dir_all(&events_dir).unwrap();

    // A file-backed sink under the deterministic constructor: the run file
    // sits where `keel tail` looks, but carries no wall clock or pid.
    let file = std::fs::File::create(events_dir.join("run-live.ndjson")).unwrap();
    let sink =
        keel_core::events::EventSink::to_writer(Box::new(file), "run-live").expect("sink starts");

    let mut engine = keel_core::Engine::new();
    engine.attach_events(sink);
    engine
        .configure(&serde_json::json!({
            "target": { "api.slow.internal": {
                "retry": { "attempts": 2, "schedule": "exp(100ms, x2, max 1s)" }
            } }
        }))
        .expect("valid policy");

    let request = keel_core_api::Request {
        v: keel_core_api::ENVELOPE_VERSION,
        target: "api.slow.internal".to_owned(),
        op: "GET api.slow.internal".to_owned(),
        idempotent: true,
        args_hash: None,
    };
    let out = engine
        .execute(&request, async |_attempt| {
            keel_core_api::AttemptResult::Error {
                class: keel_core_api::ErrorClass::Timeout,
                http_status: None,
                retry_after_ms: None,
                message: "read timeout".to_owned(),
                original: None,
            }
        })
        .await;
    assert!(out.error.is_some(), "the call must exhaust its attempts");
    engine.events().expect("sink attached").flush();

    assert_eq!(
        snapshot(&project, false, None),
        "00:00.000  run run-live\n\
         00:00.000  t-000001  api.slow.internal        call     GET api.slow.internal\n\
         00:00.000  t-000001  api.slow.internal        attempt  #1\n\
         00:00.000  t-000001  api.slow.internal        fail     #1 timeout\n\
         00:00.000  t-000001  api.slow.internal        backoff  100ms \u{2192} #2\n\
         00:00.100  t-000001  api.slow.internal        attempt  #2\n\
         00:00.100  t-000001  api.slow.internal        fail     #2 timeout\n\
         00:00.100  t-000001  api.slow.internal        error    KEEL-E010 after 2 attempts\n"
    );
}
