//! Shared project-layout conventions and the discovery-evidence read.
//!
//! Keel keeps its state in `.keel/` next to the project's `keel.toml` (dx-spec
//! §3 — "state is files in `.keel/`"). The front ends write `.keel/discovery.db`
//! and `.keel/journal.db`; the CLI reads them. This module is the one place
//! those paths and the discovery read live, so `init`/`status`/`doctor` agree.

use std::path::{Path, PathBuf};

use keel_journal::{DiscoveryStore, SystemClock, TargetStats};

/// `<project>/keel.toml` — the policy file.
pub fn keel_toml(project: &Path) -> PathBuf {
    project.join("keel.toml")
}

/// `<project>/.keel/discovery.db` — the observed-traffic evidence.
pub fn discovery_db(project: &Path) -> PathBuf {
    project.join(".keel").join("discovery.db")
}

/// `<project>/.keel/journal.db` — flows, steps, and the persistent cache.
pub fn journal_db(project: &Path) -> PathBuf {
    project.join(".keel").join("journal.db")
}

/// Read observed per-target stats if `.keel/discovery.db` exists, else an empty
/// vec. A read clock never originates timestamps, so [`SystemClock`] is inert
/// here — determinism is preserved.
pub fn read_discovery(project: &Path) -> Result<Vec<TargetStats>, String> {
    let path = discovery_db(project);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let store = DiscoveryStore::open(&path, SystemClock)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    store
        .snapshot()
        .map_err(|e| format!("could not read {}: {e}", path.display()))
}
