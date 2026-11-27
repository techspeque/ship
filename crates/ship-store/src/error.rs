//! The store's typed error.

use std::fmt;

use sqlx::error::ErrorKind;

/// Why a store call failed.
///
/// Messages name the entity and field involved, never a stored value.
#[derive(Debug)]
pub enum StoreError {
    /// The database's schema version is newer than this binary knows. The
    /// database was not changed.
    SchemaTooNew { found: u32, supported: u32 },
    /// No row with the requested id.
    NotFound { entity: &'static str },
    /// A uniqueness or foreign-key constraint refused the write, or an
    /// idempotency key was replayed with a different request.
    Conflict { entity: &'static str },
    /// A schema check constraint refused the value, or the row contradicts
    /// another (a deployment under another environment's operation).
    Invalid { entity: &'static str },
    /// The change is not allowed from the row's current state: an operation
    /// phase moving backwards or leaving a terminal phase, a desired state
    /// leaving `removed`, a log cursor moving back, a deployment's result
    /// set twice, an audit event edited.
    IllegalTransition { entity: &'static str },
    /// A stored value does not parse as its domain type.
    Corrupt {
        entity: &'static str,
        field: &'static str,
    },
    /// The system clock is before 1970 or beyond what an id can hold.
    Clock,
    /// The operating system's random source failed.
    Entropy,
    /// File system error around the state directory.
    Io(std::io::Error),
    /// Any other database error.
    Database(sqlx::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::SchemaTooNew { found, supported } => write!(
                f,
                "database schema version {found} is newer than this binary supports ({supported}); upgrade ship"
            ),
            StoreError::NotFound { entity } => write!(f, "{entity} not found"),
            StoreError::Conflict { entity } => {
                write!(f, "{entity} conflicts with an existing record")
            }
            StoreError::Invalid { entity } => write!(f, "{entity} value rejected by the schema"),
            StoreError::IllegalTransition { entity } => {
                write!(f, "{entity} change not allowed from its current state")
            }
            StoreError::Corrupt { entity, field } => {
                write!(f, "stored {entity}.{field} is not a valid value")
            }
            StoreError::Clock => f.write_str("system clock is outside the supported range"),
            StoreError::Entropy => f.write_str("random source unavailable"),
            StoreError::Io(e) => write!(f, "state directory: {e}"),
            StoreError::Database(e) => write!(f, "database: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Io(e) => Some(e),
            StoreError::Database(e) => Some(e),
            _ => None,
        }
    }
}

/// Maps a database error from a read.
pub(crate) fn db(e: sqlx::Error) -> StoreError {
    StoreError::Database(e)
}

/// Maps a database error from a write on `entity`, classifying constraint
/// violations.
pub(crate) fn write_err(entity: &'static str) -> impl FnOnce(sqlx::Error) -> StoreError {
    move |e| {
        let kind = match &e {
            sqlx::Error::Database(d) => Some(d.kind()),
            _ => None,
        };
        match kind {
            Some(ErrorKind::UniqueViolation) | Some(ErrorKind::ForeignKeyViolation) => {
                StoreError::Conflict { entity }
            }
            Some(ErrorKind::CheckViolation) => StoreError::Invalid { entity },
            _ => match trigger_refusal(&e) {
                Some(Refusal::Illegal) => StoreError::IllegalTransition { entity },
                Some(Refusal::Invalid) => StoreError::Invalid { entity },
                None => StoreError::Database(e),
            },
        }
    }
}

pub(crate) fn not_found(entity: &'static str) -> StoreError {
    StoreError::NotFound { entity }
}

pub(crate) fn invalid(entity: &'static str) -> StoreError {
    StoreError::Invalid { entity }
}

pub(crate) fn illegal(entity: &'static str) -> StoreError {
    StoreError::IllegalTransition { entity }
}

enum Refusal {
    Illegal,
    Invalid,
}

/// Classifies a `RAISE(ABORT, ...)` from a schema trigger by its message
/// prefix (see `migrations/0002_phase1.sql`).
fn trigger_refusal(e: &sqlx::Error) -> Option<Refusal> {
    let sqlx::Error::Database(d) = e else {
        return None;
    };
    let msg = d.message();
    if msg.starts_with("illegal ") {
        Some(Refusal::Illegal)
    } else if msg.starts_with("invalid ") {
        Some(Refusal::Invalid)
    } else {
        None
    }
}
