//! Shared project-layout conventions and the discovery-evidence read.
//!
//! Keel keeps its state in `.keel/` next to the project's `keel.toml` (dx-spec
//! §3 — "state is files in `.keel/`"). The front ends write `.keel/discovery.db`
//! and `.keel/journal.db`; the CLI reads them. This module is the one place
//! those paths, the policy `journal` resolution, and the discovery read live,
//! so `init`/`status`/`doctor`/`flows`/`trace` agree.

use std::path::{Path, PathBuf};

use keel_core_api::policy::{JournalLocation, Policy};
use keel_journal::{DailyStats, DiscoveryStore, SystemClock, TargetStats};

/// `<project>/keel.toml` — the policy file.
pub fn keel_toml(project: &Path) -> PathBuf {
    project.join("keel.toml")
}

/// `<project>/.keel/discovery.db` — the observed-traffic evidence.
pub fn discovery_db(project: &Path) -> PathBuf {
    project.join(".keel").join("discovery.db")
}

/// `<project>/.keel/journal.db` — the *default* journal location. Evidence
/// readers should go through [`resolved_journal`], which honors the policy's
/// `journal` key and falls back to this.
pub fn journal_db(project: &Path) -> PathBuf {
    project.join(".keel").join("journal.db")
}

/// Which backend a journal location names (architecture spec §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalBackendKind {
    /// The default `.keel/journal.db` or a `file:` location.
    Sqlite,
    /// A `postgres://` location — reserved; no backend exists in this build,
    /// so the app fails to configure with KEEL-E005.
    Postgres,
}

impl JournalBackendKind {
    /// The stable lowercase name reports print (`"sqlite"` / `"postgres"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }
}

/// The journal location resolved for a project: `keel.toml`'s `journal` key
/// when present and parseable, else the `.keel/journal.db` default — the same
/// selection the engine makes at configure time, with one framing difference:
/// relative `file:` paths resolve against the *project* directory here (the
/// CLI inspects a project) and against the process working directory there;
/// identical whenever the app runs at the project root, the front ends' run
/// model.
#[derive(Debug, Clone)]
pub struct ResolvedJournal {
    pub backend: JournalBackendKind,
    /// The SQLite file evidence readers open. For a `postgres://` location
    /// (no local reader in this build) this falls back to the default path so
    /// `status`/`flows` stay functional; `doctor` carries the
    /// unsupported-backend finding.
    pub path: PathBuf,
    /// The user-facing location: a `file:` payload as written, the default
    /// relative path, or a credential-redacted `postgres://` form.
    pub display: String,
    /// True when the location came from `keel.toml` (vs the built-in default).
    pub from_policy: bool,
}

/// Resolve where `project`'s journal lives (see [`ResolvedJournal`]).
pub fn resolved_journal(project: &Path) -> ResolvedJournal {
    policy_journal(project).map_or_else(
        || ResolvedJournal {
            backend: JournalBackendKind::Sqlite,
            path: journal_db(project),
            display: ".keel/journal.db".to_owned(),
            from_policy: false,
        },
        |location| from_location(project, &location),
    )
}

fn from_location(project: &Path, location: &JournalLocation) -> ResolvedJournal {
    match location.0.strip_prefix("file:") {
        Some(raw) => {
            let file = Path::new(raw);
            let path = if file.is_absolute() {
                file.to_owned()
            } else {
                project.join(file)
            };
            ResolvedJournal {
                backend: JournalBackendKind::Sqlite,
                path,
                display: raw.to_owned(),
                from_policy: true,
            }
        }
        // The validated grammar admits exactly `file:` and `postgres://`.
        None => ResolvedJournal {
            backend: JournalBackendKind::Postgres,
            path: journal_db(project),
            display: redact_postgres(&location.0),
            from_policy: true,
        },
    }
}

/// Lenient read of `keel.toml`'s `journal` key: any read/parse failure is
/// `None` (the default applies) — `doctor` reports policy validity separately,
/// and `status`/`flows` must keep reading evidence past a broken policy.
fn policy_journal(project: &Path) -> Option<JournalLocation> {
    let text = std::fs::read_to_string(keel_toml(project)).ok()?;
    let toml_value: toml::Value = text.parse().ok()?;
    let json = serde_json::to_value(&toml_value).ok()?;
    let policy: Policy = serde_json::from_value(json).ok()?;
    policy.journal
}

/// Drop the userinfo (credentials) from a `postgres://` URL for display —
/// doctor output gets pasted into issues, and a journal URL can embed a
/// password.
fn redact_postgres(url: &str) -> String {
    let Some(rest) = url.strip_prefix("postgres://") else {
        return url.to_owned();
    };
    match rest.split_once('@') {
        // Only redact an '@' inside the authority (before any path slash).
        Some((userinfo, tail)) if !userinfo.contains('/') => format!("postgres://\u{2026}@{tail}"),
        _ => url.to_owned(),
    }
}

/// Read observed per-target stats if `.keel/discovery.db` exists, else an empty
/// vec. A read clock never originates timestamps, so [`SystemClock`] is inert
/// here — determinism is preserved.
pub fn read_discovery(project: &Path) -> Result<Vec<TargetStats>, String> {
    let path = discovery_db(project);
    if !path.exists() {
        return Ok(Vec::new());
    }
    // Read-only open: `status`/`init`/`doctor` only read evidence, so they must
    // not take a write lock or create/mutate the file — that lets them run from a
    // read-only checkout or mounted volume.
    let store = DiscoveryStore::open_readonly(&path, SystemClock)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    store
        .snapshot()
        .map_err(|e| format!("could not read {}: {e}", path.display()))
}

/// Read the rolling daily buckets if `.keel/discovery.db` exists, else an empty
/// vec (and, on a legacy v1 file, [`DiscoveryStore::daily_snapshot`] itself
/// returns empty — there is no bucket table to read). This is what turns
/// lifetime `discovery` totals into a real trailing-window answer ("retries
/// saved this week") in `keel status`.
pub fn read_discovery_daily(project: &Path) -> Result<Vec<DailyStats>, String> {
    let path = discovery_db(project);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let store = DiscoveryStore::open_readonly(&path, SystemClock)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    store
        .daily_snapshot()
        .map_err(|e| format!("could not read {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with_policy(toml: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("keel.toml"), toml).unwrap();
        dir
    }

    #[test]
    fn no_policy_resolves_to_the_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = resolved_journal(dir.path());
        assert_eq!(r.backend, JournalBackendKind::Sqlite);
        assert_eq!(r.path, dir.path().join(".keel").join("journal.db"));
        assert_eq!(r.display, ".keel/journal.db");
        assert!(!r.from_policy);
    }

    #[test]
    fn relative_file_location_resolves_against_the_project() {
        let dir = project_with_policy("journal = \"file:custom/j.db\"\n");
        let r = resolved_journal(dir.path());
        assert_eq!(r.backend, JournalBackendKind::Sqlite);
        assert_eq!(r.path, dir.path().join("custom").join("j.db"));
        assert_eq!(r.display, "custom/j.db");
        assert!(r.from_policy);
    }

    #[test]
    fn absolute_file_location_passes_through() {
        let target = tempfile::TempDir::new().unwrap();
        let abs = target.path().join("j.db");
        let dir = project_with_policy(&format!("journal = \"file:{}\"\n", abs.display()));
        let r = resolved_journal(dir.path());
        assert_eq!(r.path, abs);
        assert_eq!(r.display, abs.display().to_string());
    }

    #[test]
    fn postgres_location_is_reported_with_credentials_redacted() {
        let dir =
            project_with_policy("journal = \"postgres://keel:sekrit@db.internal:5432/keel\"\n");
        let r = resolved_journal(dir.path());
        assert_eq!(r.backend, JournalBackendKind::Postgres);
        assert!(r.from_policy);
        assert_eq!(r.display, "postgres://\u{2026}@db.internal:5432/keel");
        assert!(!r.display.contains("sekrit"), "credentials never printed");
        // Evidence readers fall back to the default file (no local pg reader).
        assert_eq!(r.path, dir.path().join(".keel").join("journal.db"));
    }

    #[test]
    fn a_broken_policy_falls_back_to_the_default() {
        let dir = project_with_policy("journal = [this is not toml\n");
        let r = resolved_journal(dir.path());
        assert_eq!(r.backend, JournalBackendKind::Sqlite);
        assert!(!r.from_policy);
        assert_eq!(r.display, ".keel/journal.db");
    }
}
