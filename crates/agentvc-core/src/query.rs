//! Querying attempt history (#73): `agentvc log` with filters.
//!
//! Filters combine with AND: task, status, actor, model, files touched,
//! created since, snapshot metadata (`tests=pass`) and a text search over
//! the task goal, the attempt's prompt and reason, and its metadata values.
//! For example "attempts on this task that touched x.py and passed tests" is
//! `agentvc log --task t3 --touched x.py --meta tests=pass`.
//!
//! The cheap filters run in SQL; metadata is read in one pass over the
//! snapshots table; `--touched` diffs each remaining attempt's own changes
//! (head against its base), which skips unchanged subtrees, so it costs what
//! the attempt changed rather than the size of the repository.

use std::collections::{BTreeMap, HashMap};

use globset::{Glob, GlobSetBuilder};
use rusqlite::types::Value as SqlValue;
use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::objects::Oid;
use crate::snapshot;
use crate::space::{Space, Status};

#[derive(Clone, Debug, Default)]
pub struct Query {
    pub task: Option<i64>,
    /// Any of these statuses.
    pub status: Vec<Status>,
    /// Actor, exact or as a prefix (`agent:claude` matches
    /// `agent:claude-code`; `agent` matches every agent).
    pub actor: Option<String>,
    /// Model, exact or as a prefix.
    pub model: Option<String>,
    /// Paths, directories or globs; the attempt's own changes must touch
    /// one of them.
    pub touched: Vec<String>,
    /// Created at or after this time (Unix seconds).
    pub since: Option<i64>,
    /// `key=value` pairs the attempt's latest metadata must have (all of them).
    pub meta: Vec<(String, String)>,
    /// Case-insensitive text in the task goal, prompt, reason or metadata.
    pub grep: Option<String>,
}

/// One attempt in a query result: flat, for `--json` consumers.
#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub id: i64,
    pub task_id: Option<i64>,
    pub task: Option<String>,
    pub parent_attempt: Option<i64>,
    pub actor: String,
    pub model: Option<String>,
    pub status: Status,
    pub reason: Option<String>,
    pub created_at: i64,
    pub snapshots: i64,
    pub last_snapshot_at: Option<i64>,
    /// Latest value of every metadata key over the attempt's snapshots.
    pub meta: BTreeMap<String, String>,
    /// Summed agent cost (`agentvc annotate`, #10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// With `touched`: the attempt's changed files that matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub touched: Option<Vec<String>>,
}

fn prefix_match(value: &str, want: &str) -> bool {
    value == want
        || value.strip_prefix(want).is_some_and(|rest| {
            want.ends_with([':', '-', '/']) || rest.starts_with([':', '-', '/'])
        })
}

fn meta_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parse `--since`: Unix seconds, `YYYY-MM-DD`, or an age like `30m`, `2h`,
/// `3d`, `1w` (relative to `now`).
pub fn parse_since(s: &str, now: i64) -> Result<i64> {
    let bad = || {
        Error::BadQuery(format!(
            "--since {s:?}: expected 2h, 3d, 1w, YYYY-MM-DD or Unix seconds"
        ))
    };
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    if let Some((num, unit)) = s
        .char_indices()
        .last()
        .map(|(i, c)| (&s[..i], c))
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    {
        let n: i64 = num.parse().map_err(|_| bad())?;
        let secs = match unit {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 7 * 86_400,
            _ => return Err(bad()),
        };
        return Ok(now - n * secs);
    }
    // YYYY-MM-DD, midnight UTC (days since the epoch, civil calendar).
    let parts: Vec<&str> = s.split('-').collect();
    if let [y, m, d] = parts.as_slice() {
        let (y, m, d): (i64, i64, i64) = (
            y.parse().map_err(|_| bad())?,
            m.parse().map_err(|_| bad())?,
            d.parse().map_err(|_| bad())?,
        );
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return Err(bad());
        }
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        return Ok((era * 146_097 + doe - 719_468) * 86_400);
    }
    Err(bad())
}

impl Space {
    /// Attempts matching `q`, oldest first.
    pub fn query(&self, q: &Query) -> Result<Vec<Row>> {
        // SQL filters first.
        let mut sql = String::from(
            "SELECT a.id, a.task_id, t.goal, a.parent_attempt, a.actor, a.model, a.status,
                    a.reason, a.created_at, a.prompt, a.parent_snapshot,
                    (SELECT COUNT(*) FROM snapshots s WHERE s.attempt_id = a.id),
                    (SELECT MAX(created_at) FROM snapshots s WHERE s.attempt_id = a.id)
             FROM attempts a LEFT JOIN tasks t ON t.id = a.task_id WHERE 1=1",
        );
        let mut args: Vec<SqlValue> = Vec::new();
        if let Some(t) = q.task {
            sql += " AND a.task_id = ?";
            args.push(t.into());
        }
        if !q.status.is_empty() {
            sql += &format!(" AND a.status IN ({})", vec!["?"; q.status.len()].join(","));
            args.extend(
                q.status
                    .iter()
                    .map(|s| SqlValue::from(s.as_str().to_string())),
            );
        }
        if let Some(s) = q.since {
            sql += " AND a.created_at >= ?";
            args.push(s.into());
        }
        sql += " ORDER BY a.id";
        struct Cand {
            row: Row,
            prompt: Option<String>,
            parent_snapshot: Option<i64>,
        }
        let mut stmt = self.db.prepare(&sql)?;
        let mut cands: Vec<Cand> = stmt
            .query_map(rusqlite::params_from_iter(args), |r| {
                let status: String = r.get(6)?;
                Ok(Cand {
                    row: Row {
                        id: r.get(0)?,
                        task_id: r.get(1)?,
                        task: r.get(2)?,
                        parent_attempt: r.get(3)?,
                        actor: r.get(4)?,
                        model: r.get(5)?,
                        status: Status::parse(&status).unwrap_or(Status::Exploring),
                        reason: r.get(7)?,
                        created_at: r.get(8)?,
                        snapshots: r.get(11)?,
                        last_snapshot_at: r.get(12)?,
                        meta: BTreeMap::new(),
                        cost_usd: None,
                        touched: None,
                    },
                    prompt: r.get(9)?,
                    parent_snapshot: r.get(10)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        if let Some(a) = &q.actor {
            cands.retain(|c| prefix_match(&c.row.actor, a));
        }
        if let Some(m) = &q.model {
            cands.retain(|c| c.row.model.as_deref().is_some_and(|v| prefix_match(v, m)));
        }
        if cands.is_empty() {
            return Ok(Vec::new());
        }

        // Metadata: one pass over the snapshots that carry any.
        let wanted: HashMap<i64, usize> = cands
            .iter()
            .enumerate()
            .map(|(i, c)| (c.row.id, i))
            .collect();
        let mut stmt = self
            .db
            .prepare("SELECT attempt_id, meta FROM snapshots WHERE meta IS NOT NULL ORDER BY id")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let Some(&i) = wanted.get(&id) else { continue };
            let m: String = r.get(1)?;
            if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&m) {
                for (k, v) in m {
                    cands[i].row.meta.insert(k, meta_string(&v));
                }
            }
        }
        drop(rows);
        // Recorded checks (#11) count as metadata: `--meta tests=pass`.
        for (id, checks) in self.all_check_status()? {
            if let Some(&i) = wanted.get(&id) {
                for (name, st) in checks {
                    cands[i]
                        .row
                        .meta
                        .insert(name, st.check.outcome().to_string());
                }
            }
        }
        for (id, agent) in self.agent_summaries()? {
            if let Some(&i) = wanted.get(&id) {
                cands[i].row.cost_usd = agent.cost_usd;
            }
        }
        cands.retain(|c| {
            q.meta
                .iter()
                .all(|(k, v)| c.row.meta.get(k).is_some_and(|x| x == v))
        });
        if let Some(g) = &q.grep {
            let g = g.to_lowercase();
            cands.retain(|c| {
                let hay = [
                    c.row.task.as_deref(),
                    c.prompt.as_deref(),
                    c.row.reason.as_deref(),
                ];
                hay.iter().flatten().any(|h| h.to_lowercase().contains(&g))
                    || c.row.meta.values().any(|v| v.to_lowercase().contains(&g))
            });
        }

        // Files touched: the attempt's own changes.
        if !q.touched.is_empty() {
            let mut b = GlobSetBuilder::new();
            let mut plain = Vec::new();
            for p in &q.touched {
                let p = crate::objects::tree_path(p)
                    .trim_end_matches('/')
                    .to_string();
                b.add(Glob::new(&p).map_err(|e| Error::BadQuery(format!("--touched {p:?}: {e}")))?);
                plain.push(p);
            }
            let set = b.build().map_err(|e| Error::BadQuery(e.to_string()))?;
            let hit = |path: &str| {
                set.is_match(path)
                    || plain
                        .iter()
                        .any(|p| path == p || path.starts_with(&format!("{p}/")))
            };
            let mut kept = Vec::new();
            for mut c in cands {
                let Some(base) = self.own_base_tree(c.row.id, c.parent_snapshot)? else {
                    continue;
                };
                let head = match self.snapshots_latest_tree(c.row.id)? {
                    Some(t) => t,
                    None => continue, // never snapshotted: changed nothing yet
                };
                let files: Vec<String> = snapshot::diff(
                    self.objects(),
                    &Oid::from_hex(&base)?,
                    &Oid::from_hex(&head)?,
                )?
                .into_iter()
                .map(|f| f.path)
                .filter(|p| hit(p))
                .collect();
                if !files.is_empty() {
                    c.row.touched = Some(files);
                    kept.push(c);
                }
            }
            cands = kept;
        }
        Ok(cands.into_iter().map(|c| c.row).collect())
    }

    /// The tree an attempt's own changes are measured against: its latest
    /// rebase target, else the snapshot it was forked from.
    fn own_base_tree(&self, id: i64, parent_snapshot: Option<i64>) -> Result<Option<String>> {
        if let Some(b) = self.own_git_base(id)? {
            return Ok(Some(b.tree));
        }
        match parent_snapshot {
            Some(p) => Ok(Some(self.snapshot_by_id(p)?.tree)),
            None => Ok(None),
        }
    }

    fn snapshots_latest_tree(&self, id: i64) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .db
            .query_row(
                "SELECT tree FROM snapshots WHERE attempt_id = ?1 ORDER BY id DESC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Ref, ROOT_ATTEMPT};
    use crate::view::BackendChoice;
    use std::fs;

    #[test]
    fn filters_combine() {
        let tmp = crate::testutil::tempdir();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/x.py"), "x = 1\n").unwrap();
        fs::write(tmp.path().join("README.md"), "r\n").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let t = space.create_task("speed up the parser", None).unwrap();
        let other = space.create_task("docs", None).unwrap();
        let fork = |task: i64, actor: &str, model: Option<&str>| {
            space
                .fork(&ForkRequest {
                    task_id: Some(task),
                    from: Some(Ref::Attempt(ROOT_ATTEMPT)),
                    actor: actor.into(),
                    model: model.map(String::from),
                    backend: BackendChoice::Copy,
                    ..Default::default()
                })
                .unwrap()
                .attempt
        };
        let a = fork(t.id, "agent:claude", Some("opus"));
        fs::write(a.view_path.join("src/x.py"), "x = 2\n").unwrap();
        space
            .snapshot(a.id, "t", Some(serde_json::json!({ "tests": "pass" })))
            .unwrap();
        let b = fork(t.id, "agent:codex", None);
        fs::write(b.view_path.join("src/x.py"), "x = 3\n").unwrap();
        space
            .snapshot(b.id, "t", Some(serde_json::json!({ "tests": "fail" })))
            .unwrap();
        space
            .set_status(b.id, Status::Failed, Some("parser crashed"))
            .unwrap();
        let c = fork(t.id, "agent:claude", Some("sonnet"));
        fs::write(c.view_path.join("README.md"), "new\n").unwrap();
        space
            .snapshot(c.id, "t", Some(serde_json::json!({ "tests": "pass" })))
            .unwrap();
        let d = fork(other.id, "human:ana", None);

        let ids =
            |q: Query| -> Vec<i64> { space.query(&q).unwrap().iter().map(|r| r.id).collect() };
        // The design's example: on this task, touched x.py and passed tests.
        assert_eq!(
            ids(Query {
                task: Some(t.id),
                touched: vec!["src/x.py".into()],
                meta: vec![("tests".into(), "pass".into())],
                ..Default::default()
            }),
            [a.id]
        );
        assert_eq!(
            ids(Query {
                touched: vec!["*.py".into()],
                ..Default::default()
            }),
            [a.id, b.id]
        );
        assert_eq!(
            ids(Query {
                touched: vec!["src".into()],
                status: vec![Status::Failed],
                ..Default::default()
            }),
            [b.id]
        );
        assert_eq!(
            ids(Query {
                actor: Some("agent:claude".into()),
                ..Default::default()
            }),
            [a.id, c.id]
        );
        assert_eq!(
            ids(Query {
                actor: Some("agent".into()),
                model: Some("opus".into()),
                ..Default::default()
            }),
            [a.id]
        );
        assert_eq!(
            ids(Query {
                grep: Some("CRASHED".into()),
                ..Default::default()
            }),
            [b.id]
        );
        assert_eq!(
            ids(Query {
                grep: Some("docs".into()),
                ..Default::default()
            }),
            [d.id]
        );
        let now = crate::space::now();
        assert_eq!(
            ids(Query {
                since: Some(now + 60),
                ..Default::default()
            }),
            Vec::<i64>::new()
        );
        let rows = space
            .query(&Query {
                touched: vec!["src/x.py".into()],
                task: Some(t.id),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            rows[0].touched.as_deref(),
            Some(&["src/x.py".to_string()][..])
        );
        assert_eq!(rows[0].meta["tests"], "pass");
        assert_eq!(rows[0].task.as_deref(), Some("speed up the parser"));
    }

    #[test]
    fn since_parses_ages_and_dates() {
        assert_eq!(parse_since("2h", 10_000).unwrap(), 10_000 - 7200);
        assert_eq!(
            parse_since("3d", 1_000_000).unwrap(),
            1_000_000 - 3 * 86_400
        );
        assert_eq!(parse_since("1970-01-02", 0).unwrap(), 86_400);
        assert_eq!(parse_since("2026-11-26", 0).unwrap(), 1_795_651_200);
        assert_eq!(parse_since("1795651200", 0).unwrap(), 1_795_651_200);
        assert!(parse_since("yesterday", 0).is_err());
    }

    /// 1,000 attempts and 50,000 snapshots: queries stay interactive.
    #[test]
    fn queries_are_fast_on_a_large_space() {
        let tmp = crate::testutil::tempdir();
        fs::write(tmp.path().join("a.txt"), "a\n").unwrap();
        let space = Space::init(tmp.path()).unwrap();
        let task = space.create_task("big", None).unwrap();
        let root = space.head_snapshot(ROOT_ATTEMPT).unwrap();
        let tx = space.db.unchecked_transaction().unwrap();
        for i in 0..1000i64 {
            tx.execute(
                "INSERT INTO attempts (task_id, parent_attempt, parent_snapshot, actor, model,
                     status, view_path, backend, created_at)
                 VALUES (?1, 1, ?2, ?3, 'm', ?4, '/nonexistent', 'copy', ?5)",
                rusqlite::params![
                    task.id,
                    root.id,
                    if i % 2 == 0 {
                        "agent:claude"
                    } else {
                        "agent:codex"
                    },
                    if i % 3 == 0 { "passed" } else { "exploring" },
                    i
                ],
            )
            .unwrap();
            let id = tx.last_insert_rowid();
            for j in 0..50 {
                tx.execute(
                    "INSERT INTO snapshots (attempt_id, tree, trigger, meta, created_at)
                     VALUES (?1, ?2, 'tool:Edit', ?3, ?4)",
                    rusqlite::params![
                        id,
                        root.tree,
                        (j == 49).then(|| format!(
                            "{{\"tests\":\"{}\"}}",
                            if i % 5 == 0 { "pass" } else { "fail" }
                        )),
                        i
                    ],
                )
                .unwrap();
            }
        }
        tx.commit().unwrap();
        let start = std::time::Instant::now();
        let rows = space
            .query(&Query {
                task: Some(task.id),
                status: vec![Status::Passed],
                actor: Some("agent:claude".into()),
                meta: vec![("tests".into(), "pass".into())],
                grep: Some("big".into()),
                touched: vec!["a.txt".into()],
                ..Default::default()
            })
            .unwrap();
        let took = start.elapsed();
        assert!(rows.is_empty()); // nothing changed a.txt
        let all = space
            .query(&Query {
                meta: vec![("tests".into(), "pass".into())],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(all.len(), 200);
        let limit = if cfg!(debug_assertions) { 2000 } else { 200 };
        assert!(
            took.as_millis() < limit,
            "query took {took:?} on 1,000 attempts / 50,000 snapshots"
        );
        eprintln!("query on 1,000 attempts / 50,000 snapshots: {took:?}");
    }
}
