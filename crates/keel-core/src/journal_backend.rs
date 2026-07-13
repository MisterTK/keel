//! Policy-selected journal backends (`policy.journal`, architecture spec §4.2:
//! the default `.keel/journal.db` is "overridable to a shared path or a backend
//! URL — that override is the entire laptop→enterprise migration").
//!
//! [`JournalBackend::select`] splits a schema-validated
//! [`JournalLocation`](keel_core_api::policy::JournalLocation) into one variant
//! per frozen scheme, and [`open`] is the single factory the engine calls at
//! configure time. **This is the seam a later slice extends**: the Postgres arm
//! of [`open`] returns a real backend once one exists. Until then it fails
//! loudly with `KEEL-E005` (unsupported-configuration — valid policy, missing
//! capability) instead of warning and journaling somewhere the user did not
//! ask for.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use keel_core_api::policy::JournalLocation;
use keel_core_api::{ErrorCode, KeelError};
use keel_journal::{Journal, SqliteJournal, SystemClock};

/// The exact diagnostic for a `postgres://` journal in a build with no Postgres
/// backend. Frozen as a message contract: the slice that ships the backend
/// replaces the error, not the wording.
pub(crate) const POSTGRES_UNAVAILABLE: &str =
    "Postgres journal not yet available in this build; use file: — see docs";

/// The backend kind a `journal` location names — one variant per scheme the
/// frozen policy schema admits (`^(file:.+|postgres://.+)$`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JournalBackend {
    /// `file:<path>` — SQLite at `<path>`, resolved to an absolute path.
    File(PathBuf),
    /// `postgres://…` — recognized and reserved; no backend in this build.
    /// The URL is deliberately not carried: it can embed credentials, and
    /// nothing in this build could use it anyway.
    Postgres,
}

impl JournalBackend {
    /// Classify a validated location and resolve its path. A relative `file:`
    /// path resolves against the process working directory *at selection
    /// (configure) time* — the project root under the front ends' run model —
    /// so the stored path is unambiguous however the engine later uses it.
    pub(crate) fn select(location: &JournalLocation) -> Self {
        match location.0.strip_prefix("file:") {
            Some(path) => Self::File(resolve_file_path(Path::new(path))),
            // `JournalLocation`'s validated grammar admits exactly two
            // schemes, so not-`file:` is `postgres://`.
            None => Self::Postgres,
        }
    }
}

/// Absolutize a `file:` payload: absolute paths pass through, relative paths
/// join the current working directory. If the working directory is unreadable
/// the relative path is kept — opening it will then resolve (or fail) against
/// whatever cwd the process has at that point.
fn resolve_file_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |cwd| cwd.join(path))
    }
}

/// Open the selected backend. The single factory `Engine::configure` calls,
/// and the one place a later slice adds the real Postgres store.
///
/// # Errors
/// - `KEEL-E005` for [`JournalBackend::Postgres`] — this build has no Postgres
///   backend (the [`POSTGRES_UNAVAILABLE`] message contract).
/// - `KEEL-E040` when the `file:` store cannot be created or opened.
pub(crate) fn open(backend: &JournalBackend) -> Result<Arc<dyn Journal>, KeelError> {
    match backend {
        JournalBackend::File(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|e| open_failed(path, &e))?;
            }
            let journal =
                SqliteJournal::open(path, SystemClock).map_err(|e| open_failed(path, &e))?;
            Ok(Arc::new(journal))
        }
        JournalBackend::Postgres => Err(KeelError {
            code: ErrorCode::UnsupportedConfiguration,
            message: POSTGRES_UNAVAILABLE.to_owned(),
        }),
    }
}

/// The diagnostic for a `file:` journal that could not be opened: what failed,
/// why, and what to do next. The path is a filesystem path (never a URL), so
/// including it leaks no credentials.
fn open_failed(path: &Path, cause: &dyn core::fmt::Display) -> KeelError {
    KeelError {
        code: ErrorCode::Internal,
        message: format!(
            "could not open the policy-selected journal at {}: {cause}. Check the path and \
             directory permissions, or drop the `journal` key to use the default \
             .keel/journal.db.",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(s: &str) -> JournalLocation {
        s.parse().expect("valid journal location")
    }

    /// The relative-path rule: a relative `file:` payload resolves against the
    /// process working directory at selection time; absolute paths pass through.
    #[test]
    fn file_paths_resolve_relative_to_cwd() {
        let JournalBackend::File(resolved) = JournalBackend::select(&location("file:rel/j.db"))
        else {
            panic!("file: selects the File backend");
        };
        assert!(resolved.is_absolute());
        assert_eq!(
            resolved,
            std::env::current_dir().unwrap().join("rel/j.db"),
            "relative file: paths join the current working directory"
        );

        let abs = if cfg!(windows) {
            PathBuf::from("C:\\keel\\j.db")
        } else {
            PathBuf::from("/var/keel/j.db")
        };
        let loc = location(&format!("file:{}", abs.display()));
        assert_eq!(JournalBackend::select(&loc), JournalBackend::File(abs));
    }

    #[test]
    fn postgres_selects_the_reserved_backend() {
        assert_eq!(
            JournalBackend::select(&location("postgres://user:secret@db.internal/keel")),
            JournalBackend::Postgres
        );
    }

    /// `Arc<dyn Journal>` has no `Debug`, so `unwrap_err` can't be used here.
    fn open_err(backend: &JournalBackend) -> KeelError {
        match open(backend) {
            Ok(_) => panic!("open unexpectedly succeeded"),
            Err(err) => err,
        }
    }

    /// The exact KEEL-E005 message contract for the missing Postgres backend.
    #[test]
    fn opening_postgres_fails_with_e005_and_the_frozen_message() {
        let err = open_err(&JournalBackend::Postgres);
        assert_eq!(err.code, ErrorCode::UnsupportedConfiguration);
        assert_eq!(err.code.as_str(), "KEEL-E005");
        assert_eq!(
            err.message,
            "Postgres journal not yet available in this build; use file: — see docs"
        );
    }

    /// The file arm creates missing parent directories, then opens SQLite there.
    #[test]
    fn opening_a_file_backend_creates_parent_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("deeper").join("journal.db");
        let journal = open(&JournalBackend::File(path.clone())).expect("open creates dirs");
        drop(journal);
        assert!(path.exists(), "journal file created at the selected path");
    }

    /// An unopenable path fails loudly (KEEL-E040) with the path in the message.
    #[test]
    fn an_unopenable_file_path_is_a_loud_e040() {
        let dir = tempfile::TempDir::new().unwrap();
        // A directory at the target path makes SQLite's open fail.
        let path = dir.path().join("journal.db");
        std::fs::create_dir_all(&path).unwrap();
        let err = open_err(&JournalBackend::File(path.clone()));
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(err.message.contains(&path.display().to_string()));
        assert!(err.message.contains("journal"));
    }
}
