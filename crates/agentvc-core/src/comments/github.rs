//! Review comments and GitHub PR reviews (#170).
//!
//! A branch with a PR (`agentvc pr`, `stack submit`) can send a review to
//! GitHub too: [`post`] turns the drafts into one PR review, line comments
//! where the lines are in the PR's diff and the rest in the review's body.
//! [`sync`] brings the PR's review threads in (new threads from GitHub,
//! replies, resolutions) and pushes local changes out (replies written
//! here, threads resolved or reopened here). A reply or a resolve on a
//! thread that is on GitHub pushes right away when it can; when it can't
//! (offline, no token) the error is kept on the thread and the next sync
//! tries again.
//!
//! Resolution is three-way: `github.resolved` is the state last seen on
//! GitHub, so whichever side changed since wins. `addressed` (an agent says
//! it is done) stays open on GitHub; only a person resolves.

use super::*;
use crate::github::{repo_for, token, Api};

/// A pull request on GitHub.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrRef {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
}

/// What [`sync`] did.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Synced {
    pub repo: String,
    pub pr: u64,
    /// Threads new from GitHub.
    pub imported: Vec<String>,
    /// Threads that got replies or a new state from GitHub.
    pub updated: Vec<String>,
    /// Threads whose local replies or state went to GitHub.
    pub pushed: Vec<String>,
    /// `id: message` for threads that could not be pushed.
    pub errors: Vec<String>,
}

/// A review posted on GitHub, before it is recorded here.
#[derive(Clone, Debug, Default)]
pub struct Posted {
    pub review: u64,
    pub links: Vec<(String, GithubLink)>,
}

#[cfg(test)]
pub(crate) static TEST_API: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn api() -> Result<Api> {
    #[cfg(test)]
    if let Some(base) = TEST_API.lock().unwrap().clone() {
        let mut a = Api::new("t".into());
        a.set_base(&base);
        return Ok(a);
    }
    Ok(Api::new(token()?))
}

fn no_pr(target: &Target) -> Error {
    err_with(
        "no_pr",
        format!(
            "{} has no GitHub PR; open one with `agentvc pr` or `agentvc stack submit`",
            target.key()
        ),
        json!({ "target": target.key() }),
    )
}

/// The PR of a branch target.
pub fn pr_of(cwd: &Path, target: &Target) -> Result<PrRef> {
    if target.kind != TargetKind::Branch {
        return Err(no_pr(target));
    }
    let here = Window::open(cwd)?;
    let ws = crate::workspace::find_local(&here, target.workspace.as_deref().unwrap_or(""))?;
    let w = Window::open(&ws.path)?;
    let number = w
        .state
        .branches
        .iter()
        .find(|b| b.name == target.name)
        .and_then(|b| b.pr)
        .ok_or_else(|| no_pr(target))?;
    let remote = if w.state.remote.is_empty() {
        "origin"
    } else {
        w.state.remote.as_str()
    };
    Ok(PrRef {
        repo: repo_for(&ws.path, remote, None)?,
        number,
    })
}

/// The target's PR, `None` when it has none.
pub fn pr_if_any(cwd: &Path, target: &Target) -> Option<PrRef> {
    pr_of(cwd, target).ok()
}

// ---- small helpers -------------------------------------------------------------

/// Seconds since the epoch of an RFC 3339 time like `2026-11-27T10:00:00Z`.
pub(crate) fn rfc3339(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.trim_end_matches('Z');
    let time = time.split(['+', '.']).next()?;
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next().flatten().unwrap_or(0));
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// A diff hunk's `(first, last)` lines on the old and the new side.
pub(crate) type Hunk = ((usize, usize), (usize, usize));

/// Line ranges of a file's diff hunks, `(old, new)` per hunk, from a
/// GitHub `patch`.
pub(crate) fn hunks(patch: &str) -> Vec<Hunk> {
    let range = |s: &str| -> Option<(usize, usize)> {
        let (a, n) = s.split_once(',').unwrap_or((s, "1"));
        let a: usize = a.parse().ok()?;
        let n: usize = n.parse().ok()?;
        Some((a, a + n.max(1) - 1))
    };
    patch
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("@@ -")?;
            let (old, rest) = rest.split_once(" +")?;
            let new = rest.split(' ').next()?;
            Some((range(old)?, range(new)?))
        })
        .collect()
}

fn agent_footer(c: &Comment) -> String {
    match c.author.strip_prefix("agent:") {
        Some(a) => format!("{}\n\n_Posted by agentvc for agent {a}._", c.body),
        None => c.body.clone(),
    }
}

fn gh_side(side: Side) -> &'static str {
    match side {
        Side::Old => "LEFT",
        Side::New => "RIGHT",
    }
}

fn quote_md(a: &Anchor) -> String {
    a.lines
        .iter()
        .map(|l| format!("> `{}`\n", l.replace('`', "'")))
        .collect()
}

// ---- posting a review --------------------------------------------------------

/// Post `threads` (the drafts being sent) as one PR review of `pr`.
pub fn post_review(cwd: &Path, pr: &PrRef, threads: &[Thread], by: &str) -> Result<Posted> {
    let api = api()?;
    let base = format!("/repos/{}/pulls/{}", pr.repo, pr.number);
    let pull = api.call("GET", &base, None)?;
    let head_sha = pull["head"]["sha"].as_str().unwrap_or("").to_string();

    // The lines each file's diff shows, per side.
    let mut diff: HashMap<String, Vec<Hunk>> = HashMap::new();
    for page in 1..=30 {
        let files = api.call(
            "GET",
            &format!("{base}/files?per_page=100&page={page}"),
            None,
        )?;
        let files = files.as_array().cloned().unwrap_or_default();
        for f in &files {
            if let (Some(name), Some(patch)) = (f["filename"].as_str(), f["patch"].as_str()) {
                diff.insert(name.to_string(), hunks(patch));
            }
        }
        if files.len() < 100 {
            break;
        }
    }

    // Where the PR's head has each thread's lines: the local tip when it is
    // what was pushed, else the file at the pushed commit, when it is here.
    let heads_now = threads.first().and_then(|t| heads(cwd, &t.target).ok());
    let head_rev = heads_now.as_ref().and_then(|h| match &h.head {
        Rev::Git { dir, commit } if *commit != head_sha && !head_sha.is_empty() => Some(Rev::Git {
            dir: dir.clone(),
            commit: head_sha.clone(),
        }),
        _ => None,
    });
    let at_head = |t: &Thread, a: &Anchor| -> Option<Position> {
        match (&head_rev, a.side) {
            (Some(rev), Side::New) => locate(a, &rev.read(&a.path)?),
            _ => t.current,
        }
    };

    let mut comments = Vec::new();
    let mut inline: Vec<(String, String, String)> = Vec::new(); // (thread, path, body)
    let mut body = String::new();
    for t in threads {
        let Some(c) = t.comments.first() else {
            continue;
        };
        let text = agent_footer(c);
        let placed = t.anchor.as_ref().and_then(|a| {
            let p = at_head(t, a)?;
            let hs = diff.get(&a.path)?;
            let fits = hs.iter().any(|(old, new)| {
                let (lo, hi) = if a.side == Side::Old { *old } else { *new };
                p.start >= lo && p.end <= hi
            });
            fits.then_some((a, p))
        });
        match placed {
            Some((a, p)) => {
                let mut c = json!({
                    "path": a.path, "body": text, "line": p.end, "side": gh_side(a.side),
                });
                if p.start < p.end {
                    c["start_line"] = json!(p.start);
                    c["start_side"] = json!(gh_side(a.side));
                }
                comments.push(c);
                inline.push((t.id.clone(), a.path.clone(), text));
            }
            None => {
                if !body.is_empty() {
                    body.push_str("\n---\n\n");
                }
                if let Some(a) = &t.anchor {
                    let p = t.current.unwrap_or(Position {
                        start: a.start,
                        end: a.end,
                    });
                    let lines = if p.start == p.end {
                        p.start.to_string()
                    } else {
                        format!("{}-{}", p.start, p.end)
                    };
                    body.push_str(&format!("**{}:{lines}**\n{}\n", a.path, quote_md(a)));
                }
                body.push_str(&text);
                body.push('\n');
            }
        }
    }
    if by.starts_with("agent:") && body.is_empty() && comments.is_empty() {
        return Ok(Posted::default());
    }
    let mut req = json!({ "event": "COMMENT", "body": body.trim_end(), "comments": comments });
    if !head_sha.is_empty() {
        req["commit_id"] = json!(head_sha);
    }
    let review = api.call("POST", &format!("{base}/reviews"), Some(req))?;
    let rid = review["id"].as_u64().unwrap_or(0);
    let review_url = review["html_url"].as_str().map(str::to_string);

    // Which posted comment is which thread's.
    let posted = if inline.is_empty() {
        Vec::new()
    } else {
        api.call(
            "GET",
            &format!("{base}/reviews/{rid}/comments?per_page=100"),
            None,
        )?
        .as_array()
        .cloned()
        .unwrap_or_default()
    };
    let mut used = vec![false; posted.len()];
    let mut links = Vec::new();
    for t in threads {
        let mine = inline.iter().find(|(id, _, _)| *id == t.id);
        let found = mine.and_then(|(_, path, text)| {
            let i = posted.iter().enumerate().position(|(i, c)| {
                !used[i] && c["path"].as_str() == Some(path) && c["body"].as_str() == Some(text)
            })?;
            used[i] = true;
            Some(&posted[i])
        });
        let link = match found {
            Some(c) => GithubLink {
                repo: Some(pr.repo.clone()),
                pr: pr.number,
                thread: None,
                comment: c["id"].as_u64(),
                url: c["html_url"].as_str().map(str::to_string),
                resolved: Some(false),
                error: None,
            },
            None => GithubLink {
                repo: Some(pr.repo.clone()),
                pr: pr.number,
                thread: None,
                comment: None,
                url: review_url.clone(),
                resolved: None,
                error: None,
            },
        };
        links.push((t.id.clone(), link));
    }
    Ok(Posted { review: rid, links })
}

/// Record a posted review on the threads it carried.
pub(super) fn record(s: &mut Store, review: &mut Review, posted: &Posted) {
    review.github_review = Some(posted.review).filter(|r| *r != 0);
    for (id, link) in &posted.links {
        if let Some(t) = s.threads.iter_mut().find(|t| t.id == *id) {
            if let Some(c) = t.comments.first_mut() {
                c.github_id = link.comment;
            }
            t.github = Some(link.clone());
        }
    }
}

// ---- pulling and pushing -----------------------------------------------------

const THREADS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { \
  repository(owner: $owner, name: $name) { pullRequest(number: $number) { \
    reviewThreads(first: 100) { nodes { id isResolved isOutdated path line startLine \
      originalLine originalStartLine diffSide \
      comments(first: 100) { nodes { databaseId author { login } body url createdAt } } } } } } }";

#[derive(Clone, Debug)]
struct GhComment {
    id: u64,
    author: String,
    body: String,
    url: Option<String>,
    at: i64,
}

#[derive(Clone, Debug)]
struct GhThread {
    id: String,
    resolved: bool,
    path: String,
    side: Side,
    line: Option<usize>,
    start: Option<usize>,
    original: Option<usize>,
    original_start: Option<usize>,
    comments: Vec<GhComment>,
}

fn fetch_threads(api: &Api, pr: &PrRef) -> Result<Vec<GhThread>> {
    let (owner, name) = pr.repo.split_once('/').unwrap_or((&pr.repo, ""));
    let v = api.call(
        "POST",
        "/graphql",
        Some(json!({
            "query": THREADS_QUERY,
            "variables": { "owner": owner, "name": name, "number": pr.number },
        })),
    )?;
    let nodes = &v["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"];
    let Some(nodes) = nodes.as_array() else {
        return Err(Error::GitHub {
            status: 200,
            message: format!("unexpected GraphQL reply: {}", v["errors"]),
        });
    };
    let n = |v: &Value| v.as_u64().map(|x| x as usize);
    Ok(nodes
        .iter()
        .map(|t| GhThread {
            id: t["id"].as_str().unwrap_or("").to_string(),
            resolved: t["isResolved"].as_bool().unwrap_or(false),
            path: t["path"].as_str().unwrap_or("").to_string(),
            side: if t["diffSide"].as_str() == Some("LEFT") {
                Side::Old
            } else {
                Side::New
            },
            line: n(&t["line"]),
            start: n(&t["startLine"]),
            original: n(&t["originalLine"]),
            original_start: n(&t["originalStartLine"]),
            comments: t["comments"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| GhComment {
                    id: c["databaseId"].as_u64().unwrap_or(0),
                    author: format!(
                        "github:{}",
                        c["author"]["login"].as_str().unwrap_or("ghost")
                    ),
                    body: c["body"].as_str().unwrap_or("").to_string(),
                    url: c["url"].as_str().map(str::to_string),
                    at: c["createdAt"]
                        .as_str()
                        .and_then(rfc3339)
                        .unwrap_or_else(now),
                })
                .collect(),
        })
        .collect())
}

/// An anchor for a thread from GitHub: on its lines when the file is here,
/// else on no lines (shown as outdated, with the comment).
fn gh_anchor(cwd: &Path, target: &Target, head_sha: &str, g: &GhThread) -> Option<Anchor> {
    if g.path.is_empty() {
        return None;
    }
    let (start, end) = match g.line {
        Some(end) => (g.start.unwrap_or(end), end),
        None => {
            let end = g.original.unwrap_or(1);
            return Some(Anchor {
                path: g.path.clone(),
                side: g.side,
                start: g.original_start.unwrap_or(end),
                end,
                rev: None,
                lines: Vec::new(),
                before: Vec::new(),
                after: Vec::new(),
                hash: String::new(),
            });
        }
    };
    let h = heads(cwd, target).ok();
    let rev = match (g.side, &h) {
        (
            Side::New,
            Some(Heads {
                head: Rev::Git { dir, .. },
                ..
            }),
        ) if !head_sha.is_empty() => {
            let at_pr = Rev::Git {
                dir: dir.clone(),
                commit: head_sha.to_string(),
            };
            if at_pr.read(&g.path).is_some() {
                Some(at_pr)
            } else {
                h.as_ref().map(|h| h.head.clone())
            }
        }
        (side, Some(h)) => h.side(side).cloned(),
        _ => None,
    };
    let made = rev.as_ref().and_then(|r| {
        let text = r.read(&g.path)?;
        make_anchor(&g.path, g.side, start, end, &text, Some(r.label())).ok()
    });
    made.or(Some(Anchor {
        path: g.path.clone(),
        side: g.side,
        start,
        end,
        rev: None,
        lines: Vec::new(),
        before: Vec::new(),
        after: Vec::new(),
        hash: String::new(),
    }))
}

/// Bring `pr`'s review threads into `target`, then push what changed here.
pub fn sync_pr(cwd: &Path, target: &Target, pr: &PrRef) -> Result<Synced> {
    let api = api()?;
    let mut out = Synced {
        repo: pr.repo.clone(),
        pr: pr.number,
        ..Default::default()
    };
    let pull = api.call(
        "GET",
        &format!("/repos/{}/pulls/{}", pr.repo, pr.number),
        None,
    )?;
    let head_sha = pull["head"]["sha"].as_str().unwrap_or("").to_string();
    let remote = fetch_threads(&api, pr)?;

    // New threads need their anchors read before taking the lock.
    let known: Vec<u64> = read_store(&store_path(cwd)?)?
        .threads
        .iter()
        .flat_map(|t| t.comments.iter().filter_map(|c| c.github_id))
        .collect();
    let anchors: HashMap<String, Option<Anchor>> = remote
        .iter()
        .filter(|g| g.comments.first().is_some_and(|c| !known.contains(&c.id)))
        .map(|g| (g.id.clone(), gh_anchor(cwd, target, &head_sha, g)))
        .collect();

    let at = now();
    change(&store_path(cwd)?, |s| {
        for g in &remote {
            let Some(first) = g.comments.first() else {
                continue;
            };
            let found = s.threads.iter_mut().find(|t| {
                t.target == *target
                    && t.github.as_ref().is_some_and(|l| {
                        l.thread.as_deref() == Some(g.id.as_str()) || l.comment == Some(first.id)
                    })
            });
            match found {
                Some(t) => {
                    let mut touched = false;
                    let link = t.github.get_or_insert_with(|| GithubLink {
                        pr: pr.number,
                        ..Default::default()
                    });
                    if link.thread.is_none() {
                        link.thread = Some(g.id.clone());
                    }
                    link.repo.get_or_insert_with(|| pr.repo.clone());
                    link.comment.get_or_insert(first.id);
                    if link.url.is_none() {
                        link.url = first.url.clone();
                    }
                    if Some(g.resolved) != link.resolved {
                        let was = link.resolved;
                        link.resolved = Some(g.resolved);
                        // Changed on GitHub since we last looked (a thread we
                        // posted starts as not resolved).
                        if was.is_some() || g.resolved {
                            if g.resolved && t.state != State::Resolved {
                                t.state = State::Resolved;
                                t.state_by = Some("github".into());
                                touched = true;
                            } else if !g.resolved && t.state == State::Resolved {
                                t.state = State::Open;
                                t.state_by = Some("github".into());
                                touched = true;
                            }
                        }
                    }
                    for c in &g.comments {
                        if t.comments.iter().any(|x| x.github_id == Some(c.id)) {
                            continue;
                        }
                        let next = t.comments.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                        t.comments.push(Comment {
                            id: next,
                            author: c.author.clone(),
                            body: c.body.clone(),
                            at: c.at,
                            github_id: Some(c.id),
                        });
                        // A reviewer's reply on GitHub reopens an addressed thread.
                        if t.state == State::Addressed {
                            t.state = State::Open;
                            t.state_by = Some(c.author.clone());
                        }
                        touched = true;
                    }
                    if touched {
                        t.updated_at = at;
                        out.updated.push(t.id.clone());
                    }
                }
                None => {
                    if known.contains(&first.id) {
                        continue; // on another target already
                    }
                    s.next_thread += 1;
                    let id = format!("c{}", s.next_thread);
                    let comments = g
                        .comments
                        .iter()
                        .enumerate()
                        .map(|(i, c)| Comment {
                            id: i as u32 + 1,
                            author: c.author.clone(),
                            body: c.body.clone(),
                            at: c.at,
                            github_id: Some(c.id),
                        })
                        .collect();
                    s.threads.push(Thread {
                        id: id.clone(),
                        target: target.clone(),
                        anchor: anchors.get(&g.id).cloned().flatten(),
                        state: if g.resolved {
                            State::Resolved
                        } else {
                            State::Open
                        },
                        comments,
                        source: "github".into(),
                        github: Some(GithubLink {
                            repo: Some(pr.repo.clone()),
                            pr: pr.number,
                            thread: Some(g.id.clone()),
                            comment: Some(first.id),
                            url: first.url.clone(),
                            resolved: Some(g.resolved),
                            error: None,
                        }),
                        review: None,
                        state_by: g.resolved.then(|| "github".to_string()),
                        created_at: first.at,
                        updated_at: at,
                        current: None,
                        outdated: false,
                    });
                    out.imported.push(id);
                }
            }
        }
        Ok(())
    })?;

    push(&api, cwd, target, None, &mut out)?;
    Ok(out)
}

/// Push local replies and states of `target`'s GitHub threads (or only
/// `only`). Failures are kept on the thread, not returned.
fn push(
    api: &Api,
    cwd: &Path,
    target: &Target,
    only: Option<&str>,
    out: &mut Synced,
) -> Result<()> {
    let file = store_path(cwd)?;
    let todo: Vec<Thread> = read_store(&file)?
        .threads
        .into_iter()
        .filter(|t| t.target == *target && only.is_none_or(|id| t.id == id))
        .filter(|t| t.state != State::Pending && t.github.is_some())
        .collect();
    for t in todo {
        let Some(link) = t.github.clone() else {
            continue;
        };
        let Some(repo) = link.repo.clone() else {
            continue;
        };
        let mut posted: Vec<(u32, u64)> = Vec::new();
        let mut resolved = link.resolved;
        let mut error = None;
        if let Some(first) = link.comment {
            for c in t.comments.iter().skip(1).filter(|c| c.github_id.is_none()) {
                let r = api.call(
                    "POST",
                    &format!("/repos/{repo}/pulls/{}/comments/{first}/replies", link.pr),
                    Some(json!({ "body": agent_footer(c) })),
                );
                match r {
                    Ok(v) => posted.push((c.id, v["id"].as_u64().unwrap_or(0))),
                    Err(e) => {
                        error = Some(e.to_string());
                        break;
                    }
                }
            }
        }
        let want = t.state == State::Resolved;
        if error.is_none() && Some(want) != resolved.or(Some(false)) {
            if let Some(node) = &link.thread {
                let m = if want {
                    "resolveReviewThread"
                } else {
                    "unresolveReviewThread"
                };
                let q = format!(
                    "mutation($id: ID!) {{ {m}(input: {{threadId: $id}}) {{ thread {{ isResolved }} }} }}"
                );
                match api.call(
                    "POST",
                    "/graphql",
                    Some(json!({ "query": q, "variables": { "id": node } })),
                ) {
                    Ok(v) if v["errors"].is_null() => resolved = Some(want),
                    Ok(v) => error = Some(format!("GitHub: {}", v["errors"])),
                    Err(e) => error = Some(e.to_string()),
                }
            }
        }
        let changed = !posted.is_empty() || resolved != link.resolved;
        if changed || error != link.error {
            change(&file, |s| {
                let x = find(s, &t.id)?;
                for (cid, gid) in &posted {
                    if let Some(c) = x.comments.iter_mut().find(|c| c.id == *cid) {
                        c.github_id = Some(*gid);
                    }
                }
                if let Some(l) = x.github.as_mut() {
                    l.resolved = resolved;
                    l.error = error.clone();
                }
                Ok(())
            })?;
        }
        match error {
            Some(e) => out.errors.push(format!("{}: {e}", t.id)),
            None if changed => out.pushed.push(t.id.clone()),
            None => {}
        }
    }
    Ok(())
}

/// Sync `target` with its PR: pull GitHub's review threads, push local
/// replies and states.
pub fn sync(cwd: &Path, target: &Target) -> Result<Synced> {
    let pr = pr_of(cwd, target)?;
    sync_pr(cwd, target, &pr)
}

/// After a local reply or state change on thread `id`: push it to GitHub
/// when the thread is there. Never fails; errors stay on the thread.
pub(super) fn push_soon(cwd: &Path, t: &Thread) {
    let Some(link) = &t.github else { return };
    if link.repo.is_none() {
        return;
    }
    let mut out = Synced::default();
    let r = match link.thread {
        // Without the thread's node id it can't be resolved: pull first.
        None if t.state == State::Resolved => {
            let pr = PrRef {
                repo: link.repo.clone().unwrap_or_default(),
                number: link.pr,
            };
            sync_pr(cwd, &t.target, &pr).map(|_| ())
        }
        _ => api().and_then(|api| push(&api, cwd, &t.target, Some(&t.id), &mut out)),
    };
    if let Err(e) = r {
        let _ = change(&store_path(cwd).unwrap_or_default(), |s| {
            if let Some(l) = find(s, &t.id)?.github.as_mut() {
                l.error = Some(e.to_string());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_times() {
        assert_eq!(rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339("2026-11-27T10:00:00Z"), Some(1_795_773_600));
        assert_eq!(rfc3339("2000-02-29T12:30:15.5Z"), Some(951_827_415));
        assert_eq!(rfc3339("nope"), None);
    }

    #[test]
    fn hunk_ranges() {
        let p = "@@ -3,6 +3,9 @@ fn x\n a\n+b\n@@ -20 +23,0 @@\n-c\n";
        assert_eq!(hunks(p), vec![((3, 8), (3, 11)), ((20, 20), (23, 23))]);
    }
}
