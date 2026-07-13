//! Policy-selected journal backends (`policy.journal`, architecture spec §4.2:
//! the default `.keel/journal.db` is "overridable to a shared path or a backend
//! URL — that override is the entire laptop→enterprise migration").
//!
//! [`JournalBackend::select`] splits a schema-validated
//! [`JournalLocation`](keel_core_api::policy::JournalLocation) into one variant
//! per frozen scheme, and [`open`] is the single factory the engine calls at
//! configure time: `file:` opens (or creates) a [`SqliteJournal`], and
//! `postgres://` opens (pooled) a [`PostgresJournal`] — the Level 3/fleet
//! backend (architecture spec §6).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use keel_core_api::policy::JournalLocation;
use keel_core_api::{ErrorCode, KeelError};
use keel_journal::{Journal, PostgresJournal, SqliteJournal, SystemClock};

/// The backend kind a `journal` location names — one variant per scheme the
/// frozen policy schema admits (`^(file:.+|postgres://.+)$`).
///
/// `Postgres` carries the full connection URL (needed to actually connect),
/// but nothing in this module ever puts it in a log line or an error message
/// verbatim — see [`redact_postgres_url`] — since it can embed credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JournalBackend {
    /// `file:<path>` — SQLite at `<path>`, resolved to an absolute path.
    File(PathBuf),
    /// `postgres://…` — the full connection URL, as written in policy.
    Postgres(String),
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
            None => Self::Postgres(location.0.clone()),
        }
    }
}

/// Mask credentials in a `postgres://` URL for logging/error text: everything
/// between `://` and the last `@` (the `user:password@` segment, if any)
/// becomes `***`. Used instead of ever formatting the raw URL a caller handed
/// us, since [`open`]'s failure path is the one place this build would
/// otherwise be tempted to echo it back for diagnosis.
fn redact_postgres_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return "postgres://<malformed>".to_owned();
    };
    let (scheme, rest) = url.split_at(scheme_end);
    let after_scheme = &rest[3..];
    after_scheme.rfind('@').map_or_else(
        || url.to_owned(),
        |at| format!("{scheme}://***@{}", &after_scheme[at + 1..]),
    )
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

/// Open the selected backend. The single factory `Engine::configure` calls.
///
/// # Errors
/// - `KEEL-E040` when the `file:` store cannot be created or opened, or the
///   `postgres://` store cannot be connected to (the URL is malformed,
///   unreachable, or the schema batch fails).
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
        JournalBackend::Postgres(url) => {
            let journal = PostgresJournal::open(url).map_err(|e| postgres_open_failed(url, &e))?;
            Ok(Arc::new(journal))
        }
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

/// The diagnostic for a `postgres://` journal that could not be opened: what
/// failed and why, with the URL credential-redacted ([`redact_postgres_url`])
/// so the failing location can be identified without echoing a password back
/// into logs or a CLI's stderr.
fn postgres_open_failed(url: &str, cause: &dyn core::fmt::Display) -> KeelError {
    KeelError {
        code: ErrorCode::Internal,
        message: format!(
            "could not open the policy-selected journal at {}: {cause}. Check the connection \
             string, that the server is reachable, and that the connecting role can create \
             tables, or drop the `journal` key to use the default .keel/journal.db.",
            redact_postgres_url(url)
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
    fn postgres_selects_the_backend_carrying_its_url() {
        assert_eq!(
            JournalBackend::select(&location("postgres://user:secret@db.internal/keel")),
            JournalBackend::Postgres("postgres://user:secret@db.internal/keel".to_owned())
        );
    }

    /// `Arc<dyn Journal>` has no `Debug`, so `unwrap_err` can't be used here.
    fn open_err(backend: &JournalBackend) -> KeelError {
        match open(backend) {
            Ok(_) => panic!("open unexpectedly succeeded"),
            Err(err) => err,
        }
    }

    /// A malformed `postgres://` location fails loudly (KEEL-E040, same
    /// taxonomy slot as an unopenable `file:` path) without ever attempting a
    /// connection (so this test has no network dependency and cannot hang on
    /// a connect timeout) and without echoing the URL's credentials back.
    #[test]
    fn opening_postgres_with_a_malformed_location_fails_loudly_without_leaking_credentials() {
        let err = open_err(&JournalBackend::Postgres(
            "postgres://user:s3cr3t@[not-a-valid-host/keel".to_owned(),
        ));
        assert_eq!(err.code, ErrorCode::Internal);
        assert_eq!(err.code.as_str(), "KEEL-E040");
        assert!(
            !err.message.contains("s3cr3t"),
            "credentials must be redacted"
        );
    }

    #[test]
    fn redact_postgres_url_masks_only_the_credential_segment() {
        assert_eq!(
            redact_postgres_url("postgres://user:secret@db.internal:5432/keel"),
            "postgres://***@db.internal:5432/keel"
        );
        // No credentials to mask: passed through unchanged.
        assert_eq!(
            redact_postgres_url("postgres://db.internal/keel"),
            "postgres://db.internal/keel"
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
