//! A scratch Postgres cluster for `flows_conformance_postgres.rs`: `initdb` +
//! `pg_ctl` against a throwaway data directory on an unused port, stopped and
//! deleted on [`Drop`].
//!
//! This is a plain module (`tests/support/mod.rs`, not `tests/support.rs`) so
//! Cargo does not treat it as its own integration-test binary.
//!
//! A near-identical copy lives at `crates/keel-journal/tests/support/mod.rs`
//! for that crate's own `PostgresJournal` integration tests; it is not shared
//! across the crate boundary (integration-test binaries in different crates
//! cannot import each other's `tests/` modules), so this one is kept in sync
//! by hand — it is a small, self-contained process harness unlikely to drift.
//!
//! Requires `initdb`/`pg_ctl` on `PATH`, at `KEEL_PG_BIN` (a directory), or at
//! the Homebrew `postgresql@15` keg this repo's dev docs reference. Tests
//! using [`ScratchPg::start`] skip (print and return, never panic or fail)
//! when none of those resolve, so `cargo test` stays green on a machine (or
//! CI image) with no local Postgres.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

/// Ask the OS for a free ephemeral TCP port by binding to port 0 and
/// immediately releasing it. Picking a *fixed* port (even a high, "unlikely"
/// one) is not safe on this shared machine: other worktrees/agents may run
/// the same scratch-cluster harness concurrently and collide on it. There is
/// a small bind-race window between releasing the listener and `pg_ctl`
/// binding the same port, but it is far narrower than a fixed-port collision
/// across independent processes.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local_addr of the ephemeral listener")
        .port()
}

/// Resolve one Postgres server binary: `KEEL_PG_BIN/<tool>` if set, else the
/// Homebrew `postgresql@15` keg, else a bare `PATH` lookup.
fn pg_bin(tool: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("KEEL_PG_BIN") {
        return PathBuf::from(dir).join(tool);
    }
    let homebrew = PathBuf::from("/opt/homebrew/opt/postgresql@15/bin").join(tool);
    if homebrew.exists() {
        return homebrew;
    }
    PathBuf::from(tool)
}

/// A running scratch cluster: one throwaway data directory, one unused port,
/// `trust` auth (no password) for a `keeltest` superuser. Connect with
/// [`ScratchPg::url`]. Stopped (`pg_ctl -m fast stop`) and its data directory
/// deleted when dropped.
pub struct ScratchPg {
    dir: TempDir,
    port: u16,
}

impl ScratchPg {
    /// Start a fresh cluster. Returns `None` — never panics — when `initdb`
    /// cannot be found or run, so callers skip the test instead of failing it
    /// on a machine with no local Postgres.
    #[must_use]
    pub fn start() -> Option<Self> {
        let initdb = pg_bin("initdb");
        let pg_ctl = pg_bin("pg_ctl");
        if Command::new(&initdb).arg("--version").output().is_err() {
            return None;
        }

        let dir = TempDir::new().expect("tempdir for scratch postgres");
        let data_dir = dir.path().join("data");

        let init = Command::new(&initdb)
            .arg("-D")
            .arg(&data_dir)
            .args(["-U", "keeltest", "-A", "trust", "--no-sync"])
            .output()
            .expect("run initdb");
        assert!(
            init.status.success(),
            "initdb failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        // Retry on a fresh port a couple of times: `free_port` has a small
        // bind-race window against whatever else is running on this shared
        // machine, so a single `pg_ctl start` failure isn't necessarily this
        // cluster's fault.
        let mut last_err = String::new();
        for _ in 0..3 {
            let port = free_port();
            let start = Command::new(&pg_ctl)
                .arg("-D")
                .arg(&data_dir)
                .arg("-o")
                .arg(format!("-p {port} -h 127.0.0.1 -k {}", data_dir.display()))
                .arg("-w")
                .arg("-l")
                .arg(dir.path().join("log"))
                .arg("start")
                .output()
                .expect("run pg_ctl start");
            if start.status.success() {
                return Some(Self { dir, port });
            }
            last_err = String::from_utf8_lossy(&start.stderr).into_owned();
        }
        panic!("pg_ctl start failed after retries: {last_err}");
    }

    /// The `postgres://` URL for this cluster's default `postgres` database.
    #[must_use]
    pub fn url(&self) -> String {
        format!("postgres://keeltest@127.0.0.1:{}/postgres", self.port)
    }
}

impl Drop for ScratchPg {
    fn drop(&mut self) {
        let pg_ctl = pg_bin("pg_ctl");
        let _ = Command::new(&pg_ctl)
            .arg("-D")
            .arg(self.dir.path().join("data"))
            .arg("-m")
            .arg("fast")
            .arg("stop")
            .output();
    }
}
