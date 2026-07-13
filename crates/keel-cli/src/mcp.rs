//! `keel mcp` — the CLI doubles as an MCP server for coding agents (dx-spec §5).
//!
//! A hand-rolled, dependency-free JSON-RPC 2.0 loop over stdio (one
//! newline-delimited message per line) speaking the Model Context Protocol:
//! `initialize`, `ping`, `tools/list`, `tools/call`. Exactly the six
//! spec-promised tools are exposed — `get_status`, `get_doctor_report`,
//! `propose_policy` (returns a keel.toml *diff*), `get_trace`, `list_flows`,
//! `explain_error` — and each is a thin wrapper over the same library producer
//! as the corresponding CLI command, so a tool's text result is **byte-identical
//! to that command's `--json` output** (golden-tested). An agent that already
//! understands `keel status --json` can diff the MCP result against it and see
//! no change.
//!
//! No daemon (dx-spec §3 invariant 3): the server is client-launched, owns no
//! port, and exits on stdin EOF. Determinism as courtesy (dx-spec §5): responses
//! serialize through [`serde_json::Value`] (sorted keys), the tool catalog is a
//! fixed alphabetical list, and no wall-clock value reaches any response.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::render::json_string;
use crate::{EXIT_FAILURE, EXIT_OK, Rendered, doctor, explain, flows, init, status};

/// The MCP protocol revision this server speaks natively (offered when the
/// client requests a revision we do not recognize).
const LATEST_PROTOCOL: &str = "2025-06-18";

/// Revisions we recognize and echo back unchanged, per the MCP version
/// negotiation rule ("if the server supports the requested version, it MUST
/// respond with the same version").
const SUPPORTED_PROTOCOLS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

/// Server usage notes surfaced to the client at `initialize` time.
const INSTRUCTIONS: &str = "Keel adds production-grade resilience (retry, backoff, timeout, breaker, \
rate limit, cache) and durable flows to this project with zero code changes; policy lives in keel.toml. \
Every tool's text result is byte-identical to the matching CLI --json output: \
get_status = `keel status --json`, get_doctor_report = `keel doctor --json`, \
propose_policy = `keel init --diff --json` (an applyable keel.toml patch — never writes), \
list_flows = `keel flows --json`, get_trace = `keel trace <flow> --json`, \
explain_error = `keel explain <code> --json`. Outputs are deterministic \
(sorted keys, no timestamps), so two calls can be diffed to see real change.";

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// A JSON-RPC protocol-level failure (distinct from a *tool* failure, which is
/// reported inside a successful `tools/call` result with `isError: true`).
struct RpcError {
    code: i64,
    message: String,
}

/// Build an [`RpcError`].
fn rpc_error(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}

/// The stdio MCP server: the project it reports on plus an injected clock
/// (a fn pointer so tests stay deterministic; only human-facing views consume
/// it — no wall-clock value ever reaches a JSON response).
#[derive(Debug)]
pub struct Server {
    project: PathBuf,
    now_ms: fn() -> i64,
}

impl Server {
    /// A server for `project`, dating any age computations via `now_ms`.
    #[must_use]
    pub fn new(project: PathBuf, now_ms: fn() -> i64) -> Self {
        Self { project, now_ms }
    }

    /// Run the loop: one JSON-RPC message per line in, one per line out
    /// (notifications get no reply). Returns the process exit code —
    /// [`EXIT_OK`] on clean EOF, [`EXIT_FAILURE`] on an I/O error.
    pub fn serve<R: BufRead, W: Write>(&self, input: R, mut output: W) -> i32 {
        for line in input.lines() {
            let Ok(line) = line else {
                return EXIT_FAILURE;
            };
            if line.trim().is_empty() {
                continue;
            }
            let Some(response) = self.handle_line(&line) else {
                continue;
            };
            let Ok(text) = serde_json::to_string(&response) else {
                return EXIT_FAILURE;
            };
            if writeln!(output, "{text}")
                .and_then(|()| output.flush())
                .is_err()
            {
                return EXIT_FAILURE;
            }
        }
        EXIT_OK
    }

    /// Handle one raw input line. `None` means no response is due (a
    /// notification, or a response frame we never asked for).
    fn handle_line(&self, line: &str) -> Option<Value> {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return Some(error_response(
                &Value::Null,
                PARSE_ERROR,
                "Parse error: the line is not valid JSON. Send one JSON-RPC 2.0 message per line.",
            ));
        };
        self.handle_message(&message)
    }

    /// Dispatch one parsed JSON-RPC message.
    fn handle_message(&self, message: &Value) -> Option<Value> {
        let Some(frame) = message.as_object() else {
            return Some(error_response(
                &Value::Null,
                INVALID_REQUEST,
                "Invalid request: expected a JSON-RPC 2.0 object (batches are not supported).",
            ));
        };
        let id = frame.get("id").cloned();
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            // No method: a response frame (we never send requests) or garbage.
            // Answer only when an id makes the failure addressable.
            return id.map(|id| {
                error_response(
                    &id,
                    INVALID_REQUEST,
                    "Invalid request: missing `method`. This server accepts initialize, ping, tools/list, and tools/call.",
                )
            });
        };
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        // A notification (no id) never gets a response; unknown ones are ignored.
        let id = id?;
        let outcome = match method {
            "initialize" => Ok(initialize_result(&params)),
            "ping" => Ok(json!({})),
            "tools/call" => self.tools_call(&params),
            "tools/list" => Ok(json!({ "tools": tool_catalog() })),
            other => Err(rpc_error(
                METHOD_NOT_FOUND,
                format!(
                    "Method not found: {other:?}. This server supports initialize, ping, tools/list, and tools/call."
                ),
            )),
        };
        Some(match outcome {
            Ok(result) => json!({ "id": id, "jsonrpc": "2.0", "result": result }),
            Err(e) => error_response(&id, e.code, &e.message),
        })
    }

    /// `tools/call`: run one of the six tools and wrap its `--json` twin as the
    /// text content. A tool that renders a failure (non-zero exit) is a
    /// *successful* call carrying `isError: true` — protocol errors are only
    /// for unknown tools and malformed arguments.
    fn tools_call(&self, params: &Value) -> Result<Value, RpcError> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err(rpc_error(
                INVALID_PARAMS,
                "tools/call requires a string `name` parameter naming the tool.",
            ));
        };
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let rendered = self.call_tool(name, &args)?;
        Ok(json!({
            "content": [ { "text": json_string(&rendered.json), "type": "text" } ],
            "isError": rendered.exit != EXIT_OK,
        }))
    }

    /// Map a tool name to the library producer behind the same-named CLI
    /// command. The [`Rendered`] comes back whole, so the text content is the
    /// exact bytes `keel <cmd> --json` would print.
    fn call_tool(&self, name: &str, args: &Value) -> Result<Rendered, RpcError> {
        match name {
            "explain_error" => Ok(explain::run(require_str(args, "code", name)?)),
            "get_doctor_report" => Ok(doctor::run(&self.project)),
            "get_status" => Ok(status::run(&self.project)),
            "get_trace" => Ok(flows::trace(
                &self.project,
                require_str(args, "flow", name)?,
            )),
            "list_flows" => Ok(flows::flows(
                &self.project,
                optional_bool(args, "dead", name)?,
                (self.now_ms)(),
            )),
            "propose_policy" => Ok(init::run(
                &self.project,
                init::InitOptions {
                    diff: true,
                    stamp: false,
                    agents: false,
                },
            )),
            other => Err(rpc_error(
                INVALID_PARAMS,
                format!(
                    "Unknown tool: {other:?}. Available tools: explain_error, get_doctor_report, get_status, get_trace, list_flows, propose_policy."
                ),
            )),
        }
    }
}

/// The `initialize` result: negotiated protocol version, capabilities, server
/// identity, and usage instructions.
fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LATEST_PROTOCOL);
    let version = if SUPPORTED_PROTOCOLS.contains(&requested) {
        requested
    } else {
        LATEST_PROTOCOL
    };
    json!({
        "capabilities": { "tools": {} },
        "instructions": INSTRUCTIONS,
        "protocolVersion": version,
        "serverInfo": { "name": "keel", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The fixed tool catalog, alphabetical by name. Schemas are deterministic
/// values (sorted keys on serialization), so `tools/list` is byte-stable.
fn tool_catalog() -> Value {
    json!([
        {
            "description": "Explain a KEEL-E0NN error code: what happened, why, and what to do next. Byte-identical to `keel explain <code> --json`.",
            "inputSchema": {
                "properties": {
                    "code": { "description": "The error code, e.g. \"KEEL-E014\".", "type": "string" }
                },
                "required": ["code"],
                "type": "object"
            },
            "name": "explain_error"
        },
        {
            "description": "The honesty report: what is wrapped, what is visible but unwrapped and why, adapter pins, policy validity, and the journal backend — findings carry applyable fixes where possible. Byte-identical to `keel doctor --json`.",
            "inputSchema": { "properties": {}, "type": "object" },
            "name": "get_doctor_report"
        },
        {
            "description": "One screen of what Keel is doing for this project: coverage, calls, retries saved, breaker opens, cache hit rate, and durable-flow counts. Byte-identical to `keel status --json`.",
            "inputSchema": { "properties": {}, "type": "object" },
            "name": "get_status"
        },
        {
            "description": "Trace one durable (Tier 2) flow step by step: outcomes, attempts, timings. Byte-identical to `keel trace <flow> --json`.",
            "inputSchema": {
                "properties": {
                    "flow": { "description": "A flow_id, or a substring of an id/entrypoint that names exactly one flow.", "type": "string" }
                },
                "required": ["flow"],
                "type": "object"
            },
            "name": "get_trace"
        },
        {
            "description": "List durable (Tier 2) flows: id, entrypoint, status, steps done/total. Byte-identical to `keel flows --json`.",
            "inputSchema": {
                "properties": {
                    "dead": { "description": "List only dead flows (those that exhausted their resume cap).", "type": "boolean" }
                },
                "type": "object"
            },
            "name": "list_flows"
        },
        {
            "description": "Propose policy changes as a keel.toml diff from static + observed evidence (never writes): an applyable unified patch plus structured changes. Byte-identical to `keel init --diff --json`.",
            "inputSchema": { "properties": {}, "type": "object" },
            "name": "propose_policy"
        }
    ])
}

/// A JSON-RPC error response frame.
fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "error": { "code": code, "message": message },
        "id": id,
        "jsonrpc": "2.0",
    })
}

/// A required string argument, or the invalid-params error naming the tool.
fn require_str<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, RpcError> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| {
        rpc_error(
            INVALID_PARAMS,
            format!("{tool} requires a string `{key}` argument."),
        )
    })
}

/// An optional boolean argument (absent/null → `false`); a non-boolean value is
/// an invalid-params error, never a silent coercion.
fn optional_bool(args: &Value, key: &str, tool: &str) -> Result<bool, RpcError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(rpc_error(
            INVALID_PARAMS,
            format!("{tool}'s `{key}` argument must be a boolean."),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_783_728_000_000;

    fn t0() -> i64 {
        T0
    }

    fn server_in(dir: &std::path::Path) -> Server {
        Server::new(dir.to_path_buf(), t0)
    }

    fn empty_project() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    /// Drive one request line and unwrap the response.
    fn respond(server: &Server, line: &str) -> Value {
        server.handle_line(line).expect("a response is due")
    }

    #[test]
    fn initialize_echoes_a_supported_version_and_names_the_server() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        );
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "keel");
        assert_eq!(
            r["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(
            r["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("byte-identical")
        );
    }

    #[test]
    fn initialize_with_an_unknown_version_offers_the_latest_supported() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#,
        );
        assert_eq!(r["result"]["protocolVersion"], LATEST_PROTOCOL);
    }

    #[test]
    fn notifications_get_no_response() {
        let dir = empty_project();
        let s = server_in(dir.path());
        assert!(
            s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        // Unknown notifications are ignored too, per JSON-RPC 2.0.
        assert!(
            s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#)
                .is_none()
        );
    }

    #[test]
    fn parse_error_answers_with_null_id() {
        let dir = empty_project();
        let r = respond(&server_in(dir.path()), "{not json");
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert!(r["id"].is_null());
    }

    #[test]
    fn non_object_frames_are_invalid_requests() {
        let dir = empty_project();
        let r = respond(&server_in(dir.path()), "[1,2,3]");
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#,
        );
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(r["id"], 7);
    }

    #[test]
    fn ping_answers_an_empty_object() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
        );
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn tools_list_is_the_six_spec_tools_alphabetically() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
        );
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "explain_error",
                "get_doctor_report",
                "get_status",
                "get_trace",
                "list_flows",
                "propose_policy",
            ]
        );
        // Every tool declares an object input schema.
        for tool in r["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(tool["description"].as_str().unwrap().contains("--json"));
        }
    }

    #[test]
    fn unknown_tool_is_invalid_params() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_everything"}}"#,
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Available tools")
        );
    }

    #[test]
    fn missing_required_argument_is_invalid_params() {
        let dir = empty_project();
        let s = server_in(dir.path());
        let r = respond(
            &s,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_trace"}}"#,
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        assert!(r["error"]["message"].as_str().unwrap().contains("`flow`"));
        let r = respond(
            &s,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"explain_error","arguments":{}}}"#,
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        assert!(r["error"]["message"].as_str().unwrap().contains("`code`"));
    }

    #[test]
    fn mistyped_dead_argument_is_invalid_params_not_coerced() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"list_flows","arguments":{"dead":"yes"}}}"#,
        );
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        assert!(r["error"]["message"].as_str().unwrap().contains("boolean"));
    }

    #[test]
    fn explain_error_text_is_byte_identical_to_the_json_twin() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"explain_error","arguments":{"code":"KEEL-E014"}}}"#,
        );
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(
            r["result"]["content"][0]["text"].as_str().unwrap(),
            json_string(&explain::run("KEEL-E014").json)
        );
        assert_eq!(r["result"]["content"][0]["type"], "text");
    }

    #[test]
    fn a_failing_tool_is_a_result_with_is_error_not_a_protocol_error() {
        let dir = empty_project();
        let r = respond(
            &server_in(dir.path()),
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"explain_error","arguments":{"code":"KEEL-E999"}}}"#,
        );
        assert!(r.get("error").is_none(), "tool failures are not RPC errors");
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(
            r["result"]["content"][0]["text"].as_str().unwrap(),
            json_string(&explain::run("KEEL-E999").json)
        );
    }

    #[test]
    fn list_flows_defaults_dead_to_false_and_accepts_true() {
        let dir = empty_project();
        let s = server_in(dir.path());
        let r = respond(
            &s,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"list_flows"}}"#,
        );
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, json_string(&flows::flows(dir.path(), false, T0).json));
        let r = respond(
            &s,
            r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"list_flows","arguments":{"dead":true}}}"#,
        );
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, json_string(&flows::flows(dir.path(), true, T0).json));
    }

    #[test]
    fn serve_skips_blank_lines_and_exits_ok_on_eof() {
        let dir = empty_project();
        let script = "\n\
            {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\
            \n\
            {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let mut out = Vec::new();
        let code = server_in(dir.path()).serve(std::io::Cursor::new(script), &mut out);
        assert_eq!(code, EXIT_OK);
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(lines.len(), 1, "one request → one response line");
        let v: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["id"], 1);
    }
}
