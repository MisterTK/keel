//! `keel report --serve` — the live page over a `127.0.0.1` listener
//! (design spec 2026-09-04, Part B). Two routes: `/` (the page, whose
//! script polls) and `/api/state?since=<seq>` (the blob, events after the
//! cursor). A hand-rolled HTTP/1.1 responder over `std::net`: request line
//! and headers in, one response out, `Connection: close`. Read-only,
//! loopback only, no other route exists — there is no path-traversal surface.
//!
//! It is a foreground command like `keel tail`: the accept loop is
//! non-blocking so it can notice the Ctrl-C flag and return within 50ms.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::render::{json_string, to_json};
use crate::report::{self, Mode, ReportOptions, no_evidence};
use crate::{EXIT_FAILURE, Rendered, report_html};

/// The largest request head we read before answering; anything bigger is
/// not a browser asking for the page.
const MAX_HEAD: usize = 8 * 1024;

/// One HTTP response, built by [`handle_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Response {
    fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            body,
        }
    }
    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.to_owned(),
        }
    }
}

/// Route one request line. Pure apart from reading the evidence files, so
/// it is unit-testable without a socket.
pub fn handle_request(project: &Path, now_ms: i64, request_line: &str) -> Response {
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Response::text(400, "bad request\n");
    };
    if method != "GET" {
        return Response::text(405, "method not allowed\n");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" | "/index.html" => match report::assemble(project, now_ms, Mode::Serve, 0, None) {
            Ok(Some(data)) => Response {
                status: 200,
                content_type: "text/html; charset=utf-8",
                body: report_html::render(&data),
            },
            Ok(None) => Response::json(503, "{\"error\":\"no evidence yet\"}\n".to_owned()),
            Err(r) => Response::json(500, json_string(&r.json)),
        },
        "/api/state" => {
            let since = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("since="))
                .and_then(|v| v.parse::<u64>().ok());
            match report::assemble(project, now_ms, Mode::Serve, 0, since) {
                Ok(Some(data)) => Response::json(200, json_string(&to_json(&data))),
                Ok(None) => Response::json(503, "{\"error\":\"no evidence yet\"}\n".to_owned()),
                Err(r) => Response::json(500, json_string(&r.json)),
            }
        }
        _ => Response::text(404, "not found\n"),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        421 => "Misdirected Request",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// Read the request head, answer, close. Every error here is the client's
/// problem, never the server's — a broken connection is simply dropped.
fn handle_connection(mut stream: TcpStream, project: &Path, now_ms: i64) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                head.extend_from_slice(&chunk[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > MAX_HEAD {
                    break;
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&head);
    let request_line = text.lines().next().unwrap_or("");
    // The listener is loopback-only, but a browser on this machine can be
    // steered to it by DNS rebinding (`attacker.example` resolving to
    // 127.0.0.1 on a later lookup) and then read the blob same-origin. The
    // `Host` header is the one thing such a page cannot forge, so only
    // loopback spellings are served; anything else is misdirected.
    let resp = if host_is_loopback(&text) {
        handle_request(project, now_ms, request_line)
    } else {
        Response {
            status: 421,
            content_type: "text/plain; charset=utf-8",
            body: "keel report --serve answers only 127.0.0.1 / localhost\n".to_owned(),
        }
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(resp.body.as_bytes());
    let _ = stream.flush();
}

/// `true` when the request head carries a `Host` header naming this machine:
/// `127.0.0.1`, `localhost`, or `[::1]`, each with an optional `:port`.
/// A missing `Host` is refused too — every browser sends one.
fn host_is_loopback(head: &str) -> bool {
    let Some(value) = head
        .lines()
        .skip(1)
        .take_while(|l| !l.is_empty())
        .find_map(|l| {
            l.split_once(':')
                .filter(|(name, _)| name.trim().eq_ignore_ascii_case("host"))
                .map(|(_, v)| v.trim())
        })
    else {
        return false;
    };
    let lower = value.to_ascii_lowercase();
    let host = if let Some(rest) = lower.strip_prefix("[::1]") {
        // Bracketed IPv6 keeps its brackets; only an optional port follows.
        if rest.is_empty() || rest.starts_with(':') {
            "[::1]"
        } else {
            return false;
        }
    } else {
        lower.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// Bind loopback only (`port` 0 = ephemeral); the listener is non-blocking
/// so [`serve_on`] can poll the stop flag between accepts.
pub fn bind(port: u16) -> io::Result<TcpListener> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Accept until `stop` is set. One thread per connection; `now` is read on
/// the accept thread so each response is dated at accept time.
///
/// Takes `listener`/`project` by value rather than by reference: callers
/// (notably [`run_serve`] and the socket test) hand the listener off to run
/// here for the rest of the process's life, and `project` is cloned once per
/// accepted connection regardless — owning it up front documents that this
/// call takes over the listener rather than merely borrowing it.
#[allow(clippy::needless_pass_by_value)]
pub fn serve_on(
    listener: TcpListener,
    project: PathBuf,
    now: impl Fn() -> i64,
    stop: &AtomicBool,
) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let now_ms = now();
                let project = project.clone();
                std::thread::spawn(move || handle_connection(stream, &project, now_ms));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

/// `--serve`: refuse to start with no evidence (the same nudge as the
/// static mode), bind, announce the URL, optionally open it, then serve
/// until Ctrl-C.
pub fn run_serve(
    project: &Path,
    opts: &ReportOptions,
    now: impl Fn() -> i64,
    stop: &AtomicBool,
    out: &mut dyn Write,
) -> Result<(), Rendered> {
    if report::assemble(project, now(), Mode::Serve, 0, None)?.is_none() {
        return Err(no_evidence());
    }
    let listener = bind(opts.port).map_err(|e| bind_error(opts.port, &e))?;
    let port = listener.local_addr().map_or(opts.port, |a| a.port());
    let url = format!("http://127.0.0.1:{port}/");
    let _ = writeln!(
        out,
        "keel \u{25b8} serving the live report at {url} (Ctrl-C to stop)"
    );
    if opts.open && !report::open_in_browser(&url) {
        let _ = writeln!(out, "  (could not launch a browser; open the URL yourself)");
    }
    serve_on(listener, project.to_path_buf(), now, stop).map_err(|e| serve_error(&e))
}

fn bind_error(port: u16, error: &io::Error) -> Rendered {
    #[derive(serde::Serialize)]
    struct Err {
        error: String,
        port: u16,
    }
    let message = format!("could not bind 127.0.0.1:{port}: {error}");
    Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&Err {
            error: message,
            port,
        }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}

fn serve_error(error: &io::Error) -> Rendered {
    #[derive(serde::Serialize)]
    struct Err {
        error: String,
    }
    let message = format!("serve loop failed: {error}");
    Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&Err { error: message }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}
