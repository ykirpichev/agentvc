//! PR feedback in the attempt (#76): checks, reviews, review comments and
//! the merge, pulled back from GitHub after `agentvc pr`.
//!
//! `agentvc pr --refresh [aN]` (and `agentvcd`, every few minutes while a PR
//! is open) fetches the PR's state, its head commit's check runs and
//! statuses, the latest review of each reviewer and the review threads with
//! their comments, path, line and whether they are resolved, and stores them
//! in the space, keyed by PR URL. `status aN` shows the summary, the Inbox
//! lists failed checks and change requests, and `agentvc comments [aN]`
//! (MCP `comments`) hands the threads to the next agent: from a fork of the
//! PR's attempt it finds the PR through the attempts it was forked from.
//!
//! A refresh costs four API calls (PR, check runs, statuses, reviews) plus
//! one GraphQL query for the threads; when GraphQL is unavailable it falls
//! back to the REST review comments, without resolution state. Rate limits
//! come back as `rate_limited` with the reset time, and the daemon waits
//! until then. When a PR merges, the attempt records the merge commit
//! (operation `attempt.merged`) and the PR drops out of `status`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::github::{self, Api};
use crate::space::{now, Space};

/// Space metadata key prefix for stored feedback; the PR URL follows.
const META_PREFIX: &str = "pr_feedback:";

/// Check results of the PR's head commit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checks {
    /// `pass`, `fail`, `pending`, or `none` when there are no checks.
    pub state: String,
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
    /// Names of failing checks.
    pub failing: Vec<String>,
    /// Names of checks still running or queued.
    pub running: Vec<String>,
}

/// A reviewer's latest review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub author: String,
    /// `approved`, `changes_requested`, `commented` or `dismissed`.
    pub state: String,
    pub body: String,
    pub submitted_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub url: Option<String>,
    pub created_at: Option<String>,
}

/// A review comment thread on a line of the diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub path: String,
    pub line: Option<u64>,
    /// `None` when GitHub did not say (REST fallback).
    pub resolved: Option<bool>,
    /// The line changed since the comment was written.
    pub outdated: bool,
    pub comments: Vec<Comment>,
}

impl Thread {
    /// Still needs an answer: not resolved and not outdated.
    pub fn is_open(&self) -> bool {
        self.resolved != Some(true) && !self.outdated
    }
}

/// What GitHub says about an attempt's PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFeedback {
    pub url: String,
    pub repo: String,
    pub number: u64,
    /// `open`, `closed` or `merged`.
    pub state: String,
    pub draft: bool,
    /// GitHub's `mergeable` (`None` while it is still computing it).
    #[serde(default)]
    pub mergeable: Option<bool>,
    /// `clean`, `blocked`, `behind`, `dirty`, `unstable`, ...
    #[serde(default)]
    pub mergeable_state: Option<String>,
    pub head_sha: Option<String>,
    pub merge_commit: Option<String>,
    pub merged_at: Option<String>,
    pub checks: Checks,
    /// Latest review per reviewer.
    pub reviews: Vec<Review>,
    /// `changes_requested` when any reviewer's latest review requests
    /// changes, else `approved` when one approved, else `none`.
    pub decision: String,
    pub threads: Vec<Thread>,
    /// Threads that are neither resolved nor outdated.
    pub unresolved: usize,
    pub refreshed_at: i64,
}

impl PrFeedback {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    /// One line for `status`.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.state.clone()];
        if self.draft {
            parts.push("draft".into());
        }
        parts.push(match self.checks.state.as_str() {
            "fail" => format!("checks fail: {}", self.checks.failing.join(", ")),
            "pending" => format!("checks running: {}", self.checks.running.join(", ")),
            "pass" => format!("checks pass ({})", self.checks.passed),
            _ => "no checks".into(),
        });
        match self.decision.as_str() {
            "changes_requested" => {
                let who: Vec<&str> = self
                    .reviews
                    .iter()
                    .filter(|r| r.state == "changes_requested")
                    .map(|r| r.author.as_str())
                    .collect();
                parts.push(format!("changes requested by {}", who.join(", ")));
            }
            "approved" => parts.push("approved".into()),
            _ => {}
        }
        if self.unresolved > 0 {
            parts.push(format!(
                "{} unresolved comment{}",
                self.unresolved,
                if self.unresolved == 1 { "" } else { "s" }
            ));
        }
        if let Some(c) = &self.merge_commit {
            if self.state == "merged" {
                parts.push(format!("merge commit {}", &c[..12.min(c.len())]));
            }
        }
        parts.join(", ")
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn checks_of(runs: &Value, statuses: &Value) -> Checks {
    let mut c = Checks::default();
    let mut add = |name: String, outcome: &str| match outcome {
        "pass" => c.passed += 1,
        "fail" => {
            c.failed += 1;
            c.failing.push(name);
        }
        _ => {
            c.pending += 1;
            c.running.push(name);
        }
    };
    for r in runs["check_runs"].as_array().into_iter().flatten() {
        let outcome = if r["status"].as_str() != Some("completed") {
            "pending"
        } else {
            match r["conclusion"].as_str().unwrap_or("") {
                "success" | "neutral" | "skipped" => "pass",
                _ => "fail",
            }
        };
        add(s(&r["name"]), outcome);
    }
    for st in statuses["statuses"].as_array().into_iter().flatten() {
        let outcome = match st["state"].as_str().unwrap_or("") {
            "success" => "pass",
            "failure" | "error" => "fail",
            _ => "pending",
        };
        add(s(&st["context"]), outcome);
    }
    c.state = if c.failed > 0 {
        "fail"
    } else if c.pending > 0 {
        "pending"
    } else if c.passed > 0 {
        "pass"
    } else {
        "none"
    }
    .into();
    c
}

fn reviews_of(reviews: &Value) -> Vec<Review> {
    // Latest review per author; a plain comment does not replace an
    // approval or a change request.
    let mut latest: Vec<Review> = Vec::new();
    for r in reviews.as_array().into_iter().flatten() {
        let state = s(&r["state"]).to_ascii_lowercase();
        if state == "pending" {
            continue;
        }
        let review = Review {
            author: s(&r["user"]["login"]),
            state: state.clone(),
            body: s(&r["body"]),
            submitted_at: r["submitted_at"].as_str().map(str::to_string),
        };
        match latest.iter_mut().find(|x| x.author == review.author) {
            Some(x) if state != "commented" || x.state == "commented" => *x = review,
            Some(_) => {}
            None => latest.push(review),
        }
    }
    latest
}

const THREADS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { \
  repository(owner: $owner, name: $name) { pullRequest(number: $number) { \
    reviewThreads(first: 100) { nodes { isResolved isOutdated path line originalLine \
      comments(first: 50) { nodes { author { login } body url createdAt } } } } } } }";

fn threads_graphql(api: &Api, repo: &str, number: u64) -> Result<Vec<Thread>> {
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    let v = api.call(
        "POST",
        "/graphql",
        Some(json!({
            "query": THREADS_QUERY,
            "variables": { "owner": owner, "name": name, "number": number },
        })),
    )?;
    let nodes = &v["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"];
    let Some(nodes) = nodes.as_array() else {
        return Err(Error::GitHub {
            status: 200,
            message: format!("unexpected GraphQL reply: {}", v["errors"]),
        });
    };
    Ok(nodes
        .iter()
        .map(|t| Thread {
            path: s(&t["path"]),
            line: t["line"].as_u64().or(t["originalLine"].as_u64()),
            resolved: t["isResolved"].as_bool(),
            outdated: t["isOutdated"].as_bool().unwrap_or(false),
            comments: t["comments"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| Comment {
                    author: s(&c["author"]["login"]),
                    body: s(&c["body"]),
                    url: c["url"].as_str().map(str::to_string),
                    created_at: c["createdAt"].as_str().map(str::to_string),
                })
                .collect(),
        })
        .collect())
}

/// Review comments from REST, grouped into threads by `in_reply_to_id`.
fn threads_rest(api: &Api, repo: &str, number: u64) -> Result<Vec<Thread>> {
    let v = api.call(
        "GET",
        &format!("/repos/{repo}/pulls/{number}/comments?per_page=100"),
        None,
    )?;
    let mut threads: Vec<(u64, Thread)> = Vec::new();
    for c in v.as_array().into_iter().flatten() {
        let comment = Comment {
            author: s(&c["user"]["login"]),
            body: s(&c["body"]),
            url: c["html_url"].as_str().map(str::to_string),
            created_at: c["created_at"].as_str().map(str::to_string),
        };
        if let Some(parent) = c["in_reply_to_id"].as_u64() {
            if let Some((_, t)) = threads.iter_mut().find(|(id, _)| *id == parent) {
                t.comments.push(comment);
                continue;
            }
        }
        threads.push((
            c["id"].as_u64().unwrap_or(0),
            Thread {
                path: s(&c["path"]),
                line: c["line"].as_u64().or(c["original_line"].as_u64()),
                resolved: None,
                outdated: c["line"].is_null() && c["position"].is_null(),
                comments: vec![comment],
            },
        ));
    }
    Ok(threads.into_iter().map(|(_, t)| t).collect())
}

/// Fetch the feedback on the PR at `url`.
pub fn fetch(url: &str) -> Result<PrFeedback> {
    let (repo, number) = github::parse_pr_url(url).ok_or_else(|| Error::NotFound {
        kind: "pull request",
        id: url.to_string(),
    })?;
    fetch_with(&Api::new(github::token()?), &repo, number)
}

/// [`fetch`] with a given client, for PR `number` of `repo` (owner/name).
pub(crate) fn fetch_with(api: &Api, repo: &str, number: u64) -> Result<PrFeedback> {
    let url = format!("https://github.com/{repo}/pull/{number}");
    let repo = repo.to_string();
    let pr = api.call("GET", &format!("/repos/{repo}/pulls/{number}"), None)?;
    let head_sha = pr["head"]["sha"].as_str().map(str::to_string);
    let (runs, statuses) = match &head_sha {
        Some(sha) => (
            api.call(
                "GET",
                &format!("/repos/{repo}/commits/{sha}/check-runs?per_page=100"),
                None,
            )?,
            api.call("GET", &format!("/repos/{repo}/commits/{sha}/status"), None)?,
        ),
        None => (Value::Null, Value::Null),
    };
    let reviews = reviews_of(&api.call(
        "GET",
        &format!("/repos/{repo}/pulls/{number}/reviews?per_page=100"),
        None,
    )?);
    let threads = match threads_graphql(api, &repo, number) {
        Ok(t) => t,
        Err(e @ Error::RateLimited { .. }) => return Err(e),
        Err(_) => threads_rest(api, &repo, number).unwrap_or_default(),
    };
    let merged = pr["merged"].as_bool().unwrap_or(false) || !pr["merged_at"].is_null();
    let state = if merged {
        "merged".to_string()
    } else {
        s(&pr["state"])
    };
    let decision = if reviews.iter().any(|r| r.state == "changes_requested") {
        "changes_requested"
    } else if reviews.iter().any(|r| r.state == "approved") {
        "approved"
    } else {
        "none"
    };
    Ok(PrFeedback {
        url,
        repo,
        number,
        state,
        draft: pr["draft"].as_bool().unwrap_or(false),
        mergeable: pr["mergeable"].as_bool(),
        mergeable_state: pr["mergeable_state"].as_str().map(str::to_string),
        head_sha,
        merge_commit: merged
            .then(|| pr["merge_commit_sha"].as_str().map(str::to_string))
            .flatten(),
        merged_at: pr["merged_at"].as_str().map(str::to_string),
        checks: checks_of(&runs, &statuses),
        decision: decision.into(),
        unresolved: threads.iter().filter(|t| t.is_open()).count(),
        reviews,
        threads,
        refreshed_at: now(),
    })
}

/// An attempt's PR, or the PR of the nearest attempt it was forked from
/// (a follow-up attempt continues its parent's PR).
pub fn pr_url_for(space: &Space, attempt: i64) -> Result<Option<(i64, String)>> {
    let mut cur = Some(attempt);
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cur {
        if !seen.insert(id) {
            break;
        }
        if let Some(url) = space.pr_url(id)? {
            return Ok(Some((id, url)));
        }
        cur = space.attempt(id)?.parent_attempt;
    }
    Ok(None)
}

/// A PR URL, the attempts exported to it (newest first), and its stored
/// feedback.
pub type OpenPrRow = (String, Vec<i64>, Option<PrFeedback>);

#[derive(Clone, Debug, Serialize)]
pub struct Refreshed {
    /// Attempts whose PR this is (the latest export first).
    pub attempts: Vec<i64>,
    pub feedback: PrFeedback,
    /// True when this refresh saw the PR merge.
    pub newly_merged: bool,
}

impl Space {
    /// The stored feedback on the PR at `url`, from the last refresh.
    pub fn feedback_for_url(&self, url: &str) -> Result<Option<PrFeedback>> {
        Ok(self
            .meta(&format!("{META_PREFIX}{url}"))?
            .and_then(|v| serde_json::from_str(&v).ok()))
    }

    /// The stored feedback on an attempt's PR (or its ancestors' PR).
    pub fn pr_feedback(&self, attempt: i64) -> Result<Option<PrFeedback>> {
        match pr_url_for(self, attempt)? {
            Some((_, url)) => self.feedback_for_url(&url),
            None => Ok(None),
        }
    }

    /// Store feedback; on the first sight of a merge, record it.
    pub fn store_feedback(&self, attempts: &[i64], f: &PrFeedback) -> Result<bool> {
        let before = self.feedback_for_url(&f.url)?;
        self.set_meta(
            &format!("{META_PREFIX}{}", f.url),
            &serde_json::to_string(f).expect("serializable"),
        )?;
        let newly_merged =
            f.state == "merged" && before.as_ref().is_none_or(|b| b.state != "merged");
        if newly_merged {
            for &a in attempts {
                self.record(
                    "github",
                    "attempt.merged",
                    json!({ "attempt": a, "pr": f.url, "merge_commit": f.merge_commit }),
                )?;
            }
        }
        Ok(newly_merged)
    }

    /// Every distinct PR URL with the attempts exported to it, newest first.
    pub(crate) fn prs(&self) -> Result<Vec<(String, Vec<i64>)>> {
        let mut stmt = self.db.prepare(
            "SELECT pr_url, attempt_id FROM exports WHERE pr_url IS NOT NULL ORDER BY id DESC",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut out: Vec<(String, Vec<i64>)> = Vec::new();
        for (url, a) in rows {
            match out.iter_mut().find(|(u, _)| *u == url) {
                Some((_, ids)) => {
                    if !ids.contains(&a) {
                        ids.push(a)
                    }
                }
                None => out.push((url, vec![a])),
            }
        }
        Ok(out)
    }

    /// PRs not known to be merged or closed, with their attempts and the
    /// stored feedback (`None` before the first refresh).
    pub fn open_prs(&self) -> Result<Vec<OpenPrRow>> {
        let mut out = Vec::new();
        for (url, ids) in self.prs()? {
            let f = self.feedback_for_url(&url)?;
            if f.as_ref().is_none_or(PrFeedback::is_open) {
                out.push((url, ids, f));
            }
        }
        Ok(out)
    }
}

/// Refresh one attempt's PR (found through its ancestors if needed).
pub fn refresh(space: &Space, attempt: i64) -> Result<Refreshed> {
    let (owner, url) = pr_url_for(space, attempt)?.ok_or(Error::NoPr(attempt))?;
    let attempts = space
        .prs()?
        .into_iter()
        .find(|(u, _)| *u == url)
        .map(|(_, ids)| ids)
        .unwrap_or_else(|| vec![owner]);
    let feedback = fetch(&url)?;
    let newly_merged = space.store_feedback(&attempts, &feedback)?;
    Ok(Refreshed {
        attempts,
        feedback,
        newly_merged,
    })
}

/// Refresh every PR that is not known to be merged or closed. Stops at the
/// first rate-limit error, returning it with what was refreshed so far.
pub fn refresh_open(space: &Space) -> (Vec<Refreshed>, Option<Error>) {
    let prs = match space.open_prs() {
        Ok(p) => p,
        Err(e) => return (Vec::new(), Some(e)),
    };
    let mut done = Vec::new();
    let mut err = None;
    for (url, attempts, _) in prs {
        match fetch(&url) {
            Ok(feedback) => match space.store_feedback(&attempts, &feedback) {
                Ok(newly_merged) => done.push(Refreshed {
                    attempts,
                    feedback,
                    newly_merged,
                }),
                Err(e) => err = Some(e),
            },
            Err(e @ Error::RateLimited { .. }) => return (done, Some(e)),
            Err(e) => err = Some(e),
        }
    }
    (done, err)
}

/// The review comments to hand to an agent, as Markdown: change requests
/// and open threads (or all threads with `all`).
pub fn comments_markdown(f: &PrFeedback, all: bool) -> String {
    let mut out = format!(
        "# Review of {} (PR #{})\n\n{}\n",
        f.url,
        f.number,
        f.summary()
    );
    for r in f
        .reviews
        .iter()
        .filter(|r| r.state == "changes_requested" && !r.body.trim().is_empty())
    {
        out += &format!(
            "\n## Changes requested by {}\n\n{}\n",
            r.author,
            r.body.trim()
        );
    }
    let threads: Vec<&Thread> = f.threads.iter().filter(|t| all || t.is_open()).collect();
    for t in threads {
        let at = match t.line {
            Some(l) => format!("{}:{l}", t.path),
            None => t.path.clone(),
        };
        let tag = match (t.resolved, t.outdated) {
            (Some(true), _) => " (resolved)",
            (_, true) => " (outdated)",
            _ => "",
        };
        out += &format!("\n## {at}{tag}\n");
        for c in &t.comments {
            out += &format!("\n**{}**: {}\n", c.author, c.body.trim());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_come_back_with_their_reset_time() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = r#"{"message":"API rate limit exceeded"}"#;
            write!(
                stream,
                "HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 1800000000\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let mut api = Api::new("t".into());
        api.set_base(&base);
        let e = api.call("GET", "/repos/o/r/pulls/1", None).unwrap_err();
        assert_eq!(e.code(), "rate_limited");
        assert!(matches!(
            e,
            Error::RateLimited {
                reset_at: Some(1800000000)
            }
        ));
    }

    #[test]
    fn stored_feedback_follows_forks_and_records_the_merge_once() {
        let tmp = crate::testutil::tempdir();
        std::fs::write(tmp.path().join("f"), "x").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let fork = |from: i64| {
            space
                .fork(&crate::space::ForkRequest {
                    from: Some(crate::space::Ref::Attempt(from)),
                    actor: "agent".into(),
                    ..Default::default()
                })
                .unwrap()
                .attempt
                .id
        };
        let a = fork(1);
        let child = fork(a);
        let url = "https://github.com/o/r/pull/7";
        let snap = space.head_snapshot(a).unwrap().id;
        space
            .db
            .execute(
                "INSERT INTO exports (attempt_id, snapshot_id, branch, commit_id, pr_url, created_at)
                 VALUES (?1, ?2, 'b', 'c', ?3, 0)",
                rusqlite::params![a, snap, url],
            )
            .unwrap();
        assert_eq!(
            pr_url_for(&space, child).unwrap(),
            Some((a, url.to_string()))
        );
        assert_eq!(space.open_prs().unwrap().len(), 1);
        let mut f: PrFeedback = serde_json::from_value(json!({
            "url": url, "repo": "o/r", "number": 7, "state": "open", "draft": false,
            "head_sha": null, "merge_commit": null, "merged_at": null,
            "checks": { "state": "fail", "passed": 0, "failed": 1, "pending": 0,
                        "failing": ["rust"], "running": [] },
            "reviews": [], "decision": "none", "threads": [], "unresolved": 0, "refreshed_at": 0,
        }))
        .unwrap();
        assert!(!space.store_feedback(&[a], &f).unwrap());
        assert_eq!(
            space.pr_feedback(child).unwrap().unwrap().checks.failing,
            ["rust"]
        );
        f.state = "merged".into();
        f.merge_commit = Some("m".into());
        assert!(space.store_feedback(&[a], &f).unwrap());
        assert!(!space.store_feedback(&[a], &f).unwrap());
        assert!(space.open_prs().unwrap().is_empty());
        let merged: i64 = space
            .db
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE kind = 'attempt.merged'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(merged, 1);
    }

    #[test]
    fn checks_and_reviews_are_summarised() {
        let runs = json!({ "check_runs": [
            { "name": "rust", "status": "completed", "conclusion": "failure" },
            { "name": "ui", "status": "completed", "conclusion": "success" },
            { "name": "docs", "status": "in_progress", "conclusion": null },
        ]});
        let statuses = json!({ "statuses": [{ "context": "ci/legacy", "state": "success" }] });
        let c = checks_of(&runs, &statuses);
        assert_eq!(c.state, "fail");
        assert_eq!((c.passed, c.failed, c.pending), (2, 1, 1));
        assert_eq!(c.failing, ["rust"]);
        assert_eq!(c.running, ["docs"]);

        let reviews = reviews_of(&json!([
            { "user": { "login": "ana" }, "state": "CHANGES_REQUESTED", "body": "fix x" },
            { "user": { "login": "ana" }, "state": "COMMENTED", "body": "also" },
            { "user": { "login": "bo" }, "state": "APPROVED", "body": "" },
        ]));
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].state, "changes_requested");
        assert_eq!(reviews[0].body, "fix x");
    }

    #[test]
    fn markdown_lists_open_threads_and_change_requests() {
        let f = PrFeedback {
            url: "https://github.com/o/r/pull/7".into(),
            repo: "o/r".into(),
            number: 7,
            state: "open".into(),
            draft: false,
            mergeable: None,
            mergeable_state: None,
            head_sha: None,
            merge_commit: None,
            merged_at: None,
            checks: Checks {
                state: "none".into(),
                ..Default::default()
            },
            reviews: vec![Review {
                author: "ana".into(),
                state: "changes_requested".into(),
                body: "Please split this".into(),
                submitted_at: None,
            }],
            decision: "changes_requested".into(),
            threads: vec![
                Thread {
                    path: "src/a.rs".into(),
                    line: Some(3),
                    resolved: Some(false),
                    outdated: false,
                    comments: vec![Comment {
                        author: "ana".into(),
                        body: "off by one".into(),
                        url: None,
                        created_at: None,
                    }],
                },
                Thread {
                    path: "src/b.rs".into(),
                    line: Some(9),
                    resolved: Some(true),
                    outdated: false,
                    comments: vec![],
                },
            ],
            unresolved: 1,
            refreshed_at: 0,
        };
        let md = comments_markdown(&f, false);
        assert!(md.contains("## Changes requested by ana\n\nPlease split this"));
        assert!(md.contains("## src/a.rs:3\n\n**ana**: off by one"));
        assert!(!md.contains("src/b.rs"));
        assert!(comments_markdown(&f, true).contains("src/b.rs:9 (resolved)"));
        assert_eq!(
            f.summary(),
            "open, no checks, changes requested by ana, 1 unresolved comment"
        );
    }
}
