use std::path::PathBuf;

/// Errors returned by agentvc-core. Every variant has a stable machine-readable
/// [`Error::code`] so agents can branch on it without parsing messages.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no agentvc space found at or above {0}")]
    NoSpace(PathBuf),
    #[error("an agentvc space already exists at {0}")]
    SpaceExists(PathBuf),
    #[error("{kind} {id} not found")]
    NotFound { kind: &'static str, id: String },
    #[error("invalid reference {0:?}: expected aN (attempt) or sN (snapshot)")]
    BadRef(String),
    #[error("attempt a{id} is {status}; this needs an open attempt")]
    AttemptClosed { id: i64, status: String },
    #[error("object {0} is corrupt or has an unexpected type")]
    BadObject(String),
    #[error("path {0} is not inside any attempt view")]
    NotInView(PathBuf),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not start {program:?}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("walk error: {0}")]
    Walk(#[from] ignore::Error),
}

impl Error {
    /// Stable error code, safe to match on from scripts and agents.
    pub fn code(&self) -> &'static str {
        match self {
            Error::NoSpace(_) => "no_space",
            Error::SpaceExists(_) => "space_exists",
            Error::NotFound { .. } => "not_found",
            Error::BadRef(_) => "bad_ref",
            Error::AttemptClosed { .. } => "attempt_closed",
            Error::BadObject(_) => "bad_object",
            Error::NotInView(_) => "not_in_view",
            Error::Io { .. } => "io",
            Error::Spawn { .. } => "spawn_failed",
            Error::Db(_) => "db",
            Error::Walk(_) => "walk",
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Attach a path to an `std::io::Error`.
pub(crate) trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }
}
