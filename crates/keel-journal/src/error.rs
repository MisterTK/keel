//! The crate's error type.
//!
//! The journal is persistence, not policy, so its errors are narrow: either
//! the SQLite layer failed, or a row held a value outside the frozen schema's
//! `CHECK` set (which can only happen if a foreign tool corrupted the file).
//! Mapping into the `KEEL-E0NN` taxonomy is the caller's job, done where the
//! journal is wired into the engine — this layer stays taxonomy-free.

use core::fmt;

/// The result type used throughout this crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Something the journal could not do.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying SQLite backend failed (I/O, locking, constraint, …).
    Sqlite(rusqlite::Error),
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
            Self::Corrupt { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}
