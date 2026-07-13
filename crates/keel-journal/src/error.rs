//! The crate's error type.
//!
//! The journal is persistence, not policy, so its errors are narrow: either
//! a backend (SQLite or Postgres) failed, the dedicated worker thread behind
//! a Postgres connection is gone (it panicked mid-job — see
//! `postgres_journal`'s module doc for why every connection lives on its own
//! thread), or a row held a value outside the frozen schema's `CHECK` set
//! (which can only happen if a foreign tool corrupted the store). Mapping
//! into the `KEEL-E0NN` taxonomy is the caller's job, done where the journal
//! is wired into the engine — this layer stays taxonomy-free.

use core::fmt;

/// The result type used throughout this crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Something the journal could not do.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying SQLite backend failed (I/O, locking, constraint, …).
    Sqlite(rusqlite::Error),
    /// The underlying Postgres backend failed — connecting, a malformed
    /// connection URL, or a query/constraint failure.
    Postgres(postgres::Error),
    /// The dedicated worker thread that owns a pooled Postgres connection is
    /// gone — it exited (normally impossible while the pool is alive) or
    /// panicked while executing a previous job, before this call's job could
    /// be dispatched or its result received.
    WorkerUnavailable,
    /// A stored string fell outside the frozen schema's `CHECK` set, or an
    /// integer fell outside its domain type — i.e. the database is corrupt or
    /// was written by something that does not honour the contract.
    Corrupt {
        /// The column whose stored value could not be interpreted.
        column: &'static str,
        /// The offending value, rendered for the diagnostic.
        value: String,
    },
}

impl Error {
    pub(crate) fn corrupt(column: &'static str, value: impl fmt::Display) -> Self {
        Self::Corrupt {
            column,
            value: value.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "journal storage error: {e}"),
            Self::Postgres(e) => write!(f, "journal storage error: {e}"),
            Self::WorkerUnavailable => {
                write!(f, "journal connection worker thread is no longer running")
            }
            Self::Corrupt { column, value } => write!(
                f,
                "journal column `{column}` holds a value outside the schema contract: {value:?}"
            ),
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Postgres(e) => Some(e),
            Self::WorkerUnavailable | Self::Corrupt { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<postgres::Error> for Error {
    fn from(e: postgres::Error) -> Self {
        Self::Postgres(e)
    }
}
