//! Runtime proof that [`keel::KeelMiddleware`] retries a transient response
//! per a real `keel.toml` fixture, against a minimal hand-rolled HTTP/1.1
//! server over `std::net::TcpListener` (no new mock-http-server dependency —
//! per the task brief, this is cheaper than pulling one in for three
//! canned responses).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/middleware")
}

fn ensure_init() {
    if !keel::is_initialized() {
        keel::init_from(fixture_dir()).expect("fixture keel.toml is valid");
    }
}

/// Spawns a background thread that replies to `responses.len()` connections,
/// one raw HTTP/1.1 response each (in order), then stops accepting. Returns
/// the bound port and a counter of accepted connections (for assertions).
fn spawn_mock_server(responses: Vec<&'static str>) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_for_thread = Arc::clone(&accepted);
    std::thread::spawn(move || {
        for resp in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            accepted_for_thread.fetch_add(1, Ordering::SeqCst);
            // Don't bother parsing the request — read whatever's pending and
            // reply. `Connection: close` on every response forces reqwest to
            // open a fresh connection per attempt, so each retry is its own
            // `accept()`.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            stream.write_all(resp.as_bytes()).expect("write response");
            let _ = stream.flush();
        }
    });
    (port, accepted)
}

const RESP_503: &str =
    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const RESP_200_OK: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
const RESP_404: &str = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

#[tokio::test]
async fn retries_a_transient_503_then_succeeds() {
    ensure_init();
    let (port, accepted) = spawn_mock_server(vec![RESP_503, RESP_503, RESP_200_OK]);

    let raw = reqwest::Client::new();
    let client = reqwest_middleware::ClientBuilder::new(raw.clone())
        .with(keel::KeelMiddleware::new(raw))
        .build();

    let resp = client
        .get(format!("http://127.0.0.1:{port}/orders"))
        .send()
        .await
        .expect("eventually succeeds");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
    // Give the server thread a moment to record the final accept (the client
    // future can resolve a hair before the counter increments).
    for _ in 0..50 {
        if accepted.load(Ordering::SeqCst) == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        3,
        "two 503s then the 200: three attempts"
    );
}

#[tokio::test]
async fn a_non_transient_4xx_is_never_retried_and_is_returned_not_raised() {
    ensure_init();
    let (port, accepted) = spawn_mock_server(vec![RESP_404]);

    let raw = reqwest::Client::new();
    let client = reqwest_middleware::ClientBuilder::new(raw.clone())
        .with(keel::KeelMiddleware::new(raw))
        .build();

    let resp = client
        .get(format!("http://127.0.0.1:{port}/missing"))
        .send()
        .await
        .expect("a 404 is a successful HTTP exchange, not a middleware error");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    for _ in 0..50 {
        if accepted.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        1,
        "a plain 404 is not retried"
    );
}

#[tokio::test]
async fn exhausting_retries_on_a_persistent_503_still_returns_the_response_not_an_error() {
    ensure_init();
    // Policy allows 3 attempts; all three see a 503.
    let (port, accepted) = spawn_mock_server(vec![RESP_503, RESP_503, RESP_503]);

    let raw = reqwest::Client::new();
    let client = reqwest_middleware::ClientBuilder::new(raw.clone())
        .with(keel::KeelMiddleware::new(raw))
        .build();

    let resp = client
        .get(format!("http://127.0.0.1:{port}/orders"))
        .send()
        .await
        .expect("a real (if unhappy) HTTP response, never a middleware Err");
    assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    for _ in 0..50 {
        if accepted.load(Ordering::SeqCst) == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        3,
        "retried until attempts exhausted"
    );
}
