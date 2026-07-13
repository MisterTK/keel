use keel_core_api::{ErrorCode, OutcomeError};
use std::fmt;

/// The error a `#[keel::wrap]`-wrapped function or [`crate::KeelMiddleware`]
/// surfaces.
///
/// `Original` is your own code's error, unchanged, from the attempt the
/// engine actually ran last (DX invariant 5's "re-raise the original
/// exception unchanged", ported to a typed enum since Rust has no untyped
/// exceptions to re-raise). `Keel` is a judgment the engine made *without*
/// ever calling your code: a circuit breaker fast-fail (`KEEL-E012`), a rate
/// budget rejection (`KEEL-E013`), or a non-idempotent call correctly left
/// un-retried (`KEEL-E014`) — see `contracts/error-codes.json` for the full
/// taxonomy.
#[derive(Debug)]
pub enum Error<E> {
    /// Your code/library's own error from the last attempt.
    Original(E),
    /// A judgment the engine made without calling your code.
    Keel(OutcomeError),
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Original(err) => write!(f, "{err}"),
            Error::Keel(err) => write!(f, "{}: {}", err.code, err.message),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for Error<E> {}

impl<E> Error<E> {
    /// The `KEEL-E0NN` code the engine assigned, if this is a [`Error::Keel`]
    /// judgment rather than your own code's [`Error::Original`] error.
    #[must_use]
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Error::Original(_) => None,
            Error::Keel(err) => Some(err.code),
        }
    }

    /// Recover your code/library's original error, if this wraps one — an
    /// engine judgment made without ever calling your code has none to give
    /// back.
    pub fn into_original(self) -> Option<E> {
        match self {
            Error::Original(err) => Some(err),
            Error::Keel(_) => None,
        }
    }
}
