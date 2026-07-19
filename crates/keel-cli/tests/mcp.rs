//! Scripted-session tests for `keel mcp` (dx-spec §5: the CLI doubles as an
//! MCP server).
//!
//! One fixture project (scan evidence + valid keel.toml + golden journal +
//! discovery store) is driven through a fixed stdio session — `initialize` →
//! `tools/list` → every `tools/call` — and the whole transcript is byte-golden
//! (`tests/golden/mcp_session.jsonl`; regenerate deliberately with
//! `KEEL_UPDATE_GOLDEN=1`). The load-bearing assertion is *equivalence*: each
//! tool's text content must be byte-identical to the `--json` output of the
//! library producer behind the matching CLI command. A subprocess leg replays
//! the same session through the real `keel mcp` binary.

use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use keel_cli::render::json_string;
use keel_cli::{doctor, explain, flows, init, mcp, status};
use keel_journal::{Clock, DiscoveryStore, ManualClock, SystemClock, TargetStats};

/// The completed/interrupted/dead flow fixtures (2026-07-11T00:00:00Z base;
/// the SQL rows' own timestamps, unrelated to `t0` below — flows::trace
/// never takes a `now_ms`, and flows::flows only uses one for its `.human`
/// text, never `.json`, so this base never needs to track live time).
const JOURNAL_SCHEMA: &str = include_str!("../../../contracts/journal.sql");
const COMPLETED_FLOW: &str =
    include_str!("../../../conformance/fixtures/journal/completed-flow.sql");
const INTERRUPTED_FLOW: &str =
    include_str!("../../../conformance/fixtures/journal/interrupted-flow.sql");
const DEAD_FLOW: &str = include_str!("../../../conformance/fixtures/journal/dead-flow.sql");

/// The scripted session: one JSON-RPC message per line, ids 1–13. Covers the
/// handshake, the catalog, every tool (including a failing tool call), and the
/// protocol error paths.
const SESSION_SCRIPT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"golden-client","version":"1.0.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_status","arguments":{}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_doctor_report","arguments":{}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"propose_policy","arguments":{}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_flows","arguments":{}}}
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_flows","arguments":{"dead":true}}}
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"get_trace","arguments":{"flow":"01JZWY0A0000000000000001"}}}
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"explain_error","arguments":{"code":"KEEL-E014"}}}
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"explain_error","arguments":{"code":"KEEL-E999"}}}
{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"get_everything"}}
{"jsonrpc":"2.0","id":12,"method":"resources/list"}
{"jsonrpc":"2.0","id":13,"method":"ping"}
"#;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn golden_dir() -> PathBuf {
    manifest_dir().join("tests").join("golden")
}

/// Compare `actual` to the named golden file, or rewrite it under
/// `KEEL_UPDATE_GOLDEN`.
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

/// The fixture project every session runs against: a fetch call the JS scan
/// sees, a valid keel.toml, the three golden flows, and a discovery store with
/// two observed targets (`llm:openai` is discovery-only, so `propose_policy`
/// emits a nontrivial add hunk). Discovery rows are dated relative to `t0`
/// (the test's live-captured "now" — see `mcp_subprocess_transcript_matches_in_process`'s
/// doc comment for why this must track real time rather than a frozen date).
fn fixture_project(t0: i64) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        manifest_dir().join("tests/fixtures/node_fetch/app.mjs"),
        dir.path().join("app.mjs"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("keel.toml"),
        "[target.\"api.example.com\"]\nretry = { attempts = 5 }\n",
    )
    .unwrap();

    let keel = dir.path().join(".keel");
    std::fs::create_dir_all(&keel).unwrap();
    let conn = rusqlite::Connection::open(keel.join("journal.db")).unwrap();
    conn.execute_batch(JOURNAL_SCHEMA).unwrap();
    conn.execute_batch(COMPLETED_FLOW).unwrap();
    conn.execute_batch(INTERRUPTED_FLOW).unwrap();
    conn.execute_batch(DEAD_FLOW).unwrap();
    drop(conn);

    let store = DiscoveryStore::open(keel.join("discovery.db"), ManualClock::new(t0)).unwrap();
    store
        .merge_report(&[
            // Honors the discovery invariant calls == successes+failures+cache_hits.
            TargetStats {
                target: "api.example.com".to_owned(),
                calls: 100,
                attempts: 102,
                retries: 12,
                successes: 88,
                failures: 2,
                cache_hits: 10,
                throttled: 3,
                breaker_opens: 1,
                total_latency_ms: 12_000,
                max_latency_ms: 300,
                first_seen_ms: t0,
                last_seen_ms: t0 + 120_000,
                last_error_class: Some(keel_journal::ErrorClass::Http),
                last_error_status: Some(503),
                not_retried: 1,
                unwrapped_calls: 0,
            },
            TargetStats {
                target: "llm:openai".to_owned(),
                calls: 40,
                attempts: 20,
                retries: 0,
                successes: 20,
                failures: 0,
                cache_hits: 20,
                throttled: 0,
                breaker_opens: 0,
                total_latency_ms: 8_000,
                max_latency_ms: 400,
                first_seen_ms: t0,
                last_seen_ms: t0 + 60_000,
                last_error_class: None,
                last_error_status: None,
                not_retried: 0,
                unwrapped_calls: 5,
            },
        ])
        .unwrap();
    dir
}

/// Run the scripted session in-process against `project` and return the
/// response lines. `t0` is threaded through to the server's injected clock
/// (see `fixture_project`) so the in-process path's "now" tracks the same
/// live reading the fixture data was dated against.
fn run_session(project: &Path, script: &str, t0: i64) -> Vec<String> {
    let server = mcp::Server::new(project.to_path_buf(), move || t0);
    let mut out = Vec::new();
    let code = server.serve(Cursor::new(script), &mut out);
    assert_eq!(code, keel_cli::EXIT_OK, "clean EOF exits 0");
    String::from_utf8(out)
        .expect("responses are UTF-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The one build-dependent byte range: `serverInfo.version` is the crate
/// version, normalized so the golden survives version bumps.
fn normalize_version(line: &str) -> String {
    line.replace(
        &format!("\"version\":\"{}\"", env!("CARGO_PKG_VERSION")),
        "\"version\":\"<CARGO_PKG_VERSION>\"",
    )
}

/// Parse a response line and return the tool text content for `id`.
fn tool_text(lines: &[String], id: i64) -> String {
    let line = lines
        .iter()
        .find(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .is_ok_and(|v| v["id"] == serde_json::json!(id))
        })
        .unwrap_or_else(|| panic!("no response with id {id}"));
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("response {id} has no text content: {v}"))
        .to_owned()
}

/// The whole session transcript is byte-golden: handshake, catalog, every
/// tool result, the failing tool, and both protocol errors.
#[test]
fn mcp_session_transcript_matches_golden() {
    let t0 = SystemClock.now_ms();
    let dir = fixture_project(t0);
    let lines = run_session(dir.path(), SESSION_SCRIPT, t0);
    assert_eq!(lines.len(), 13, "13 requests → 13 responses");
    let transcript: String = lines.iter().map(|l| normalize_version(l) + "\n").collect();
    check_golden("mcp_session.jsonl", &transcript);
}

/// The load-bearing equivalence (dx-spec §5): every tool's text result is
/// byte-identical to the `--json` output of the library producer behind the
/// same-named CLI command.
#[test]
fn mcp_tool_outputs_are_byte_identical_to_the_json_twins() {
    let t0 = SystemClock.now_ms();
    let dir = fixture_project(t0);
    let p = dir.path();
    let lines = run_session(p, SESSION_SCRIPT, t0);

    // get_status ↔ `keel status --json`
    assert_eq!(tool_text(&lines, 3), json_string(&status::run(p, t0).json));
    // get_doctor_report ↔ `keel doctor --json`
    assert_eq!(tool_text(&lines, 4), json_string(&doctor::run(p).json));
    // propose_policy ↔ `keel init --diff --json`
    let diff = init::run(
        p,
        init::InitOptions {
            diff: true,
            stamp: false,
            agents: false,
        },
    );
    assert_eq!(tool_text(&lines, 5), json_string(&diff.json));
    // list_flows ↔ `keel flows --json` (and --dead)
    assert_eq!(
        tool_text(&lines, 6),
        json_string(&flows::flows(p, false, t0).json)
    );
    assert_eq!(
        tool_text(&lines, 7),
        json_string(&flows::flows(p, true, t0).json)
    );
    // get_trace ↔ `keel trace <flow> --json`
    assert_eq!(
        tool_text(&lines, 8),
        json_string(&flows::trace(p, "01JZWY0A0000000000000001").json)
    );
    // explain_error ↔ `keel explain <code> --json`
    assert_eq!(
        tool_text(&lines, 9),
        json_string(&explain::run("KEEL-E014").json)
    );
    // The unknown code renders the soft error's JSON twin with isError: true.
    let e999: serde_json::Value = serde_json::from_str(
        lines
            .iter()
            .find(|l| l.contains("\"id\":10"))
            .expect("id 10 response"),
    )
    .unwrap();
    assert_eq!(e999["result"]["isError"], true);
    assert_eq!(
        e999["result"]["content"][0]["text"].as_str().unwrap(),
        json_string(&explain::run("KEEL-E999").json)
    );
}

/// The diff `propose_policy` returns is nontrivial for this fixture: the
/// discovery-only `llm:openai` target becomes an add hunk with an applyable
/// patch — diffs as the lingua franca reach MCP unchanged.
#[test]
fn propose_policy_carries_the_applyable_diff() {
    let t0 = SystemClock.now_ms();
    let dir = fixture_project(t0);
    let lines = run_session(dir.path(), SESSION_SCRIPT, t0);
    let report: serde_json::Value = serde_json::from_str(&tool_text(&lines, 5)).unwrap();
    assert_eq!(report["added"], serde_json::json!(["llm:openai"]));
    assert_eq!(report["unchanged"], serde_json::json!(["api.example.com"]));
    let patch = report["patch"].as_str().unwrap();
    assert!(
        patch.starts_with("--- a/keel.toml\n+++ b/keel.toml\n"),
        "{patch}"
    );
    assert!(patch.contains("+[target.\"llm:openai\"]"));
}

/// Protocol error paths inside the golden session: an unknown tool and an
/// unknown method answer JSON-RPC errors, never tool results.
#[test]
fn protocol_errors_answer_json_rpc_errors() {
    let t0 = SystemClock.now_ms();
    let dir = fixture_project(t0);
    let lines = run_session(dir.path(), SESSION_SCRIPT, t0);
    let unknown_tool: serde_json::Value = serde_json::from_str(
        lines
            .iter()
            .find(|l| l.contains("\"id\":11"))
            .expect("id 11 response"),
    )
    .unwrap();
    assert_eq!(unknown_tool["error"]["code"], -32602);
    let unknown_method: serde_json::Value = serde_json::from_str(
        lines
            .iter()
            .find(|l| l.contains("\"id\":12"))
            .expect("id 12 response"),
    )
    .unwrap();
    assert_eq!(unknown_method["error"]["code"], -32601);
}

/// `get_doctor_report` surfaces the pre-existing-resilience finding for a
/// real project (not a synthetic `ScanResult`, unlike `doctor.rs`'s unit
/// tests) and stays byte-identical to `keel doctor --json` for it — the
/// concrete, persisted version of the manual verification this feature was
/// checked with during development.
#[test]
fn get_doctor_report_surfaces_a_real_preexisting_resilience_finding() {
    if Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skip: python3 not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("app.py"),
        "import httpx\nfrom tenacity import retry\n\n@retry\ndef call():\n    return httpx.get(\"https://api.example.com\")\n",
    )
    .unwrap();

    let script = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"get_doctor_report\",\"arguments\":{}}}\n";
    let lines = run_session(dir.path(), script, SystemClock.now_ms());
    let text = tool_text(&lines, 1);

    assert_eq!(text, json_string(&doctor::run(dir.path()).json));
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["topic"] == "preexisting-resilience")
        .unwrap_or_else(|| panic!("no preexisting-resilience finding in {report}"));
    assert!(finding["detail"].as_str().unwrap().contains("tenacity"));
}

/// Node parity to the Python preexisting-resilience test above (issue #21):
/// `get_doctor_report` surfaces the same finding for a real Node project, no
/// `python3` gate needed since the JS/TS scan is the Rust-native oxc parser.
#[test]
fn get_doctor_report_surfaces_a_real_node_preexisting_resilience_finding() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("app.mjs"),
        "import pRetry from \"p-retry\";\n\nasync function call() {\n  return pRetry(() => fetch(\"https://api.example.com\"));\n}\n",
    )
    .unwrap();

    let script = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"get_doctor_report\",\"arguments\":{}}}\n";
    let lines = run_session(dir.path(), script, SystemClock.now_ms());
    let text = tool_text(&lines, 1);

    assert_eq!(text, json_string(&doctor::run(dir.path()).json));
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["topic"] == "preexisting-resilience")
        .unwrap_or_else(|| panic!("no preexisting-resilience finding in {report}"));
    assert!(finding["detail"].as_str().unwrap().contains("p-retry"));
}

/// `get_doctor_report` surfaces the `agents-cli-config-placement` finding for
/// a real agents-cli project (manifest + `app/` + a root `keel.toml`) and
/// stays byte-identical to `keel doctor --json` for it, per the same
/// established pattern as the preexisting-resilience parity test above.
#[test]
fn get_doctor_report_surfaces_a_real_agents_cli_placement_finding() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("agents-cli-manifest.yaml"),
        "agent_directory: app\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("app")).unwrap();
    std::fs::write(
        dir.path().join("app").join("app.mjs"),
        "const r = await fetch(\"https://api.example.com/v1/x\");\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("keel.toml"),
        "[target.\"api.example.com\"]\nretry = { attempts = 5 }\n",
    )
    .unwrap();

    let script = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"get_doctor_report\",\"arguments\":{}}}\n";
    let lines = run_session(dir.path(), script, SystemClock.now_ms());
    let text = tool_text(&lines, 1);

    assert_eq!(text, json_string(&doctor::run(dir.path()).json));
    let report: serde_json::Value = serde_json::from_str(&text).unwrap();
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["topic"] == "agents-cli-config-placement")
        .unwrap_or_else(|| panic!("no agents-cli-config-placement finding in {report}"));
    assert_eq!(finding["level"], "warn");
    assert!(finding["detail"].as_str().unwrap().contains("Dockerfile"));
}

/// The real binary (`keel mcp`, project = cwd) replays the same session with a
/// byte-identical transcript. The binary always dates itself off the real
/// `SystemClock` (it has no way to receive an injected clock), so the
/// in-process harness must too — `t0` is captured live, once, right before
/// both paths run, so the two `now` readings land within milliseconds of each
/// other and the `get_status` week-window computation (which buckets by UTC
/// day relative to "now") can't straddle a day boundary between them. A
/// frozen historical constant would drift out of the subprocess's real
/// trailing-7-day window as wall-clock time passed (issue #30).
#[test]
fn mcp_subprocess_transcript_matches_in_process() {
    let t0 = SystemClock.now_ms();
    let dir = fixture_project(t0);
    let expected = run_session(dir.path(), SESSION_SCRIPT, t0).join("\n") + "\n";

    let mut child = Command::new(env!("CARGO_BIN_EXE_keel"))
        .arg("mcp")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn keel mcp");
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(SESSION_SCRIPT.as_bytes())
        .expect("write session");
    let out = child.wait_with_output().expect("wait for keel mcp");
    assert!(
        out.status.success(),
        "keel mcp failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8(out.stdout).unwrap(), expected);
}
