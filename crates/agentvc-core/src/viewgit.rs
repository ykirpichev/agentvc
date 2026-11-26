//! Git inside attempt views (#68).
//!
//! A view has no `.git` directory of its own (views never copy it), so git
//! run inside one used to walk up to the host repository and see nothing of
//! the attempt. Each open view now gets a `.git` *file* pointing at a small
//! per-view git directory in `.agentvc/git/aN`, like a linked worktree:
//!
//! - `commondir` points at the host's git directory, so objects, refs,
//!   config, `info/exclude` and history are shared, and `git log`, `git blame`
//!   and `git show` see the host's history.
//! - `HEAD` is the commit the attempt is based on (the host HEAD at
//!   `agentvc init`, or the commit it was last rebased onto), detached, and
//!   `index` is that commit's tree. So `git status` and `git diff` show
//!   exactly what the attempt changed, which is what `agentvc pr` exports.
//! - Unlike `git worktree add`, nothing is registered under the host's
//!   `.git/worktrees`, so `git worktree list` and `git worktree prune` in the
//!   host never see views.
//!
//! **`git commit` in a view is recorded as a snapshot.** Agents commit by
//! habit; refusing would only make them retry or work around it. A
//! `post-commit` hook runs `agentvc git-hook post-commit`, which snapshots
//! the view (trigger `git-commit`, with the commit id and subject in the
//! snapshot's metadata) and pins the commit with `refs/agentvc/views/aN` so a
//! `git gc` in the host cannot prune it while the view points at it. HEAD
//! stays on the new commit, as git users expect; `agentvc rebase` moves it
//! to the new base. What ships is still the attempt's snapshot, not the
//! commits: `agentvc pr` exports one clean commit as before.
//!
//! The hook needs `core.hooksPath` for views only, which git reads from the
//! per-view `config.worktree` once `extensions.worktreeConfig` is set in the
//! host repository (the setting `git sparse-checkout` uses too; it changes
//! nothing for the main checkout). Views then use `.agentvc/git/hooks`,
//! whose scripts run the repository's own hooks first (from its
//! `core.hooksPath` or `.git/hooks`), so pre-commit hooks keep working.
//! `agentvc config git.capture-commits false` turns the hook off: git still
//! works in views, commits are then just commits.
//!
//! Linking is best-effort: a host without git or without commits gives no
//! link, never a failed fork. Abandoning an attempt removes its link.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::space::{Attempt, SnapshotResult, Space, Status, ROOT_ATTEMPT, SPACE_DIR};

/// Setting that turns commit capture (the post-commit hook) on or off.
pub const CAPTURE_COMMITS: &str = "git.capture-commits";

/// Snapshot trigger recorded for a `git commit` in a view.
pub const COMMIT_TRIGGER: &str = "git-commit";

/// Bumped when the generated hook scripts change, so they are rewritten.
const SHARED_VERSION: u32 = 1;

/// Client-side hooks that views run through to the repository's own. Hooks
/// git runs very often (`reference-transaction`, `post-index-change`) are
/// only wrapped when the repository has them, to keep git fast.
const HOOKS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "post-rewrite",
    "pre-auto-gc",
    "sendemail-validate",
];
const HOT_HOOKS: &[&str] = &["reference-transaction", "post-index-change"];

/// Variables git sets for hooks; they would point agentvc's own git calls
/// at the view's git directory.
const GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];

/// A view's git link, as reported by `fork --json`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ViewGit {
    /// The per-view git directory (`.agentvc/git/aN`).
    pub git_dir: PathBuf,
    /// The commit `HEAD` points at.
    pub head: String,
    /// Whether `git commit` in the view is recorded as a snapshot.
    pub capture_commits: bool,
    /// Time taken to link, which a fork adds to its own.
    pub millis: u128,
}

/// Facts about the host repository shared by every view, cached in
/// `.agentvc/git/shared.json` so a fork spawns git once, not five times.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Shared {
    version: u32,
    /// The host's git directory (absolute).
    common_dir: PathBuf,
    /// Where the repository's own hooks are: its `core.hooksPath` as
    /// written (relative paths are relative to the worktree), else
    /// `<common_dir>/hooks`.
    orig_hooks: String,
    /// Hooks in [`HOT_HOOKS`] the repository had when the scripts were
    /// written.
    hot: Vec<String>,
    /// The `agentvc` binary the post-commit hook runs.
    exe: Option<PathBuf>,
    /// `extensions.worktreeConfig` is on, so per-view `core.hooksPath` works.
    hooks: bool,
}

fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).stdin(Stdio::null());
    for v in GIT_ENV {
        cmd.env_remove(v);
    }
    cmd
}

/// Run git in `dir`, returning trimmed stdout, or `None` on failure.
fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_cmd(dir)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_run(dir: &Path, args: &[&str]) -> Result<()> {
    let out = git_cmd(dir).args(args).output().at("git")?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Git {
        cmd: args.first().copied().unwrap_or("").to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Quote `s` for a POSIX shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The running binary, when it is the `agentvc` CLI (not the daemon or a
/// test runner), for the post-commit hook to call.
fn agentvc_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    (exe.file_stem()? == "agentvc").then_some(exe)
}

fn hook_script(name: &str, orig: &str, exe: Option<&Path>) -> String {
    // A relative path is relative to the worktree top, where git runs hooks.
    let orig = sh_quote(&format!("{orig}/{name}"));
    let head = format!(
        "#!/bin/sh\n# Written by agentvc for attempt views: runs the repository's own\n\
         # {name} hook. See crates/agentvc-core/src/viewgit.rs.\norig={orig}\n"
    );
    if name != "post-commit" {
        return format!("{head}if [ -x \"$orig\" ]; then exec \"$orig\" \"$@\"; fi\nexit 0\n");
    }
    let exe = exe
        .map(|e| sh_quote(&e.to_string_lossy()))
        .unwrap_or_else(|| "agentvc".into());
    format!(
        "{head}if [ -x \"$orig\" ]; then \"$orig\" \"$@\"; fi\n\
         unset {vars}\n\
         bin={exe}\n\
         if ! [ -x \"$bin\" ]; then bin=agentvc; fi\n\
         if ! \"$bin\" git-hook post-commit >/dev/null 2>&1; then\n  \
         echo \"agentvc: could not record this commit as a snapshot; run 'agentvc snapshot'\" >&2\n\
         fi\n\
         exit 0\n",
        vars = GIT_ENV.join(" "),
    )
}

impl Space {
    fn git_root(&self) -> PathBuf {
        self.workdir().join(SPACE_DIR).join("git")
    }

    /// The per-view git directory of attempt `id`.
    pub fn view_git_dir(&self, id: i64) -> PathBuf {
        self.git_root().join(format!("a{id}"))
    }

    fn capture_commits(&self) -> bool {
        !matches!(
            self.config(CAPTURE_COMMITS).ok().flatten().as_deref(),
            Some("false" | "off" | "no" | "0")
        )
    }

    /// Load the shared facts, computing them (and writing the hook scripts)
    /// on first use or when this build writes different scripts. `None`
    /// when the host is not a git repository.
    fn shared_git(&self, want_hooks: bool) -> Result<Option<Shared>> {
        let root = self.git_root();
        let path = root.join("shared.json");
        let exe = agentvc_exe();
        let cached: Option<Shared> = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        if let Some(s) = &cached {
            let exe_ok = exe.is_none() || exe == s.exe;
            if s.version == SHARED_VERSION && exe_ok && (s.hooks || !want_hooks) {
                return Ok(Some(s.clone()));
            }
        }
        let work = self.workdir();
        let Some(common) = git_out(
            work,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ) else {
            return Ok(None);
        };
        let common_dir = PathBuf::from(common);
        let get = |k: &str| git_out(work, &["config", "--get", k]).filter(|v| !v.is_empty());
        let orig_hooks = get("core.hooksPath")
            .unwrap_or_else(|| common_dir.join("hooks").to_string_lossy().into_owned());
        let mut hooks = get("extensions.worktreeConfig").as_deref() == Some("true");
        if !hooks && want_hooks {
            // Like `git sparse-checkout`, only when the common config holds
            // nothing that would then also apply per worktree.
            let bare = get("core.bare").as_deref() == Some("true");
            if !bare && get("core.worktree").is_none() {
                hooks = git_run(work, &["config", "extensions.worktreeConfig", "true"]).is_ok();
            }
        }
        let orig_abs = if orig_hooks.starts_with('/') {
            PathBuf::from(&orig_hooks)
        } else {
            work.join(&orig_hooks)
        };
        let hot = HOT_HOOKS
            .iter()
            .filter(|h| orig_abs.join(h).is_file())
            .map(|h| h.to_string())
            .collect();
        let shared = Shared {
            version: SHARED_VERSION,
            common_dir,
            orig_hooks,
            hot,
            exe: exe.or(cached.and_then(|c| c.exe)),
            hooks,
        };
        self.write_hooks(&shared)?;
        fs::create_dir_all(&root).at(&root)?;
        let tmp = root.join("shared.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&shared).expect("json")).at(&tmp)?;
        fs::rename(&tmp, &path).at(&path)?;
        Ok(Some(shared))
    }

    fn write_hooks(&self, s: &Shared) -> Result<()> {
        let dir = self.git_root().join("hooks");
        let tmp = self
            .git_root()
            .join(format!("hooks.tmp{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).at(&tmp)?;
        let names = HOOKS
            .iter()
            .copied()
            .chain(s.hot.iter().map(String::as_str));
        for name in names {
            let p = tmp.join(name);
            fs::write(&p, hook_script(name, &s.orig_hooks, s.exe.as_deref())).at(&p)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).at(&p)?;
            }
        }
        // Swap the directory in, so a hook running right now never finds a
        // half-written set.
        let old = self
            .git_root()
            .join(format!("hooks.old{}", std::process::id()));
        let had = fs::rename(&dir, &old).is_ok();
        fs::rename(&tmp, &dir).at(&dir)?;
        if had {
            let _ = fs::remove_dir_all(&old);
        }
        Ok(())
    }

    /// Give an attempt's view a `.git` that shows the attempt as changes on
    /// top of its base commit, or point an existing one at a new base (after
    /// a rebase). Returns `None` when there is nothing to link: the root
    /// attempt (the host checkout itself), an abandoned attempt, a view that
    /// is gone, or a host with no git history. A promoted attempt keeps its
    /// link, since it is rebased and exported from its view.
    pub fn link_view_git(&self, attempt: &Attempt) -> Result<Option<ViewGit>> {
        if attempt.id == ROOT_ATTEMPT
            || attempt.status == Status::Abandoned
            || !attempt.view_path.is_dir()
        {
            return Ok(None);
        }
        let start = std::time::Instant::now();
        let Some(head) = self.git_base_commit(&self.head_snapshot(attempt.id)?)? else {
            return Ok(None);
        };
        let capture = self.capture_commits();
        let Some(shared) = self.shared_git(capture)? else {
            return Ok(None);
        };
        let gd = self.view_git_dir(attempt.id);
        fs::create_dir_all(&gd).at(&gd)?;
        let w = |name: &str, body: String| -> Result<()> {
            let p = gd.join(name);
            fs::write(&p, body).at(&p)
        };
        w(
            "commondir",
            format!("{}\n", shared.common_dir.to_string_lossy()),
        )?;
        w("HEAD", format!("{head}\n"))?;
        let hooks = capture && shared.hooks;
        let cfg = gd.join("config.worktree");
        if hooks {
            let dir = self.git_root().join("hooks");
            w(
                "config.worktree",
                format!(
                    "# Written by agentvc: views run the repository's hooks through\n\
                     # agentvc's, which also record `git commit` as a snapshot.\n\
                     [core]\n\thooksPath = {}\n",
                    dir.to_string_lossy()
                ),
            )?;
        } else {
            let _ = fs::remove_file(&cfg);
        }
        // The index starts as the base commit's tree. Stat data is empty, so
        // the first `git status` hashes the tracked files once.
        let gd_s = gd.to_string_lossy().into_owned();
        let view_s = attempt.view_path.to_string_lossy().into_owned();
        git_run(
            self.workdir(),
            &[
                "--git-dir",
                &gd_s,
                "--work-tree",
                &view_s,
                "read-tree",
                &head,
            ],
        )?;
        // Last, so a half-made link is never picked up.
        let dot = attempt.view_path.join(".git");
        let link = format!("gitdir: {gd_s}\n");
        if fs::read_to_string(&dot).ok().as_deref() != Some(link.as_str()) {
            if fs::symlink_metadata(&dot).is_ok_and(|m| m.is_dir()) {
                // Never touch a real git directory someone put there.
                return Ok(None);
            }
            fs::write(&dot, link).at(&dot)?;
        }
        Ok(Some(ViewGit {
            git_dir: gd,
            head,
            capture_commits: hooks,
            millis: start.elapsed().as_millis(),
        }))
    }

    /// [`Space::link_view_git`] for views made before views had git, or
    /// whose link was lost; cheap when the link is there.
    pub(crate) fn ensure_view_git(&self, attempt: &Attempt) {
        if attempt.id == ROOT_ATTEMPT || !attempt.status.is_open() {
            return;
        }
        if attempt.view_path.join(".git").exists() && self.view_git_dir(attempt.id).is_dir() {
            return;
        }
        let _ = self.link_view_git(attempt);
    }

    /// Remove an attempt's git link: the `.git` file in its view (if it is
    /// agentvc's), its git directory and the ref pinning its commits.
    pub fn unlink_view_git(&self, id: i64) -> Result<()> {
        let gd = self.view_git_dir(id);
        let link = format!("gitdir: {}\n", gd.to_string_lossy());
        let mut dots = Vec::new();
        if let Ok(a) = self.attempt(id) {
            dots.push(a.view_path.join(".git"));
        }
        // An unmounted overlay view keeps its `.git` in the upper layer.
        dots.push(
            self.workdir()
                .join(SPACE_DIR)
                .join("overlay")
                .join(format!("a{id}"))
                .join("upper")
                .join(".git"),
        );
        for dot in dots {
            if fs::read_to_string(&dot).ok().as_deref() == Some(link.as_str()) {
                fs::remove_file(&dot).at(&dot)?;
            }
        }
        if gd.join("pinned").exists() {
            let _ = git_run(
                self.workdir(),
                &["update-ref", "-d", &format!("refs/agentvc/views/a{id}")],
            );
        }
        match fs::remove_dir_all(&gd) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).at(&gd),
            _ => Ok(()),
        }
    }

    /// Record the commit just made in the view containing `cwd` (the
    /// post-commit hook): snapshot the view with the commit in its
    /// metadata, and pin the commit against `git gc`.
    pub fn record_git_commit(&self, cwd: &Path) -> Result<SnapshotResult> {
        let attempt = self.attempt_for_path(cwd)?;
        let view = &attempt.view_path;
        let line = git_out(view, &["log", "-1", "--format=%H%n%s"]).ok_or(Error::Git {
            cmd: "log".into(),
            stderr: format!("no commit at HEAD in {}", view.display()),
        })?;
        let (commit, subject) = line.split_once('\n').unwrap_or((line.as_str(), ""));
        let r = self.snapshot(
            attempt.id,
            COMMIT_TRIGGER,
            Some(serde_json::json!({ "git_commit": commit, "message": subject })),
        )?;
        let gd = self.view_git_dir(attempt.id);
        git_run(
            self.workdir(),
            &[
                "update-ref",
                &format!("refs/agentvc/views/a{}", attempt.id),
                commit,
            ],
        )?;
        let _ = fs::write(gd.join("pinned"), "");
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebase::{rebase, RebaseOptions};
    use crate::space::{ForkRequest, Ref};
    use crate::view::BackendChoice;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = git_cmd(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    fn host() -> (crate::testutil::TestDir, Space) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::create_dir_all(w.join("src")).unwrap();
        fs::write(w.join("src/lib.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
        fs::write(w.join("README.md"), "hello\n").unwrap();
        fs::write(w.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(w.join("target")).unwrap();
        fs::write(w.join("target/out"), "built").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-q", "-m", "init"]);
        let space = Space::init(w).unwrap();
        (tmp, space)
    }

    fn fork(space: &Space, from: i64, backend: BackendChoice) -> crate::space::ForkResult {
        space
            .fork(&ForkRequest {
                from: Some(Ref::Attempt(from)),
                actor: "agent:test".into(),
                backend,
                ..Default::default()
            })
            .unwrap()
    }

    /// Every backend that works here, so each is covered where it can be.
    fn backends(space: &Space) -> Vec<BackendChoice> {
        let mut v = vec![BackendChoice::Copy];
        if crate::view::reflinks_supported(&space.workdir().join(".agentvc/views")).is_ok() {
            v.push(BackendChoice::Reflink);
        }
        #[cfg(target_os = "linux")]
        if space.overlay_mounter().is_ok() {
            v.push(BackendChoice::Overlay);
        }
        v
    }

    fn done(space: &Space) {
        #[cfg(target_os = "linux")]
        let _ = space.unmount_views();
        let _ = space;
    }

    #[test]
    fn git_in_a_view_shows_the_attempts_changes_and_host_history() {
        for backend in backends(&host().1) {
            let (tmp, space) = host();
            let base = git(tmp.path(), &["rev-parse", "HEAD"]);
            let f = fork(&space, ROOT_ATTEMPT, backend);
            let view = f.attempt.view_path.clone();
            let vg = f.git.clone().expect("linked");
            assert_eq!(vg.head, base);
            assert_eq!(git(&view, &["status", "--porcelain"]), "", "{backend:?}");

            fs::write(view.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
            fs::write(view.join("new.rs"), "x\n").unwrap();
            fs::remove_file(view.join("README.md")).unwrap();
            let status = git(&view, &["status", "--porcelain"]);
            assert_eq!(
                status, " D README.md\n M src/lib.rs\n?? new.rs",
                "{backend:?}"
            );
            let ours: Vec<String> = space
                .diff(Ref::Attempt(ROOT_ATTEMPT), {
                    space.snapshot(f.attempt.id, "t", None).unwrap();
                    Ref::Attempt(f.attempt.id)
                })
                .unwrap()
                .into_iter()
                .map(|c| c.path)
                .collect();
            assert_eq!(ours, ["README.md", "new.rs", "src/lib.rs"]);
            assert!(git(&view, &["diff"]).contains("+pub fn f() -> u32 { 2 }"));
            assert_eq!(git(&view, &["log", "--format=%s"]), "init");
            assert!(git(
                &view,
                &["blame", "--porcelain", "-L1,1", &base, "--", "src/lib.rs"]
            )
            .starts_with(&base));
            // The snapshot never contains the link.
            let head = space.head_snapshot(f.attempt.id).unwrap();
            let files = crate::snapshot::flatten(
                space.objects(),
                &crate::objects::Oid::from_hex(&head.tree).unwrap(),
            )
            .unwrap();
            assert!(!files.contains_key(".git"));
            // The host sees no extra worktree and no change.
            assert_eq!(git(tmp.path(), &["worktree", "list"]).lines().count(), 1);
            assert_eq!(git(tmp.path(), &["status", "--porcelain"]), "");
            done(&space);
        }
    }

    #[test]
    fn a_child_view_gets_its_own_link_and_abandon_removes_it() {
        for backend in backends(&host().1) {
            let (tmp, space) = host();
            let a = fork(&space, ROOT_ATTEMPT, backend);
            fs::write(a.attempt.view_path.join("a.txt"), "a\n").unwrap();
            let b = fork(&space, a.attempt.id, backend);
            let bv = &b.attempt.view_path;
            assert_eq!(
                fs::read_to_string(bv.join(".git")).unwrap(),
                format!(
                    "gitdir: {}\n",
                    space.view_git_dir(b.attempt.id).to_string_lossy()
                )
            );
            // b sees what its parent changed on top of the base commit.
            assert_eq!(git(bv, &["status", "--porcelain"]), "?? a.txt");
            space
                .set_status(b.attempt.id, Status::Abandoned, Some("test"))
                .unwrap();
            assert!(!space.view_git_dir(b.attempt.id).exists());
            assert!(space.view_git_dir(a.attempt.id).exists());
            done(&space);
            let _ = tmp;
        }
    }

    #[test]
    fn git_commit_in_a_view_is_recorded_and_pinned() {
        let (tmp, space) = host();
        let f = fork(&space, ROOT_ATTEMPT, BackendChoice::Copy);
        let view = &f.attempt.view_path;
        assert!(f.git.as_ref().unwrap().capture_commits);
        assert_eq!(
            git(tmp.path(), &["config", "extensions.worktreeConfig"]),
            "true"
        );
        assert_eq!(
            git(view, &["config", "core.hooksPath"]),
            space.workdir().join(".agentvc/git/hooks").to_string_lossy()
        );
        // The host's own pre-commit hook still runs in views.
        let hooks = tmp.path().join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(
            hooks.join("pre-commit"),
            "#!/bin/sh\necho ran > \"$(git rev-parse --show-toplevel)/hook-ran\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(hooks.join("pre-commit"), fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        fs::write(view.join("src/lib.rs"), "pub fn f() -> u32 { 5 }\n").unwrap();
        git(view, &["commit", "-q", "-am", "five"]);
        assert!(view.join("hook-ran").exists());
        // The hook needs the agentvc binary, which unit tests don't have;
        // record the commit the way it would.
        let r = space.record_git_commit(view).unwrap();
        assert_eq!(r.snapshot.trigger, COMMIT_TRIGGER);
        let commit = git(view, &["rev-parse", "HEAD"]);
        assert_eq!(r.snapshot.meta.as_ref().unwrap()["git_commit"], commit);
        assert_eq!(r.snapshot.meta.as_ref().unwrap()["message"], "five");
        assert_eq!(
            git(
                tmp.path(),
                &[
                    "rev-parse",
                    &format!("refs/agentvc/views/a{}", f.attempt.id)
                ]
            ),
            commit
        );
        // Host HEAD and branch did not move.
        assert_eq!(git(tmp.path(), &["log", "--format=%s", "main"]), "init");
        space
            .set_status(f.attempt.id, Status::Abandoned, Some("test"))
            .unwrap();
        assert!(git_out(
            tmp.path(),
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/agentvc/views/a{}", f.attempt.id)
            ]
        )
        .is_none());
        assert!(!view.join(".git").exists());
    }

    #[test]
    fn capture_can_be_turned_off_and_old_views_get_linked() {
        let (tmp, space) = host();
        space
            .set_config(CAPTURE_COMMITS, &["false".into()])
            .unwrap();
        let f = fork(&space, ROOT_ATTEMPT, BackendChoice::Copy);
        assert!(!f.git.as_ref().unwrap().capture_commits);
        assert!(git_out(tmp.path(), &["config", "extensions.worktreeConfig"]).is_none());
        // A view whose link went missing gets it back on next use.
        fs::remove_file(f.attempt.view_path.join(".git")).unwrap();
        space.snapshot(f.attempt.id, "t", None).unwrap();
        assert!(f.attempt.view_path.join(".git").is_file());
    }

    #[test]
    fn rebasing_a_promoted_attempt_moves_its_views_head() {
        let (tmp, space) = host();
        let f = fork(&space, ROOT_ATTEMPT, BackendChoice::Copy);
        let view = &f.attempt.view_path;
        fs::write(view.join("src/lib.rs"), "pub fn f() -> u32 { 2 }\n").unwrap();
        space
            .set_status(f.attempt.id, Status::Promoted, None)
            .unwrap();
        // Main moves on in the host.
        fs::write(tmp.path().join("NEWS.md"), "news\n").unwrap();
        git(tmp.path(), &["add", "NEWS.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "news"]);
        let main = git(tmp.path(), &["rev-parse", "HEAD"]);
        let opts = RebaseOptions {
            onto: Some("main".into()),
            ..Default::default()
        };
        assert!(rebase(&space, f.attempt.id, &opts).unwrap().rebased);
        assert_eq!(git(view, &["rev-parse", "HEAD"]), main);
        assert_eq!(git(view, &["status", "--porcelain"]), " M src/lib.rs");
        done(&space);
    }

    #[test]
    fn no_git_no_link() {
        let tmp = crate::testutil::tempdir();
        fs::write(tmp.path().join("a"), "a").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let f = fork(&space, ROOT_ATTEMPT, BackendChoice::Copy);
        assert!(f.git.is_none());
        assert!(!f.attempt.view_path.join(".git").exists());
    }
}
