//! Keep open PR branches mergeable as main moves (`agentvc restack`).
//!
//! Parallel threads open PRs from the same main; each merge on github.com
//! makes the others stale, and many then conflict. `restack` re-merges each
//! branch with the new main using the deterministic resolution strategies
//! of [`crate::resolve`], and when nothing is left to a human:
//!
//! - a branch with one commit on top of its merge base (what `agentvc pr`
//!   exports) is rebased: the same message and author on top of main,
//!   pushed with a lease on the old tip;
//! - a longer branch gets a merge commit of main, so its history is kept and
//!   no force push is needed.
//!
//! When conflicts are left, the branch is not touched and the PR gets one
//! comment (edited in place on later runs) naming the files.
//!
//! It needs no agentvc space, only a git checkout with the remote, so it
//! runs in CI: `.github/workflows/restack.yml` runs it on every push to main.

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::export::HostRepo;
use crate::github::Api;
use crate::rebase::{merge_commits, resolve_onto};
use crate::resolve::{Resolution, Rules, Sides};
use crate::RebaseOptions;

/// Marks the PR comment `restack` keeps up to date.
pub const COMMENT_MARKER: &str = "<!-- agentvc:restack -->";
/// PRs with this label are left alone.
pub const SKIP_LABEL: &str = "agentvc:no-restack";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Rebase single-commit branches, merge main into longer ones.
    #[default]
    Auto,
    Rebase,
    Merge,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "auto" => Some(Mode::Auto),
            "rebase" => Some(Mode::Rebase),
            "merge" => Some(Mode::Merge),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RestackOptions {
    /// Branches to restack (names on the remote).
    pub branches: Vec<String>,
    /// Also every open PR into the default branch whose head is in this
    /// repository (needs a GitHub token).
    pub all_prs: bool,
    /// GitHub repository as owner/name; defaults to the remote's.
    pub repo: Option<String>,
    pub remote: Option<String>,
    /// What to restack onto; defaults to the remote's default branch.
    pub onto: Option<String>,
    pub no_fetch: bool,
    pub mode: Mode,
    /// Push restacked branches. Without it, `restack` only reports.
    pub push: bool,
    /// Comment on PRs that still conflict (needs a GitHub token).
    pub comment: bool,
    pub rules: Rules,
}

/// What happened to one branch.
#[derive(Clone, Debug, Serialize)]
pub struct BranchRestack {
    pub branch: String,
    pub pr: Option<u64>,
    /// `up_to_date`, `restacked`, `conflicts`, `skipped` or `error`.
    pub outcome: &'static str,
    /// `rebase` or `merge`, for restacked branches.
    pub how: Option<&'static str>,
    /// The tip before.
    pub from: Option<String>,
    /// The new tip (pushed only with `push`).
    pub commit: Option<String>,
    pub pushed: bool,
    pub resolved: Vec<Resolution>,
    pub conflicts: Vec<Resolution>,
    pub comment: Option<String>,
    pub message: Option<String>,
}

impl BranchRestack {
    fn new(branch: &str, pr: Option<u64>, outcome: &'static str) -> BranchRestack {
        BranchRestack {
            branch: branch.to_string(),
            pr,
            outcome,
            how: None,
            from: None,
            commit: None,
            pushed: false,
            resolved: Vec::new(),
            conflicts: Vec::new(),
            comment: None,
            message: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RestackReport {
    pub onto: String,
    pub onto_ref: String,
    pub branches: Vec<BranchRestack>,
}

/// Open PRs into `base` whose head branch lives in `repo`, without the skip label.
fn open_prs(api: &Api, repo: &str, base: &str) -> Result<Vec<(u64, String)>> {
    let mut out = Vec::new();
    let mut page = 1;
    loop {
        let list = api.call(
            "GET",
            &format!("/repos/{repo}/pulls?state=open&base={base}&per_page=100&page={page}"),
            None,
        )?;
        let items = list.as_array().cloned().unwrap_or_default();
        for pr in &items {
            let same_repo = pr["head"]["repo"]["full_name"].as_str() == Some(repo);
            let skip = pr["labels"]
                .as_array()
                .is_some_and(|ls| ls.iter().any(|l| l["name"] == SKIP_LABEL));
            if let (true, false, Some(n), Some(branch)) = (
                same_repo,
                skip,
                pr["number"].as_u64(),
                pr["head"]["ref"].as_str(),
            ) {
                out.push((n, branch.to_string()));
            }
        }
        if items.len() < 100 {
            return Ok(out);
        }
        page += 1;
    }
}

fn find_comment(api: &Api, repo: &str, pr: u64) -> Result<Option<Value>> {
    let list = api.call(
        "GET",
        &format!("/repos/{repo}/issues/{pr}/comments?per_page=100"),
        None,
    )?;
    Ok(list.as_array().and_then(|cs| {
        cs.iter()
            .find(|c| {
                c["body"]
                    .as_str()
                    .is_some_and(|b| b.starts_with(COMMENT_MARKER))
            })
            .cloned()
    }))
}

fn conflict_comment(r: &BranchRestack, onto_ref: &str, onto: &str) -> String {
    let mut body = format!(
        "{COMMENT_MARKER}\n**agentvc restack:** `{onto_ref}` moved to `{}` and `{}` no longer merges cleanly, so it was left as it is.\n\n",
        &onto[..12.min(onto.len())],
        r.branch
    );
    body += "Still conflicting:\n\n";
    for c in &r.conflicts {
        if c.hunks + c.remaining > 0 && c.strategy != crate::resolve::Strategy::None {
            body += &format!(
                "- `{}`: {} hunk{} left ({} resolved by `{}`)\n",
                c.path,
                c.remaining,
                if c.remaining == 1 { "" } else { "s" },
                c.hunks,
                c.strategy
            );
        } else {
            body += &format!("- `{}` (not a text conflict)\n", c.path);
        }
    }
    if !r.resolved.is_empty() {
        body += "\nResolved automatically: ";
        body += &r
            .resolved
            .iter()
            .map(|c| format!("`{}` ({})", c.path, c.strategy))
            .collect::<Vec<_>>()
            .join(", ");
        body += "\n";
    }
    body += "\nTo finish: `agentvc rebase aN --keep-conflicts` on the attempt (the files get diff3 markers; \
             fix them, `agentvc snapshot aN`, `agentvc pr aN`), or merge main into the branch by hand. \
             The next push to main tries again.\n";
    body
}

fn clean_comment(r: &BranchRestack, onto_ref: &str) -> String {
    format!(
        "{COMMENT_MARKER}\n**agentvc restack:** `{}` merges cleanly with `{onto_ref}` again ({}).\n",
        r.branch,
        match r.commit.as_deref() {
            Some(c) => format!("restacked as `{}`", &c[..12.min(c.len())]),
            None => "up to date".into(),
        }
    )
}

/// Restack the requested branches onto the remote's default branch.
pub fn restack(workdir: &std::path::Path, opts: &RestackOptions) -> Result<RestackReport> {
    let repo = HostRepo::open(workdir)?;
    let remote = opts.remote.clone().unwrap_or_else(|| "origin".into());
    let (onto, onto_ref) = resolve_onto(
        &repo,
        &RebaseOptions {
            onto: opts.onto.clone(),
            remote: Some(remote.clone()),
            no_fetch: opts.no_fetch,
            ..Default::default()
        },
    )?;
    let base_branch = onto_ref
        .strip_prefix(&format!("{remote}/"))
        .unwrap_or(&onto_ref)
        .to_string();

    let needs_api = opts.all_prs || opts.comment;
    let api = if needs_api {
        Some(Api::new(crate::github::token()?))
    } else {
        None
    };
    let gh_repo = if needs_api {
        Some(crate::github::repo_for(
            workdir,
            &remote,
            opts.repo.as_deref(),
        )?)
    } else {
        None
    };
    let mut targets: Vec<(Option<u64>, String)> =
        opts.branches.iter().map(|b| (None, b.clone())).collect();
    if let (true, Some(api), Some(r)) = (opts.all_prs, &api, &gh_repo) {
        for (n, b) in open_prs(api, r, &base_branch)? {
            match targets.iter_mut().find(|(_, t)| *t == b) {
                Some(t) => t.0 = Some(n),
                None => targets.push((Some(n), b)),
            }
        }
    }

    let mut out = Vec::new();
    for (pr, branch) in targets {
        let mut r = match restack_branch(&repo, &remote, &branch, &onto, &onto_ref, opts) {
            Ok(r) => r,
            Err(e) => {
                let mut r = BranchRestack::new(&branch, pr, "error");
                r.message = Some(e.to_string());
                r
            }
        };
        r.pr = pr;
        if let (true, Some(api), Some(gh), Some(n)) = (opts.comment, &api, &gh_repo, pr) {
            let body = match r.outcome {
                "conflicts" => Some(conflict_comment(&r, &onto_ref, &onto)),
                "restacked" if opts.push => Some(clean_comment(&r, &onto_ref)),
                _ => None,
            };
            let existing = find_comment(api, gh, n).ok().flatten();
            let posted = match (body, existing) {
                // Same text as last time: leave it.
                (Some(b), Some(c)) if c["body"].as_str() == Some(b.as_str()) => {
                    c["html_url"].as_str().map(str::to_string)
                }
                (Some(b), Some(c)) => c["id"].as_u64().and_then(|id| {
                    api.call(
                        "PATCH",
                        &format!("/repos/{gh}/issues/comments/{id}"),
                        Some(json!({ "body": b })),
                    )
                    .ok()
                    .and_then(|v| v["html_url"].as_str().map(str::to_string))
                }),
                // Only a conflict starts a comment; a clean run edits one.
                (Some(b), None) if r.outcome == "conflicts" => api
                    .call(
                        "POST",
                        &format!("/repos/{gh}/issues/{n}/comments"),
                        Some(json!({ "body": b })),
                    )
                    .ok()
                    .and_then(|v| v["html_url"].as_str().map(str::to_string)),
                _ => None,
            };
            r.comment = posted;
        }
        out.push(r);
    }
    Ok(RestackReport {
        onto,
        onto_ref,
        branches: out,
    })
}

fn restack_branch(
    repo: &HostRepo,
    remote: &str,
    branch: &str,
    onto: &str,
    onto_ref: &str,
    opts: &RestackOptions,
) -> Result<BranchRestack> {
    if branch.starts_with('-') || branch.contains("..") {
        return Err(Error::BadBranch(branch.to_string()));
    }
    let tracking = format!("refs/remotes/{remote}/{branch}");
    if !opts.no_fetch {
        repo.git(
            &[
                "fetch",
                "--quiet",
                remote,
                &format!("+refs/heads/{branch}:{tracking}"),
            ],
            None,
        )?;
    }
    let tip = repo.rev(&tracking).ok_or_else(|| Error::NotFound {
        kind: "branch",
        id: format!("{remote}/{branch}"),
    })?;
    let mut r = BranchRestack::new(branch, None, "up_to_date");
    r.from = Some(tip.clone());
    if repo.is_ancestor(onto, &tip) {
        return Ok(r);
    }
    let base = repo.git(&["merge-base", &tip, onto], None)?;
    let sides = Sides {
        ours: branch.to_string(),
        theirs: onto_ref.to_string(),
        theirs_first: false,
    };
    let m = merge_commits(repo, Some(&base), &tip, onto, &opts.rules, &sides)?;
    r.resolved = m.resolved();
    if !m.conflicts.is_empty() {
        r.outcome = "conflicts";
        r.conflicts = m
            .conflicts
            .iter()
            .map(|p| {
                m.files
                    .iter()
                    .find(|f| &f.path == p)
                    .cloned()
                    .unwrap_or(Resolution {
                        path: p.clone(),
                        strategy: crate::resolve::Strategy::None,
                        hunks: 0,
                        remaining: 1,
                    })
            })
            .collect();
        return Ok(r);
    }
    let ahead: usize = repo
        .git(&["rev-list", "--count", &format!("{base}..{tip}")], None)?
        .parse()
        .unwrap_or(0);
    let rebase = match opts.mode {
        Mode::Rebase => true,
        Mode::Merge => false,
        Mode::Auto => ahead == 1,
    };
    // Commit as the configured user, or as agentvc where none is (CI).
    let mut env: Vec<(String, String)> = Vec::new();
    if repo.git(&["config", "user.name"], None).is_err() {
        env.push(("GIT_COMMITTER_NAME".into(), "agentvc".into()));
        env.push((
            "GIT_COMMITTER_EMAIL".into(),
            "agentvc@users.noreply.github.com".into(),
        ));
        env.push(("GIT_AUTHOR_NAME".into(), "agentvc".into()));
        env.push((
            "GIT_AUTHOR_EMAIL".into(),
            "agentvc@users.noreply.github.com".into(),
        ));
    }
    let new = if rebase {
        let fmt = |f: &str| repo.git_raw(&["log", "-1", &format!("--format={f}"), &tip]);
        let message = fmt("%B")?;
        env.retain(|(k, _)| !k.starts_with("GIT_AUTHOR"));
        env.push(("GIT_AUTHOR_NAME".into(), fmt("%an")?.trim().to_string()));
        env.push(("GIT_AUTHOR_EMAIL".into(), fmt("%ae")?.trim().to_string()));
        env.push(("GIT_AUTHOR_DATE".into(), fmt("%aI")?.trim().to_string()));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        repo.git_env(
            &["commit-tree", &m.tree, "-p", onto, "-F", "-"],
            Some(&message),
            &env,
        )?
    } else {
        let mut message = format!("Merge {onto_ref} into {branch}\n\nRestacked by agentvc.");
        if !r.resolved.is_empty() {
            message += " Auto-resolved conflicts:\n";
            for f in &r.resolved {
                message += &format!("\n- {} ({})", f.path, f.strategy);
            }
        }
        message += "\n";
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        repo.git_env(
            &["commit-tree", &m.tree, "-p", &tip, "-p", onto, "-F", "-"],
            Some(&message),
            &env,
        )?
    };
    r.outcome = "restacked";
    r.how = Some(if rebase { "rebase" } else { "merge" });
    r.commit = Some(new.clone());
    if opts.push {
        repo.git(
            &[
                "push",
                "--quiet",
                &format!("--force-with-lease=refs/heads/{branch}:{tip}"),
                remote,
                &format!("{new}:refs/heads/{branch}"),
            ],
            None,
        )?;
        let _ = repo.git(&["update-ref", &tracking, &new], None);
        r.pushed = true;
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn commit(dir: &Path, file: &str, text: &str, msg: &str) -> String {
        fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", msg]);
        git(dir, &["rev-parse", "HEAD"])
    }

    /// A bare "remote" and a clone, main with one file, and three PR
    /// branches started from the same main.
    fn setup() -> (crate::testutil::TestDir, std::path::PathBuf) {
        let tmp = crate::testutil::tempdir();
        let origin = tmp.path().join("origin.git");
        let w = tmp.path().join("w");
        git(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        git(
            tmp.path(),
            &["clone", "-q", origin.to_str().unwrap(), w.to_str().unwrap()],
        );
        git(&w, &["checkout", "-q", "-b", "main"]);
        commit(&w, "list.txt", "alpha\n", "base");
        fs::write(w.join("clash.txt"), "one\n").unwrap();
        commit(&w, "other.txt", "x\n", "other");
        git(&w, &["push", "-q", "origin", "main"]);
        for (b, file, text) in [
            ("add-beta", "list.txt", "alpha\nbeta\n"),
            ("edit-other", "other.txt", "y\n"),
            ("clash", "clash.txt", "three\n"),
        ] {
            git(&w, &["checkout", "-q", "-b", b, "main"]);
            commit(&w, file, text, b);
            git(&w, &["push", "-q", "origin", b]);
        }
        // Main moves: one PR lands (adds gamma after alpha).
        git(&w, &["checkout", "-q", "main"]);
        fs::write(w.join("clash.txt"), "two\n").unwrap();
        commit(&w, "list.txt", "alpha\ngamma\n", "gamma");
        git(&w, &["push", "-q", "origin", "main"]);
        (tmp, w)
    }

    #[test]
    fn restack_rebases_resolves_and_leaves_real_conflicts() {
        let (_tmp, w) = setup();
        let opts = RestackOptions {
            branches: vec!["add-beta".into(), "edit-other".into(), "clash".into()],
            push: true,
            rules: Rules::new(),
            ..Default::default()
        };
        let r = restack(&w, &opts).unwrap();
        let by = |b: &str| r.branches.iter().find(|x| x.branch == b).unwrap();
        // Both sides added after "alpha": auto-resolved and rebased.
        let beta = by("add-beta");
        assert_eq!(beta.outcome, "restacked", "{beta:?}");
        assert_eq!(beta.how, Some("rebase"));
        assert!(beta.pushed);
        assert_eq!(beta.resolved[0].path, "list.txt");
        let tip = git(&w, &["rev-parse", "origin/add-beta"]);
        assert_eq!(
            git(&w, &["show", &format!("{tip}:list.txt")]),
            "alpha\nbeta\ngamma"
        );
        assert_eq!(
            git(&w, &["rev-parse", &format!("{tip}^")]),
            git(&w, &["rev-parse", "origin/main"])
        );
        assert_eq!(git(&w, &["log", "-1", "--format=%s", &tip]), "add-beta");
        // No overlap at all: a plain rebase.
        assert_eq!(by("edit-other").outcome, "restacked");
        // The same line changed both ways: left alone.
        let clash = by("clash");
        assert_eq!(clash.outcome, "conflicts");
        assert!(!clash.pushed);
        assert_eq!(clash.conflicts[0].path, "clash.txt");
        assert_eq!(clash.conflicts[0].remaining, 1);
        let body = conflict_comment(clash, "origin/main", &r.onto);
        assert!(body.starts_with(COMMENT_MARKER) && body.contains("`clash.txt`: 1 hunk left"));

        // A second run finds everything restacked up to date.
        let again = restack(&w, &opts).unwrap();
        let outcomes: Vec<_> = again.branches.iter().map(|b| b.outcome).collect();
        assert_eq!(outcomes, ["up_to_date", "up_to_date", "conflicts"]);
    }

    #[test]
    fn longer_branches_get_a_merge_commit_and_dry_runs_push_nothing() {
        let (_tmp, w) = setup();
        git(&w, &["checkout", "-q", "add-beta"]);
        commit(&w, "more.txt", "more\n", "second commit");
        git(&w, &["push", "-q", "origin", "add-beta"]);
        let before = git(&w, &["rev-parse", "origin/add-beta"]);

        let dry = RestackOptions {
            branches: vec!["add-beta".into()],
            rules: Rules::new(),
            ..Default::default()
        };
        let r = restack(&w, &dry).unwrap();
        assert_eq!(r.branches[0].how, Some("merge"));
        assert!(!r.branches[0].pushed);
        git(&w, &["fetch", "-q", "origin"]);
        assert_eq!(git(&w, &["rev-parse", "origin/add-beta"]), before);

        let r = restack(&w, &RestackOptions { push: true, ..dry }).unwrap();
        let tip = r.branches[0].commit.clone().unwrap();
        assert_eq!(git(&w, &["rev-parse", "origin/add-beta"]), tip);
        let parents = git(&w, &["log", "-1", "--format=%P", &tip]);
        assert_eq!(
            parents.split(' ').collect::<Vec<_>>(),
            [before.as_str(), &r.onto]
        );
    }
}
