//! `keel record` — capture effects during a run, then turn the capture into
//! an offline replay fixture + generated test glue (dx-spec §6 standing
//! structure item 5; `docs/recording-format.md`).
//!
//! Subcommands:
//!
//! - `keel record run <script> [args…]` — exactly `keel run <script>
//!   [args…]` (same dispatch via [`run::plan`]/[`run::exec_with`], same
//!   exit-code passthrough), plus `KEEL_RECORD=<fresh path>` in the child's
//!   environment. The front end (not this binary) does the actual capture —
//!   see `python/keel/src/keel/_record.py`, `node/keel/src/record.mjs`.
//! - `keel record list` — recordings under `.keel/recordings/`, newest first.
//! - `keel record test <recording> [--out DIR]` — generate a pytest fixture
//!   (Python recording) or `node:test` file (Node recording) from a
//!   completed recording.
//!
//! Non-contract end to end: the `.ndjson` line format lives entirely in the
//! front ends and is versioned but outside `contracts/` — this module only
//! reads/writes plain files under `.keel/recordings/`, never `contracts/`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::render::to_json;
use crate::{EXIT_FAILURE, EXIT_USAGE, Rendered, run};

/// The subdirectory of `.keel/` holding recordings (mirrors
/// `keel-core::events::EVENTS_SUBDIR`'s convention for non-contract, per-run
/// tooling data).
pub const RECORDINGS_SUBDIR: &str = "recordings";
/// File extension of a recording (newline-delimited JSON).
pub const RECORDING_EXT: &str = "ndjson";

/// A fresh recording id: zero-padded hex epoch-milliseconds plus the process
/// id (lexically sortable — newest last — and collision-free across
/// concurrent `keel record run` invocations without a extra dependency).
fn new_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!("{ms:011x}-{:04x}", std::process::id() & 0xffff)
}

/// `.keel/recordings/` under `project`.
fn recordings_dir(project: &Path) -> PathBuf {
    project.join(".keel").join(RECORDINGS_SUBDIR)
}

// ---------------------------------------------------------------------------
// `keel record run`
// ---------------------------------------------------------------------------

/// `keel record run <target> [args…]`: plan the dispatch exactly like `keel
/// run`, then exec with `KEEL_RECORD` set to a fresh path under
/// `.keel/recordings/` so the front end tees every intercepted call into it.
/// Otherwise byte-identical to `keel run` — recording is a pure observer
/// (`docs/recording-format.md`).
pub fn run(project: &Path, target: &str, args: &[String]) -> (Option<Rendered>, i32) {
    let plan = match run::plan(target, args, false) {
        Ok(p) => p,
        Err(e) => {
            let r = e.render();
            let code = r.exit;
            return (Some(r), code);
        }
    };
    let dir = recordings_dir(project);
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return (
            Some(soft_error(&format!(
                "could not create {}: {err}.",
                dir.display()
            ))),
            EXIT_FAILURE,
        );
    }
    let path = dir.join(format!("{}.{RECORDING_EXT}", new_id()));
    let path_str = path.to_string_lossy().into_owned();
    match run::exec_with(&plan, |cmd| {
        cmd.env("KEEL_RECORD", &path_str);
    }) {
        Ok(code) => (None, code),
        Err(r) => {
            let code = r.exit;
            (Some(r), code)
        }
    }
}

// ---------------------------------------------------------------------------
// `keel record list`
// ---------------------------------------------------------------------------

/// One recording row for `keel record list`.
#[derive(Debug, Serialize)]
struct RecordingRow {
    args: Vec<String>,
    body_captured: usize,
    calls: usize,
    errors: usize,
    id: String,
    language: String,
    started_at_ms: i64,
    target: String,
}

/// The `keel record list` report.
#[derive(Debug, Serialize)]
struct RecordingsReport {
    count: usize,
    recordings: Vec<RecordingRow>,
}

/// `keel record list` for `project`: one row per `.keel/recordings/*.ndjson`
/// file, newest first. A file that isn't a readable Keel recording (no `meta`
/// header) is skipped rather than failing the whole listing — best-effort,
/// like `keel tail`'s NDJSON line hygiene.
pub fn list(project: &Path) -> Rendered {
    let dir = recordings_dir(project);
    let mut rows: Vec<RecordingRow> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(RECORDING_EXT) {
                continue;
            }
            if let Some(row) = read_recording_row(&path) {
                rows.push(row);
            }
        }
    }
    rows.sort_by(|a, b| b.id.cmp(&a.id)); // newest first
    let report = RecordingsReport {
        count: rows.len(),
        recordings: rows,
    };
    let human = list_human(&report);
    Rendered::ok(human, to_json(&report))
}

/// Parse one recording file into its list row: the `meta` header plus a scan
/// of every `call` line for counts. Returns `None` for anything that is not a
/// readable Keel recording (missing/foreign header).
fn read_recording_row(path: &Path) -> Option<RecordingRow> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let meta: Value = serde_json::from_str(lines.next()?.trim()).ok()?;
    if meta.get("type").and_then(Value::as_str) != Some("meta") {
        return None;
    }
    let str_field = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let args = meta
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let mut calls = 0usize;
    let mut body_captured = 0usize;
    let mut errors = 0usize;
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("call") {
            continue;
        }
        calls += 1;
        if v.get("body_captured").and_then(Value::as_bool) == Some(true) {
            body_captured += 1;
        }
        if v.get("outcome")
            .and_then(|o| o.get("result"))
            .and_then(Value::as_str)
            == Some("error")
        {
            errors += 1;
        }
    }

    Some(RecordingRow {
        args,
        body_captured,
        calls,
        errors,
        id: str_field("id"),
        language: if meta.get("language").and_then(Value::as_str).is_some() {
            str_field("language")
        } else {
            "?".to_owned()
        },
        started_at_ms: meta
            .get("started_at_ms")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        target: str_field("target"),
    })
}

/// The human `keel record list` table, derived entirely from
/// [`RecordingsReport`].
fn list_human(report: &RecordingsReport) -> String {
    if report.recordings.is_empty() {
        return "keel \u{25b8} no recordings yet.\n  `keel record run <script>` to make one."
            .to_owned();
    }
    let mut lines = vec![format!(
        "keel \u{25b8} recordings: {} total\n",
        report.count
    )];
    for r in &report.recordings {
        lines.push(format!(
            "  {}  {:<7} {}  {} calls ({} with body, {} errors)\n",
            r.id, r.language, r.target, r.calls, r.body_captured, r.errors
        ));
    }
    lines.concat()
}

// ---------------------------------------------------------------------------
// `keel record test`
// ---------------------------------------------------------------------------

/// Resolve `recording` (an exact id, a filesystem path, or an unambiguous id
/// substring) to one recording file under `.keel/recordings/`.
fn resolve_recording(project: &Path, recording: &str) -> Result<PathBuf, Rendered> {
    let as_path = Path::new(recording);
    if as_path.is_file() {
        return Ok(as_path.to_path_buf());
    }
    let dir = recordings_dir(project);
    let exact = dir.join(format!("{recording}.{RECORDING_EXT}"));
    if exact.is_file() {
        return Ok(exact);
    }
    let mut ids: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|e| e.to_str()) == Some(RECORDING_EXT) {
                p.file_stem().and_then(|s| s.to_str()).map(str::to_owned)
            } else {
                None
            }
        })
        .filter(|id| id.contains(recording))
        .collect();
    ids.sort();
    match ids.len() {
        0 => Err(soft_error(&format!(
            "no recording matches {recording:?} under {}. `keel record list` to see what exists.",
            dir.display()
        ))),
        1 => Ok(dir.join(format!("{}.{RECORDING_EXT}", ids[0]))),
        n => Err(soft_error(&format!(
            "{recording:?} matches {n} recordings ({}); use a full id or path.",
            ids.join(", ")
        ))),
    }
}

/// `keel record test <recording> [--out DIR]`: read the recording's `meta`
/// header to learn its language, then write one ready-to-run generated test
/// file next to it (or under `out_dir`).
pub fn test_gen(project: &Path, recording: &str, out_dir: Option<&Path>) -> Rendered {
    let path = match resolve_recording(project, recording) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(err) => return soft_error(&format!("could not read {}: {err}.", path.display())),
    };
    let Some(first_line) = text.lines().next() else {
        return soft_error(&format!(
            "{} is empty \u{2014} nothing was recorded.",
            path.display()
        ));
    };
    let meta: Value = match serde_json::from_str(first_line.trim()) {
        Ok(v) => v,
        Err(err) => {
            return soft_error(&format!(
                "{} has no readable meta header: {err}.",
                path.display()
            ));
        }
    };
    let id = meta.get("id").and_then(Value::as_str).map_or_else(
        || {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("recording")
                .to_owned()
        },
        str::to_owned,
    );
    let language = meta.get("language").and_then(Value::as_str).unwrap_or("");
    let target = meta.get("target").and_then(Value::as_str).unwrap_or("");
    let abs_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let dest_dir = out_dir.map_or_else(
        || path.parent().map(Path::to_path_buf).unwrap_or_default(),
        Path::to_path_buf,
    );

    let (file_name, contents) = match language {
        "python" => (
            format!("test_{id}_replay.py"),
            python_fixture(&id, &abs_path, target),
        ),
        "node" => (
            format!("{id}.replay.test.mjs"),
            node_fixture(&id, &abs_path, target),
        ),
        other => {
            return soft_error(&format!(
                "{} was recorded by an unknown language {other:?} \u{2014} cannot generate test glue for it.",
                path.display()
            ));
        }
    };
    if let Err(err) = std::fs::create_dir_all(&dest_dir) {
        return soft_error(&format!("could not create {}: {err}.", dest_dir.display()));
    }
    let dest = dest_dir.join(&file_name);
    if let Err(err) = std::fs::write(&dest, contents) {
        return soft_error(&format!("could not write {}: {err}.", dest.display()));
    }

    let report = TestGenReport {
        generated: &dest.to_string_lossy(),
        language,
        recording: abs_path.to_string_lossy().into_owned(),
    };
    Rendered::ok(
        format!(
            "keel \u{25b8} generated {} from {}.\n  Fill in the call under test, then run it with your test runner.",
            dest.display(),
            abs_path.display()
        ),
        to_json(&report),
    )
}

/// The machine twin of `keel record test`'s human confirmation.
#[derive(Serialize)]
struct TestGenReport<'a> {
    generated: &'a str,
    language: &'a str,
    recording: String,
}

fn python_fixture(id: &str, recording_path: &Path, target: &str) -> String {
    let path_str = recording_path.to_string_lossy();
    // `id` (e.g. `19f7756bb6b-065f`) is hyphenated — fine as a file-name
    // component and in prose (the docstring's `keel record test {id}` hint,
    // which must stay the real id so it's copy-pasteable), but NOT a legal
    // Python identifier. Only the `def test_..._replay` name needs the
    // sanitized form.
    let py_ident = id.replace('-', "_");
    format!(
        "\"\"\"Auto-generated by `keel record test` from the recording at\n{path_str}. Keel serves the recorded call outcomes for calls matching\nthe request-matching rule in docs/recording-format.md, and raises\n`keel.testing.UnmatchedEffect` on any call the recording does not cover —\nnever a silent live pass-through. Fill in the test body below; regenerate\nthis file (`keel record test {id}`) if you re-record.\n\"\"\"\n\nfrom keel.testing import replay_fixture\n\nkeel_replay = replay_fixture({path_str:?})\n\n\ndef test_{py_ident}_replay(keel_replay):\n    # TODO: call the code you recorded ({target:?}) here. Every intercepted\n    # effect it makes is served from the recording above.\n    raise NotImplementedError(\"fill in the call under test\")\n"
    )
}

fn node_fixture(id: &str, recording_path: &Path, target: &str) -> String {
    let path_json = to_json(&recording_path.to_string_lossy().into_owned());
    let target_json = to_json(&target.to_owned());
    format!(
        "// Auto-generated by `keel record test` from the recording at\n// {}. Keel serves the recorded call outcomes for calls matching the\n// request-matching rule in docs/recording-format.md, and throws\n// UnmatchedEffectError on any call the recording does not cover — never a\n// silent live pass-through. Fill in the test body below; regenerate this\n// file (`keel record test {id}`) if you re-record.\nimport test from \"node:test\";\nimport {{ withReplay }} from \"keel/testing\";\n\ntest(\"{id} replay\", async () => {{\n  await withReplay({path_json}, async () => {{\n    // TODO: call the code you recorded ({target_json}) here. Every\n    // intercepted effect it makes is served from the recording above.\n    throw new Error(\"fill in the call under test\");\n  }});\n}});\n",
        recording_path.display(),
    )
}

/// A precise, non-fatal-to-the-process guidance error (exit 1, stderr) —
/// mirrors `crate::flows::soft_error`.
fn soft_error(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct ErrReport<'a> {
        error: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&ErrReport { error: message }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}

/// A usage error for `keel record` itself (an unresolvable `--out`, etc.).
#[allow(dead_code)] // reserved for a future `--out` validation path
fn usage_error(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct ErrReport<'a> {
        error: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&ErrReport { error: message }),
        exit: EXIT_USAGE,
        to_stderr: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn project() -> TempDir {
        TempDir::new().unwrap()
    }

    fn write_recording(
        dir: &Path,
        id: &str,
        language: &str,
        target: &str,
        calls: &[&str],
    ) -> PathBuf {
        let recordings = dir.join(".keel").join(RECORDINGS_SUBDIR);
        fs::create_dir_all(&recordings).unwrap();
        let path = recordings.join(format!("{id}.{RECORDING_EXT}"));
        let mut body = format!(
            "{{\"v\":1,\"type\":\"meta\",\"id\":{id:?},\"language\":{language:?},\"target\":{target:?},\"args\":[],\"started_at_ms\":1000,\"redacted_headers\":[]}}\n"
        );
        for call in calls {
            body.push_str(call);
            body.push('\n');
        }
        fs::write(&path, body).unwrap();
        path
    }

    /// Actually parse `source` as Python (`compile(..., 'exec')`), so a
    /// generated fixture's syntax is verified for real rather than trusted
    /// from a string-contains assertion (issue #31 shipped undetected
    /// specifically because no test ever did this). Skips quietly if
    /// python3 isn't on PATH, matching this crate's other python3-gated
    /// tests (e.g. `mcp.rs`'s doctor-report tests).
    fn py_compile(source: &str) {
        use std::io::Write as _;
        let Ok(mut child) = std::process::Command::new("python3")
            .arg("-c")
            .arg("import sys; compile(sys.stdin.read(), '<generated>', 'exec')")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            eprintln!("skip: python3 not available");
            return;
        };
        child
            .stdin
            .take()
            .expect("child stdin")
            .write_all(source.as_bytes())
            .expect("write source");
        let out = child.wait_with_output().expect("wait for python3");
        assert!(
            out.status.success(),
            "generated fixture is not valid Python: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    const OK_CALL: &str = r#"{"v":1,"type":"call","seq":1,"target":"api.example.com","op":"GET api.example.com/x","idempotent":true,"args_hash":"abc","attempts":1,"latency_ms":5,"body_captured":true,"outcome":{"v":1,"result":"ok","payload":{"__keel_http__":1,"status":200,"headers":[],"body_b64":"eA=="},"attempts":1,"from_cache":false,"waits_ms":[],"throttled":false,"throttle_wait_ms":0,"breaker":"closed","trace_id":"t-1"}}"#;
    const ERROR_CALL: &str = r#"{"v":1,"type":"call","seq":2,"target":"api.example.com","op":"POST api.example.com/y","idempotent":false,"args_hash":null,"attempts":3,"latency_ms":9,"body_captured":false,"outcome":{"v":1,"result":"error","error":{"code":"KEEL-E010","class":"http","message":"HTTP 503"},"attempts":3,"from_cache":false,"waits_ms":[10,20],"throttled":false,"throttle_wait_ms":0,"breaker":"closed","trace_id":"t-2"}}"#;

    #[test]
    fn list_reports_calls_body_captured_and_errors() {
        let dir = project();
        write_recording(
            dir.path(),
            "000000001-0000",
            "python",
            "app.py",
            &[OK_CALL, ERROR_CALL],
        );
        let r = list(dir.path());
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["count"], 1);
        let row = &r.json["recordings"][0];
        assert_eq!(row["id"], "000000001-0000");
        assert_eq!(row["language"], "python");
        assert_eq!(row["target"], "app.py");
        assert_eq!(row["calls"], 2);
        assert_eq!(row["body_captured"], 1);
        assert_eq!(row["errors"], 1);
        assert!(r.human.contains("2 calls"));
    }

    #[test]
    fn list_newest_first() {
        let dir = project();
        write_recording(dir.path(), "000000001-0000", "python", "a.py", &[]);
        write_recording(dir.path(), "000000002-0000", "node", "b.mjs", &[]);
        let r = list(dir.path());
        let ids: Vec<&str> = r.json["recordings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["000000002-0000", "000000001-0000"]);
    }

    #[test]
    fn list_empty_directory_is_a_friendly_zero() {
        let dir = project();
        let r = list(dir.path());
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["count"], 0);
        assert!(r.human.contains("no recordings yet"));
    }

    #[test]
    fn list_skips_files_without_a_meta_header() {
        let dir = project();
        let recordings = dir.path().join(".keel").join(RECORDINGS_SUBDIR);
        fs::create_dir_all(&recordings).unwrap();
        fs::write(recordings.join("garbage.ndjson"), "not json\n").unwrap();
        let r = list(dir.path());
        assert_eq!(r.json["count"], 0);
    }

    #[test]
    fn resolve_by_exact_id_and_unique_substring() {
        let dir = project();
        write_recording(dir.path(), "000000001-0000", "python", "a.py", &[]);
        assert!(resolve_recording(dir.path(), "000000001-0000").is_ok());
        assert!(resolve_recording(dir.path(), "0001").is_ok());
        assert!(resolve_recording(dir.path(), "does-not-exist").is_err());
    }

    #[test]
    fn resolve_ambiguous_substring_is_a_precise_error() {
        let dir = project();
        write_recording(dir.path(), "000000001-0000", "python", "a.py", &[]);
        write_recording(dir.path(), "000000001-0001", "python", "a.py", &[]);
        let err = resolve_recording(dir.path(), "000000001").unwrap_err();
        assert!(err.human.contains("matches 2 recordings"));
    }

    #[test]
    fn test_gen_writes_a_pytest_fixture_for_a_python_recording() {
        let dir = project();
        write_recording(
            dir.path(),
            "000000001-0000",
            "python",
            "py:app.pipeline:main",
            &[OK_CALL],
        );
        let r = test_gen(dir.path(), "000000001-0000", None);
        assert_eq!(r.exit, crate::EXIT_OK, "{r:?}");
        let generated = r.json["generated"].as_str().unwrap();
        assert!(generated.ends_with("test_000000001-0000_replay.py"));
        let contents = fs::read_to_string(generated).unwrap();
        assert!(contents.contains("from keel.testing import replay_fixture"));
        assert!(contents.contains("def test_000000001_0000_replay(keel_replay):"));
        assert!(contents.contains("keel record test 000000001-0000"));
        assert!(contents.contains("py:app.pipeline:main"));
        py_compile(&contents);
    }

    #[test]
    fn test_gen_writes_a_node_test_file_for_a_node_recording() {
        let dir = project();
        write_recording(dir.path(), "000000002-0000", "node", "app.mjs", &[OK_CALL]);
        let r = test_gen(dir.path(), "000000002-0000", None);
        assert_eq!(r.exit, crate::EXIT_OK, "{r:?}");
        let generated = r.json["generated"].as_str().unwrap();
        assert!(generated.ends_with("000000002-0000.replay.test.mjs"));
        let contents = fs::read_to_string(generated).unwrap();
        assert!(contents.contains("import { withReplay } from \"keel/testing\";"));
        assert!(contents.contains("test(\"000000002-0000 replay\""));
    }

    #[test]
    fn test_gen_out_dir_is_honored() {
        let dir = project();
        write_recording(dir.path(), "000000001-0000", "python", "a.py", &[OK_CALL]);
        let out = dir.path().join("tests");
        let r = test_gen(dir.path(), "000000001-0000", Some(&out));
        assert_eq!(r.exit, crate::EXIT_OK);
        assert!(
            r.json["generated"]
                .as_str()
                .unwrap()
                .contains(&out.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn test_gen_unknown_language_is_a_precise_error() {
        let dir = project();
        let recordings = dir.path().join(".keel").join(RECORDINGS_SUBDIR);
        fs::create_dir_all(&recordings).unwrap();
        fs::write(
            recordings.join("x.ndjson"),
            "{\"v\":1,\"type\":\"meta\",\"id\":\"x\",\"language\":\"ruby\",\"target\":\"a.rb\",\"args\":[],\"started_at_ms\":0,\"redacted_headers\":[]}\n",
        )
        .unwrap();
        let r = test_gen(dir.path(), "x", None);
        assert_eq!(r.exit, EXIT_FAILURE);
        assert!(r.human.contains("unknown language"));
    }

    #[test]
    fn run_reports_dispatch_errors_exactly_like_keel_run() {
        // `record::run` reuses `run::plan`, so an undispatchable target renders
        // the identical what/why/next error `keel run` would — no subprocess
        // ever spawns, so this needs no python3/node on the test machine.
        let dir = project();
        let (rendered, code) = run(dir.path(), "does-not-exist.py", &[]);
        let r = rendered.expect("a dispatch error renders");
        assert_eq!(code, EXIT_USAGE);
        assert!(r.human.contains("no such file or directory"));
        // No recordings directory was created for a target that never dispatched.
        assert!(!dir.path().join(".keel").join(RECORDINGS_SUBDIR).exists());
    }

    #[test]
    fn new_id_is_lowercase_hex_and_monotonic_enough_to_sort() {
        let a = new_id();
        let b = new_id();
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert!(a.contains('-'));
        // Not asserting a < b (millisecond clocks can tie within a test), only
        // that the shape is stable and reused by `run`'s path construction.
        assert_eq!(a.split('-').count(), 2);
        assert_eq!(b.split('-').count(), 2);
    }
}
