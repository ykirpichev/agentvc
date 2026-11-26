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
    #[error("branch {branch} is checked out and its working tree has changes that the export would overwrite; commit or stash them first: {stderr}")]
    CheckoutDirty { branch: String, stderr: String },
    #[error("the remote {remote} refused the push of branch {branch}: this branch is not allowed for these credentials (push to the branch you may write to, e.g. with --branch or `agentvc config pr.branch`): {stderr}")]
    PushNotAllowed {
        remote: String,
        branch: String,
        stderr: String,
    },
    #[error("git {cmd} failed: {stderr}")]
    Git { cmd: String, stderr: String },
    #[error("unknown config key {0:?}; known keys: {keys}", keys = crate::config::KEYS.iter().map(|k| k.name).collect::<Vec<_>>().join(", "))]
    BadConfigKey(String),
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
        "taking from a{source_attempt} into a{attempt} conflicts in {}: {}; nothing was changed \
         (pass --keep-conflicts to write conflict markers into the view)",
        if paths.len() == 1 { "1 file".to_string() } else { format!("{} files", paths.len()) },
        paths.join(", ")
    )]
    TakeConflict {
        attempt: i64,
        source_attempt: i64,
        paths: Vec<String>,
    },
    #[error("nothing to take from a{source_attempt}: {reason}")]
    NothingToTake { source_attempt: i64, reason: String },
    #[error("invalid --hunks {spec:?}: {reason}")]
    BadHunk { spec: String, reason: String },
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
    #[error("the tree of snapshot s{0} was pruned by `agentvc gc`; its metadata is kept, its files are gone")]
    TreePruned(i64),
    #[error("another `agentvc gc` is running in this space")]
    GcBusy,
    #[error("the space doesn't fit in a PR comment even without file contents ({0} characters)")]
    BundleTooLarge(usize),
    #[error("the space was not saved: push: {git}; PR comment: {pr}")]
    NotSaved { git: String, pr: String },
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("operation {op} ({kind}) cannot be undone")]
    NotUndoable { op: i64, kind: String },
    #[error("operation {op} was already undone by operation {by}; `agentvc undo {by}` redoes it")]
    AlreadyUndone { op: i64, by: i64 },
    #[error("cannot undo operation {op}: {reason}; nothing was changed")]
    UndoConflict { op: i64, reason: String },
    #[error(
        "applying would overwrite local changes in {}: {}; commit or stash them, \
         limit it with --paths, or pass --force",
        if paths.len() == 1 { "1 file".to_string() } else { format!("{} files", paths.len()) },
        paths.join(", ")
    )]
    DirtyWorktree { paths: Vec<String> },
    #[error("invalid tag name {name:?}: {reason}")]
    BadTagName { name: String, reason: &'static str },
    #[error("tag {0:?} already exists; delete it first with `agentvc tag -d {0}`")]
    TagExists(String),
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
            Error::CheckoutDirty { .. } => "checkout_dirty",
            Error::PushNotAllowed { .. } => "push_not_allowed",
            Error::Git { .. } => "git",
            Error::BadConfigKey(_) => "bad_config_key",
            Error::BadRemote(_) => "bad_remote",
            Error::NoToken => "no_token",
            Error::GitHub { .. } => "github",
            Error::BackendUnavailable { .. } => "backend_unavailable",
            Error::ViewUnmounted { .. } => "view_unmounted",
            Error::Conflict { .. } | Error::TakeConflict { .. } => "conflict",
            Error::NothingToTake { .. } => "nothing_to_take",
            Error::BadHunk { .. } => "bad_hunk",
            Error::RootAttempt => "root_attempt",
            Error::SyncConflict { .. } => "sync_conflict",
            Error::AmbiguousSpace { .. } => "ambiguous_space",
            Error::TreePruned(_) => "tree_pruned",
            Error::GcBusy => "gc_busy",
            Error::BundleTooLarge(_) => "bundle_too_large",
            Error::NotSaved { .. } => "not_saved",
            Error::NothingToUndo => "nothing_to_undo",
            Error::NotUndoable { .. } => "not_undoable",
            Error::AlreadyUndone { .. } => "already_undone",
            Error::UndoConflict { .. } => "undo_conflict",
            Error::DirtyWorktree { .. } => "dirty_worktree",
            Error::BadTagName { .. } => "bad_tag_name",
            Error::TagExists(_) => "exists",
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
            Error::AlreadyUndone { op, by } => Some(serde_json::json!({ "op": op, "by": by })),
            Error::UndoConflict { op, .. } => Some(serde_json::json!({ "op": op })),
            Error::DirtyWorktree { paths } => Some(serde_json::json!({ "paths": paths })),
            Error::TakeConflict {
                attempt,
                source_attempt,
                paths,
            } => Some(serde_json::json!({
                "attempt": format!("a{attempt}"),
                "source": format!("a{source_attempt}"),
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
