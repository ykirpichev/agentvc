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
    #[error("{0} is not inside a git repository")]
    NotGitRepo(PathBuf),
    #[error("the space at {workdir} is in subdirectory {prefix:?} of its git repository; export needs the space at the repository root")]
    NotRepoRoot { workdir: PathBuf, prefix: String },
    #[error("invalid branch name {0:?}")]
    BadBranch(String),
    #[error("branch {0} already exists; pass --force to move it")]
    BranchExists(String),
    #[error("branch {0} is checked out in the host repository; export to another branch")]
    BranchCheckedOut(String),
    #[error("git {cmd} failed: {stderr}")]
    Git { cmd: String, stderr: String },
    #[error("cannot tell the GitHub repository from remote URL {0:?}; pass --repo owner/name")]
    BadRemote(String),
    #[error("no GitHub token: set GITHUB_TOKEN or GH_TOKEN, or sign in with `gh auth login`")]
    NoToken,
    #[error("GitHub API returned {status}: {message}")]
    GitHub { status: u16, message: String },
    #[error("the {backend} view backend is not available here: {reason}")]
    BackendUnavailable {
        backend: &'static str,
        reason: String,
    },
    #[error("the overlay view of attempt a{id} is not mounted and could not be mounted: {reason}")]
    ViewUnmounted { id: i64, reason: String },
    #[error(
        "rebasing a{attempt} onto {onto} conflicts in {}: {}; nothing was changed \
         (pass --keep-conflicts to write conflict markers into the view)",
        if paths.len() == 1 { "1 file".to_string() } else { format!("{} files", paths.len()) },
        paths.join(", ")
    )]
    Conflict {
        attempt: i64,
        onto: String,
        paths: Vec<String>,
    },
    #[error("the root attempt a1 is the host checkout; update it with git, not agentvc rebase")]
    RootAttempt,
    #[error(
        "{refname} on {remote} moved to {tip} since this space last synced (another machine \
         continued it); clone it with `agentvc init --from-remote` elsewhere, or pass --force \
         to overwrite"
    )]
    SyncConflict {
        refname: String,
        remote: String,
        tip: String,
    },
    #[error("several spaces match on the remote: {}; pass --space <id>", ids.join(", "))]
    AmbiguousSpace { ids: Vec<String> },
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
            Error::NotGitRepo(_) => "not_git_repo",
            Error::NotRepoRoot { .. } => "not_repo_root",
            Error::BadBranch(_) => "bad_branch",
            Error::BranchExists(_) => "branch_exists",
            Error::BranchCheckedOut(_) => "branch_checked_out",
            Error::Git { .. } => "git",
            Error::BadRemote(_) => "bad_remote",
            Error::NoToken => "no_token",
            Error::GitHub { .. } => "github",
            Error::BackendUnavailable { .. } => "backend_unavailable",
            Error::ViewUnmounted { .. } => "view_unmounted",
            Error::Conflict { .. } => "conflict",
            Error::RootAttempt => "root_attempt",
            Error::SyncConflict { .. } => "sync_conflict",
            Error::AmbiguousSpace { .. } => "ambiguous_space",
            Error::Io { .. } => "io",
            Error::Spawn { .. } => "spawn_failed",
            Error::Db(_) => "db",
            Error::Walk(_) => "walk",
        }
    }

    /// Structured details for errors that carry more than a message, merged
    /// into the `error` object of `--json` output.
    pub fn details(&self) -> Option<serde_json::Value> {
        match self {
            Error::Conflict {
                attempt,
                onto,
                paths,
            } => Some(serde_json::json!({
                "attempt": format!("a{attempt}"),
                "onto": onto,
                "paths": paths,
            })),
            Error::AmbiguousSpace { ids } => Some(serde_json::json!({ "spaces": ids })),
            Error::SyncConflict { refname, tip, .. } => {
                Some(serde_json::json!({ "ref": refname, "tip": tip }))
            }
            _ => None,
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
