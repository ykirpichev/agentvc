//! Test results on snapshots (#11): `agentvc check aN -- <cmd>`.
//!
//! A check snapshots the view, runs the command there and stores the result
//! on that snapshot: pass or fail, exit code, duration, test counts and the
//! end of the output. Results are keyed by tree: the same tree and the same
//! command reuse an earlier result (from any attempt) instead of running
//! again, unless forced.
//!
//! What an attempt's checks say is read against its head: a result recorded
//! on an older tree is `stale`. That is how a rebase shows up (#103): the
//! rebased head has no result yet, the old one is shown as stale until the
//! check runs again, which works on a promoted attempt too.
//!
//! Test counts come from JUnit XML files (`--junit`) or, without them, from
//! the summary lines cargo test and pytest print.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::objects::Oid;
use crate::run::{ENV_ATTEMPT, ENV_SPACE, ENV_VIEW};
use crate::space::{now, Space, Status, SPACE_DIR};

/// The name a check gets by default; the PR body's "Tests" line reads it.
pub const DEFAULT_NAME: &str = "tests";
/// How much of a check's output is kept on the snapshot (and synced).
const EXCERPT_LINES: usize = 40;
const EXCERPT_BYTES: usize = 4000;

#[derive(Clone, Debug)]
pub struct CheckOptions {
    pub actor: String,
    pub name: String,
    /// Shell command, run with `sh -c` in the view.
    pub command: String,
    /// JUnit XML files or globs, relative to the view.
    pub junit: Vec<String>,
    /// Run even when a result for this tree and command exists.
    pub force: bool,
    /// Mark an open attempt passed or failed by the result.
    pub mark: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct TestCounts {
    pub passed: i64,
    pub failed: i64,
    pub skipped: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub id: i64,
    pub attempt_id: i64,
    pub snapshot_id: i64,
    pub tree: String,
    pub name: String,
    pub command: String,
    pub exit_code: i32,
    pub passed: bool,
    pub duration_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,
    /// The earlier check whose result this one reuses (same tree and command).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<i64>,
    pub actor: String,
    pub created_at: i64,
}

impl Check {
    pub fn outcome(&self) -> &'static str {
        if self.passed {
            "pass"
        } else {
            "fail"
        }
    }

    /// `pass · 120 passed, 1 skipped · 2m 03s`.
    pub fn line(&self) -> String {
        let mut s = self.outcome().to_string();
        if let Some(t) = &self.tests {
            s += &format!(" · {}", counts_text(t));
        }
        s += &format!(" · {}", duration_text(self.duration_ms));
        s
    }
}

pub fn counts_text(t: &TestCounts) -> String {
    let mut parts = vec![format!("{} passed", t.passed)];
    if t.failed > 0 {
        parts.push(format!("{} failed", t.failed));
    }
    if t.skipped > 0 {
        parts.push(format!("{} skipped", t.skipped));
    }
    parts.join(", ")
}

pub fn duration_text(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        0 => format!("{ms} ms"),
        s if s < 60 => format!("{s}s"),
        s => format!("{}m {:02}s", s / 60, s % 60),
    }
}

/// The latest check of one name on an attempt, read against its head.
#[derive(Clone, Debug, Serialize)]
pub struct CheckStatus {
    #[serde(flatten)]
    pub check: Check,
    /// Recorded on an older tree than the attempt's head.
    pub stale: bool,
}

impl CheckStatus {
    /// For the PR body and `show`: `pass · 12 passed · 3s` or
    /// `pass on s29, before the latest changes (stale)`.
    pub fn text(&self) -> String {
        if self.stale {
            format!(
                "{} on s{}, before the latest changes (stale)",
                self.check.line(),
                self.check.snapshot_id
            )
        } else {
            self.check.line()
        }
    }
}

const COLS: &str = "id, attempt_id, snapshot_id, tree, name, command, exit_code, duration_ms, \
                    tests_passed, tests_failed, tests_skipped, log_excerpt, reused_from, actor, \
                    created_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Check> {
    let exit_code: i32 = r.get(6)?;
    let counts: (Option<i64>, Option<i64>, Option<i64>) = (r.get(8)?, r.get(9)?, r.get(10)?);
    Ok(Check {
        id: r.get(0)?,
        attempt_id: r.get(1)?,
        snapshot_id: r.get(2)?,
        tree: r.get(3)?,
        name: r.get(4)?,
        command: r.get(5)?,
        exit_code,
        passed: exit_code == 0,
        duration_ms: r.get(7)?,
        tests: match counts {
            (None, None, None) => None,
            (p, f, s) => Some(TestCounts {
                passed: p.unwrap_or(0),
                failed: f.unwrap_or(0),
                skipped: s.unwrap_or(0),
            }),
        },
        log_excerpt: r.get(11)?,
        reused_from: r.get(12)?,
        actor: r.get(13)?,
        created_at: r.get(14)?,
    })
}

/// What a [`Space::run_check`] call did.
#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    pub check: Check,
    /// The result came from an earlier run on the same tree.
    pub cached: bool,
    /// Where the whole output is (this machine only); `None` when cached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// The attempt's status after the check.
    pub status: Status,
}

/// The last lines of `out`, at most [`EXCERPT_BYTES`].
pub fn excerpt(out: &str) -> Option<String> {
    let lines: Vec<&str> = out.lines().collect();
    let tail = lines[lines.len().saturating_sub(EXCERPT_LINES)..].join("\n");
    let mut start = tail.len().saturating_sub(EXCERPT_BYTES);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    let t = tail[start..].trim_matches('\n');
    (!t.trim().is_empty()).then(|| t.to_string())
}

/// Test counts from the summary lines cargo test (`test result: ok. 3
/// passed; 0 failed; 1 ignored`, summed over binaries) and pytest (`== 3
/// passed, 1 failed, 2 skipped in 0.1s ==`) print.
pub fn counts_from_output(out: &str) -> Option<TestCounts> {
    let mut t = TestCounts::default();
    let mut any = false;
    let num_before = |s: &str, word: &str| -> Option<i64> {
        let i = s.find(word)?;
        s[..i]
            .trim_end()
            .rsplit([' ', ';', ',', '='])
            .next()?
            .parse()
            .ok()
    };
    for line in out.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("test result: ") {
            any = true;
            for part in rest.split(';') {
                if let Some(n) = num_before(part, " passed") {
                    t.passed += n;
                } else if let Some(n) = num_before(part, " failed") {
                    t.failed += n;
                } else if let Some(n) = num_before(part, " ignored") {
                    t.skipped += n;
                }
            }
        } else if l.starts_with('=') && l.ends_with('=') && l.contains(" in ") {
            let body = l.trim_matches('=').trim();
            let body = body.split(" in ").next().unwrap_or("");
            let mut hit = false;
            for part in body.split(',') {
                let part = format!(" {}", part.trim());
                if let Some(n) = num_before(&part, " passed") {
                    t.passed += n;
                    hit = true;
                } else if let Some(n) = num_before(&part, " failed") {
                    t.failed += n;
                    hit = true;
                } else if let Some(n) = num_before(&part, " error") {
                    t.failed += n;
                    hit = true;
                } else if let Some(n) = num_before(&part, " skipped") {
                    t.skipped += n;
                    hit = true;
                }
            }
            any |= hit;
        }
    }
    any.then_some(t)
}

/// Test counts from JUnit XML: every `<testcase>`, failed when it holds a
/// `<failure>` or `<error>`, skipped when it holds `<skipped>`.
pub fn counts_from_junit(xml: &str) -> TestCounts {
    let mut t = TestCounts::default();
    let mut rest = xml;
    while let Some(i) = rest.find("<testcase") {
        rest = &rest[i + "<testcase".len()..];
        // Not `<testcases>` or similar.
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            continue;
        }
        let Some(end) = rest.find('>') else { break };
        let body = if rest[..end].ends_with('/') {
            ""
        } else {
            let close = rest.find("</testcase>").unwrap_or(rest.len());
            &rest[end..close]
        };
        if body.contains("<failure") || body.contains("<error") {
            t.failed += 1;
        } else if body.contains("<skipped") {
            t.skipped += 1;
        } else {
            t.passed += 1;
        }
    }
    t
}

fn junit_counts(view: &Path, patterns: &[String]) -> Result<Option<TestCounts>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = globset::GlobSetBuilder::new();
    for p in patterns {
        b.add(
            globset::Glob::new(p.trim_start_matches("./"))
                .map_err(|e| Error::BadQuery(format!("--junit {p:?}: {e}")))?,
        );
    }
    let set = b.build().map_err(|e| Error::BadQuery(e.to_string()))?;
    let mut total = TestCounts::default();
    let mut found = false;
    // Build output is usually git-ignored, so walk without ignore rules.
    for entry in ignore::WalkBuilder::new(view)
        .standard_filters(false)
        .filter_entry(|e| e.file_name() != SPACE_DIR && e.file_name() != ".git")
        .build()
        .flatten()
    {
        let Ok(rel) = entry.path().strip_prefix(view) else {
            continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) || !set.is_match(rel) {
            continue;
        }
        if let Ok(xml) = fs::read_to_string(entry.path()) {
            let c = counts_from_junit(&xml);
            total.passed += c.passed;
            total.failed += c.failed;
            total.skipped += c.skipped;
            found = true;
        }
    }
    Ok(found.then_some(total))
}

/// Fields of a check result, for [`Space::insert_check`].
pub(crate) struct NewCheck<'a> {
    pub attempt_id: i64,
    pub snapshot_id: i64,
    pub tree: &'a str,
    pub name: &'a str,
    pub command: &'a str,
    pub exit_code: i32,
    pub duration_ms: i64,
    pub tests: Option<TestCounts>,
    pub log_excerpt: Option<String>,
    pub reused_from: Option<i64>,
    pub actor: &'a str,
}

impl Space {
    pub(crate) fn insert_check(&self, c: NewCheck<'_>) -> Result<Check> {
        self.db.execute(
            "INSERT INTO checks (attempt_id, snapshot_id, tree, name, command, exit_code,
                                 duration_ms, tests_passed, tests_failed, tests_skipped,
                                 log_excerpt, reused_from, actor, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                c.attempt_id,
                c.snapshot_id,
                c.tree,
                c.name,
                c.command,
                c.exit_code,
                c.duration_ms,
                c.tests.map(|t| t.passed),
                c.tests.map(|t| t.failed),
                c.tests.map(|t| t.skipped),
                c.log_excerpt,
                c.reused_from,
                c.actor,
                now()
            ],
        )?;
        self.check_by_id(self.db.last_insert_rowid())
    }

    pub fn check_by_id(&self, id: i64) -> Result<Check> {
        self.db
            .query_row(
                &format!("SELECT {COLS} FROM checks WHERE id = ?1"),
                params![id],
                row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Error::NotFound {
                    kind: "check",
                    id: format!("c{id}"),
                },
                e => e.into(),
            })
    }

    /// Every check of an attempt, oldest first.
    pub fn checks(&self, attempt_id: i64) -> Result<Vec<Check>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {COLS} FROM checks WHERE attempt_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![attempt_id], row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The latest check of each name on an attempt, by name, read against
    /// the attempt's head (stale when recorded on another tree).
    pub fn check_status(&self, attempt_id: i64) -> Result<BTreeMap<String, CheckStatus>> {
        let head = self.head_snapshot(attempt_id).ok().map(|s| s.tree);
        let mut out = BTreeMap::new();
        for c in self.checks(attempt_id)? {
            let stale = head.as_deref() != Some(c.tree.as_str());
            out.insert(c.name.clone(), CheckStatus { check: c, stale });
        }
        Ok(out)
    }

    /// [`Space::check_status`] for every attempt with checks.
    pub fn all_check_status(&self) -> Result<BTreeMap<i64, BTreeMap<String, CheckStatus>>> {
        let mut heads: BTreeMap<i64, Option<String>> = BTreeMap::new();
        let mut stmt = self
            .db
            .prepare(&format!("SELECT {COLS} FROM checks ORDER BY id"))?;
        let rows: Vec<Check> = stmt
            .query_map([], row)?
            .collect::<std::result::Result<_, _>>()?;
        let mut out: BTreeMap<i64, BTreeMap<String, CheckStatus>> = BTreeMap::new();
        for c in rows {
            let head = heads
                .entry(c.attempt_id)
                .or_insert_with(|| self.head_snapshot(c.attempt_id).ok().map(|s| s.tree));
            let stale = head.as_deref() != Some(c.tree.as_str());
            out.entry(c.attempt_id)
                .or_default()
                .insert(c.name.clone(), CheckStatus { check: c, stale });
        }
        Ok(out)
    }

    /// An earlier result for this tree, command and name, from any attempt.
    /// None when the tree's object is missing or corrupt: a result recorded
    /// on a tree that was never written (#217) is not reused.
    fn cached_check(&self, tree: &str, command: &str, name: &str) -> Result<Option<Check>> {
        let stored = Oid::from_hex(tree).and_then(|oid| self.objects().verify_tree(&oid));
        if stored.is_err() {
            return Ok(None);
        }
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT {COLS} FROM checks WHERE tree = ?1 AND command = ?2 AND name = ?3
                     ORDER BY id DESC LIMIT 1"
                ),
                params![tree, command, name],
                row,
            )
            .optional()?)
    }

    /// Snapshot an attempt's view, run `opts.command` in it and record the
    /// result on that snapshot. Works on open and promoted attempts (a
    /// promoted one is still being finished, e.g. re-tested after a rebase).
    pub fn run_check(&self, attempt_id: i64, opts: &CheckOptions) -> Result<CheckResult> {
        let name = opts.name.trim();
        if name.is_empty() || opts.command.trim().is_empty() {
            return Err(Error::BadQuery("a check needs a name and a command".into()));
        }
        let attempt = self.attempt(attempt_id)?;
        let mut snap = self
            .snapshot_with(attempt_id, "check", None, false)?
            .snapshot;
        // A fresh fork's head is its parent's snapshot; record its own.
        if snap.attempt_id != attempt_id {
            snap = self
                .snapshot_with(attempt_id, "check", None, true)?
                .snapshot;
        }

        if !opts.force {
            if let Some(prev) = self.cached_check(&snap.tree, &opts.command, name)? {
                let check = if prev.attempt_id == attempt_id && prev.snapshot_id == snap.id {
                    prev
                } else {
                    self.insert_check(NewCheck {
                        attempt_id,
                        snapshot_id: snap.id,
                        tree: &snap.tree,
                        name,
                        command: &opts.command,
                        exit_code: prev.exit_code,
                        duration_ms: prev.duration_ms,
                        tests: prev.tests,
                        log_excerpt: prev.log_excerpt.clone(),
                        reused_from: Some(prev.reused_from.unwrap_or(prev.id)),
                        actor: &opts.actor,
                    })?
                };
                let status = self.mark_by_check(attempt_id, &check, opts)?;
                return Ok(CheckResult {
                    check,
                    cached: true,
                    log_path: None,
                    status,
                });
            }
        }

        let view = &attempt.view_path;
        let start = Instant::now();
        let output = Command::new("sh")
            .arg("-c")
            .arg(&opts.command)
            .current_dir(view)
            .env(ENV_ATTEMPT, format!("a{attempt_id}"))
            .env(ENV_VIEW, view)
            .env(ENV_SPACE, self.workdir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| Error::Spawn {
                program: "sh".into(),
                source: e,
            })?;
        let duration_ms = start.elapsed().as_millis() as i64;
        let exit_code = output.status.code().unwrap_or(-1);
        let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            if !log.is_empty() && !log.ends_with('\n') {
                log.push('\n');
            }
            log += &String::from_utf8_lossy(&output.stderr);
        }
        let tests = match junit_counts(view, &opts.junit)? {
            Some(t) => Some(t),
            None => counts_from_output(&log),
        };
        let check = self.insert_check(NewCheck {
            attempt_id,
            snapshot_id: snap.id,
            tree: &snap.tree,
            name,
            command: &opts.command,
            exit_code,
            duration_ms,
            tests,
            log_excerpt: excerpt(&log),
            reused_from: None,
            actor: &opts.actor,
        })?;
        let dir = self.workdir().join(SPACE_DIR).join("checks");
        let log_path = fs::create_dir_all(&dir)
            .ok()
            .map(|_| dir.join(format!("c{}.log", check.id)))
            .filter(|p| fs::write(p, &log).is_ok())
            .map(|p| p.to_string_lossy().into_owned());
        self.record(
            &opts.actor,
            "check",
            serde_json::json!({
                "attempt": attempt_id, "check": check.id, "name": name,
                "snapshot": snap.id, "exit_code": exit_code,
            }),
        )?;
        let status = self.mark_by_check(attempt_id, &check, opts)?;
        Ok(CheckResult {
            check,
            cached: false,
            log_path,
            status,
        })
    }

    fn mark_by_check(&self, attempt_id: i64, check: &Check, opts: &CheckOptions) -> Result<Status> {
        let a = self.attempt(attempt_id)?;
        if !opts.mark || !a.status.is_open() {
            return Ok(a.status);
        }
        let (to, reason) = if check.passed {
            (Status::Passed, None)
        } else {
            (
                Status::Failed,
                Some(format!("check {} exited {}", check.name, check.exit_code)),
            )
        };
        if a.status == to && a.reason == reason {
            return Ok(to);
        }
        Ok(self
            .set_status_as(&opts.actor, attempt_id, to, reason.as_deref())?
            .status)
    }

    /// Record the `--check` of `agentvc run` as a check on `snapshot_id`.
    pub(crate) fn record_run_check(
        &self,
        snap: &crate::space::Snapshot,
        command: &str,
        exit_code: i32,
        duration_ms: i64,
        log_tail: &str,
        actor: &str,
    ) -> Result<Check> {
        self.insert_check(NewCheck {
            attempt_id: snap.attempt_id,
            snapshot_id: snap.id,
            tree: &snap.tree,
            name: DEFAULT_NAME,
            command,
            exit_code,
            duration_ms,
            tests: counts_from_output(log_tail),
            log_excerpt: excerpt(log_tail),
            reused_from: None,
            actor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;

    fn space() -> (crate::testutil::TestDir, Space, i64) {
        let dir = crate::testutil::tempdir();
        fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let space = Space::init(dir.path()).unwrap();
        let a = space
            .fork(&ForkRequest {
                actor: "agent:test".into(),
                ..Default::default()
            })
            .unwrap()
            .attempt
            .id;
        (dir, space, a)
    }

    fn opts(cmd: &str) -> CheckOptions {
        CheckOptions {
            actor: "agent:test".into(),
            name: DEFAULT_NAME.into(),
            command: cmd.into(),
            junit: vec![],
            force: false,
            mark: true,
        }
    }

    #[test]
    fn records_result_on_snapshot_and_marks() {
        let (_d, space, a) = space();
        let view = space.attempt(a).unwrap().view_path;
        fs::write(view.join("a.txt"), "two\n").unwrap();
        let r = space
            .run_check(
                a,
                &opts("echo 'test result: ok. 3 passed; 0 failed; 1 ignored'; grep -q two a.txt"),
            )
            .unwrap();
        assert!(r.check.passed && !r.cached);
        assert_eq!(r.status, Status::Passed);
        assert_eq!(
            r.check.tests,
            Some(TestCounts {
                passed: 3,
                failed: 0,
                skipped: 1
            })
        );
        let head = space.head_snapshot(a).unwrap();
        assert_eq!(r.check.snapshot_id, head.id);
        assert_eq!(head.trigger, "check");
        assert!(r.check.log_excerpt.as_deref().unwrap().contains("3 passed"));
        assert!(fs::read_to_string(r.log_path.unwrap())
            .unwrap()
            .contains("3 passed"));

        // Same tree, same command: cached. Forced: runs again.
        let again = space
            .run_check(
                a,
                &opts("echo 'test result: ok. 3 passed; 0 failed; 1 ignored'; grep -q two a.txt"),
            )
            .unwrap();
        assert!(again.cached);
        assert_eq!(again.check.id, r.check.id);

        // A change makes it stale; a failing check marks failed.
        fs::write(view.join("a.txt"), "three\n").unwrap();
        space.snapshot(a, "manual", None).unwrap();
        assert!(space.check_status(a).unwrap()["tests"].stale);
        let f = space.run_check(a, &opts("grep -q two a.txt")).unwrap();
        assert!(!f.check.passed);
        assert_eq!(f.status, Status::Failed);
        let st = &space.check_status(a).unwrap()["tests"];
        assert!(!st.stale && !st.check.passed);
    }

    #[test]
    fn same_tree_in_another_attempt_reuses_the_result() {
        let (_d, space, a) = space();
        let r = space.run_check(a, &opts("true")).unwrap();
        let b = space
            .fork(&ForkRequest {
                actor: "agent:test".into(),
                from: Some(crate::space::Ref::Attempt(a)),
                ..Default::default()
            })
            .unwrap()
            .attempt
            .id;
        let rb = space.run_check(b, &opts("true")).unwrap();
        assert!(rb.cached);
        assert_eq!(rb.check.attempt_id, b);
        assert_eq!(rb.check.reused_from, Some(r.check.id));
        let forced = space
            .run_check(
                b,
                &CheckOptions {
                    force: true,
                    ..opts("true")
                },
            )
            .unwrap();
        assert!(!forced.cached && forced.check.reused_from.is_none());
    }

    #[test]
    fn a_tree_without_its_object_is_never_recorded_or_cached() {
        let (_d, space, a) = space();
        let r = space.run_check(a, &opts("true")).unwrap();
        let tree = r.check.tree.clone();
        let snaps = |s: &Space| -> i64 {
            s.db.query_row("SELECT count(*) FROM snapshots", [], |r| r.get(0))
                .unwrap()
        };
        let before = snaps(&space);

        // The tree's object is unreadable, as after a write on a full disk:
        // the snapshot fails loudly and nothing is recorded (#217).
        let path = space.objects().loose_path(&Oid::from_hex(&tree).unwrap());
        fs::write(&path, b"torn").unwrap();
        assert!(space.snapshot(a, "manual", None).is_err());
        assert!(space.run_check(a, &opts("true")).is_err());
        assert_eq!(snaps(&space), before);
        // And the earlier result on that tree is not offered as cached.
        assert!(space
            .cached_check(&tree, "true", DEFAULT_NAME)
            .unwrap()
            .is_none());

        // A result recorded on a tree id that has no object at all.
        let missing = "ab".repeat(20);
        space
            .insert_check(NewCheck {
                attempt_id: a,
                snapshot_id: r.check.snapshot_id,
                tree: &missing,
                name: DEFAULT_NAME,
                command: "true",
                exit_code: 0,
                duration_ms: 1,
                tests: None,
                log_excerpt: None,
                reused_from: None,
                actor: "agent:test",
            })
            .unwrap();
        assert!(space
            .cached_check(&missing, "true", DEFAULT_NAME)
            .unwrap()
            .is_none());

        // Once the tree is written again, checks run and cache as before.
        fs::remove_file(&path).unwrap();
        let again = space.run_check(a, &opts("true")).unwrap();
        assert!(again.cached && again.check.passed);
        assert!(space
            .cached_check(&tree, "true", DEFAULT_NAME)
            .unwrap()
            .is_some());
    }

    #[test]
    fn promoted_attempt_can_be_checked_and_keeps_its_status() {
        let (_d, space, a) = space();
        space.set_status(a, Status::Promoted, None).unwrap();
        let r = space.run_check(a, &opts("exit 1")).unwrap();
        assert_eq!(r.status, Status::Promoted);
        assert!(!r.check.passed);
    }

    #[test]
    fn abandoned_attempt_is_refused() {
        let (_d, space, a) = space();
        space.set_status(a, Status::Abandoned, Some("x")).unwrap();
        assert_eq!(
            space.run_check(a, &opts("true")).unwrap_err().code(),
            "attempt_closed"
        );
    }

    #[test]
    fn junit_and_pytest_counts() {
        let xml = r#"<?xml version="1.0"?><testsuites><testsuite name="s" tests="4">
            <testcase name="a"/><testcase name="b"><failure message="x"/></testcase>
            <testcase name="c"><skipped/></testcase><testcase name="d"></testcase>
            </testsuite></testsuites>"#;
        assert_eq!(
            counts_from_junit(xml),
            TestCounts {
                passed: 2,
                failed: 1,
                skipped: 1
            }
        );
        let py = "....\n===== 5 passed, 1 failed, 2 skipped in 0.12s =====\n";
        assert_eq!(
            counts_from_output(py),
            Some(TestCounts {
                passed: 5,
                failed: 1,
                skipped: 2
            })
        );
        assert_eq!(counts_from_output("hello"), None);
    }

    #[test]
    fn junit_files_from_the_view() {
        let (_d, space, a) = space();
        let r = space
            .run_check(
                a,
                &CheckOptions {
                    junit: vec!["out/*.xml".into()],
                    ..opts("mkdir -p out && echo '<testcase name=\"x\"/><testcase name=\"y\"><error/></testcase>' > out/r.xml && false")
                },
            )
            .unwrap();
        assert_eq!(
            r.check.tests,
            Some(TestCounts {
                passed: 1,
                failed: 1,
                skipped: 0
            })
        );
        assert_eq!(r.check.exit_code, 1);
    }

    #[test]
    fn excerpt_keeps_the_end() {
        let long: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let e = excerpt(&long).unwrap();
        assert!(e.starts_with("line 60") && e.ends_with("line 99"));
        assert_eq!(excerpt("  \n"), None);
    }
}
