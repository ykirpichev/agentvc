//! Agent metadata on attempts (#10): which model ran, what it cost, how many
//! tokens it used, notes, and where its transcript is.
//!
//! Each `agentvc annotate` call appends one row; nothing is overwritten, so
//! annotations travel with sync like any other history. [`Space::agent_summary`]
//! folds them into one per-attempt view: cost and tokens are summed, except
//! that rows sharing a `session` carry that session's running totals (the
//! Claude Code hook writes one per turn), so only its latest value of each
//! field counts.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use rusqlite::params;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::space::{now, Space};

#[derive(Clone, Debug, Default)]
pub struct AnnotateRequest {
    pub actor: String,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub note: Option<String>,
    /// A path or URL.
    pub transcript: Option<String>,
    /// Agent session these are running totals for (see the module docs).
    pub session: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Annotation {
    pub id: i64,
    pub attempt_id: i64,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub created_at: i64,
}

/// Everything recorded about the agent behind one attempt.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct AgentSummary {
    /// Models named by annotations (the attempt's own `model` first).
    pub models: Vec<String>,
    pub cost_usd: Option<f64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub notes: Vec<NoteRef>,
    pub transcripts: Vec<String>,
    /// How many annotations were folded in.
    pub annotations: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct NoteRef {
    pub text: String,
    pub actor: String,
    pub created_at: i64,
}

impl AgentSummary {
    pub fn is_empty(&self) -> bool {
        self.annotations == 0
    }

    /// One line: `$0.42 · 120k in / 8.1k out · 2 notes`.
    pub fn line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(c) = self.cost_usd {
            parts.push(format_cost(c));
        }
        match (self.tokens_in, self.tokens_out) {
            (Some(i), Some(o)) => {
                parts.push(format!("{} in / {} out", format_count(i), format_count(o)))
            }
            (Some(i), None) => parts.push(format!("{} tokens in", format_count(i))),
            (None, Some(o)) => parts.push(format!("{} tokens out", format_count(o))),
            (None, None) => {}
        }
        match self.notes.len() {
            0 => {}
            1 => parts.push("1 note".into()),
            n => parts.push(format!("{n} notes")),
        }
        parts.join(" · ")
    }
}

pub fn format_cost(c: f64) -> String {
    if c > 0.0 && c < 0.01 {
        format!("${c:.4}")
    } else {
        format!("${c:.2}")
    }
}

pub fn format_count(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{}k", n / 1000),
        n if n >= 1000 => format!("{:.1}k", n as f64 / 1000.0),
        n => n.to_string(),
    }
}

const COLS: &str = "id, attempt_id, actor, model, cost_usd, tokens_in, tokens_out, note, \
                    transcript, session, created_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Annotation> {
    Ok(Annotation {
        id: r.get(0)?,
        attempt_id: r.get(1)?,
        actor: r.get(2)?,
        model: r.get(3)?,
        cost_usd: r.get(4)?,
        tokens_in: r.get(5)?,
        tokens_out: r.get(6)?,
        note: r.get(7)?,
        transcript: r.get(8)?,
        session: r.get(9)?,
        created_at: r.get(10)?,
    })
}

/// Fold annotations (oldest first) into a summary.
pub fn summarize(model: Option<&str>, rows: &[Annotation]) -> AgentSummary {
    let mut s = AgentSummary {
        annotations: rows.len(),
        ..Default::default()
    };
    if let Some(m) = model {
        s.models.push(m.to_string());
    }
    // Per session: the latest value of each field. Rows without a session add up.
    #[derive(Default)]
    struct Totals {
        cost: Option<f64>,
        tin: Option<i64>,
        tout: Option<i64>,
    }
    let mut sessions: BTreeMap<&str, Totals> = BTreeMap::new();
    let mut loose = Totals::default();
    for a in rows {
        match a.session.as_deref() {
            Some(id) => {
                let t = sessions.entry(id).or_default();
                t.cost = a.cost_usd.or(t.cost);
                t.tin = a.tokens_in.or(t.tin);
                t.tout = a.tokens_out.or(t.tout);
            }
            None => {
                add_f(&mut loose.cost, a.cost_usd);
                add_i(&mut loose.tin, a.tokens_in);
                add_i(&mut loose.tout, a.tokens_out);
            }
        }
        if let Some(m) = &a.model {
            if !s.models.contains(m) {
                s.models.push(m.clone());
            }
        }
        if let Some(n) = &a.note {
            s.notes.push(NoteRef {
                text: n.clone(),
                actor: a.actor.clone(),
                created_at: a.created_at,
            });
        }
        if let Some(t) = &a.transcript {
            if !s.transcripts.contains(t) {
                s.transcripts.push(t.clone());
            }
        }
    }
    for t in sessions.values().chain([&loose]) {
        add_f(&mut s.cost_usd, t.cost);
        add_i(&mut s.tokens_in, t.tin);
        add_i(&mut s.tokens_out, t.tout);
    }
    s
}

fn add_f(acc: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v {
        *acc = Some(acc.unwrap_or(0.0) + v);
    }
}

fn add_i(acc: &mut Option<i64>, v: Option<i64>) {
    if let Some(v) = v {
        *acc = Some(acc.unwrap_or(0) + v);
    }
}

impl Space {
    /// Record agent metadata on an attempt. Works on any attempt, closed
    /// ones included: it is history about the attempt, not a change to it.
    /// When the attempt has no model yet, `model` becomes its model.
    pub fn annotate(&self, attempt_id: i64, req: &AnnotateRequest) -> Result<Annotation> {
        let attempt = self.attempt(attempt_id)?;
        let model = req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let note = req.note.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let transcript = req
            .transcript
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if model.is_none()
            && req.cost_usd.is_none()
            && req.tokens_in.is_none()
            && req.tokens_out.is_none()
            && note.is_none()
            && transcript.is_none()
        {
            return Err(Error::BadAnnotation(
                "nothing to record: pass --model, --cost-usd, --tokens-in, --tokens-out, \
                 --note or --transcript"
                    .into(),
            ));
        }
        if let Some(c) = req.cost_usd {
            if !c.is_finite() || c < 0.0 {
                return Err(Error::BadAnnotation(format!(
                    "cost {c} must be a number of dollars, 0 or more"
                )));
            }
        }
        for (flag, v) in [("tokens-in", req.tokens_in), ("tokens-out", req.tokens_out)] {
            if v.is_some_and(|n| n < 0) {
                return Err(Error::BadAnnotation(format!("--{flag} must be 0 or more")));
            }
        }
        self.db.execute(
            "INSERT INTO annotations (attempt_id, actor, model, cost_usd, tokens_in, tokens_out,
                                      note, transcript, session, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                attempt_id,
                req.actor,
                model,
                req.cost_usd,
                req.tokens_in,
                req.tokens_out,
                note,
                transcript,
                req.session,
                now()
            ],
        )?;
        let id = self.db.last_insert_rowid();
        if attempt.model.is_none() {
            if let Some(m) = model {
                self.db.execute(
                    "UPDATE attempts SET model = ?1 WHERE id = ?2",
                    params![m, attempt_id],
                )?;
            }
        }
        let a = self.annotation(id)?;
        // Hook rows are per turn; keep them out of the operation log.
        if req.session.is_none() {
            self.record(
                &req.actor,
                "annotate",
                serde_json::json!({ "attempt": attempt_id, "annotation": id }),
            )?;
        }
        Ok(a)
    }

    pub fn annotation(&self, id: i64) -> Result<Annotation> {
        self.db
            .query_row(
                &format!("SELECT {COLS} FROM annotations WHERE id = ?1"),
                params![id],
                row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Error::NotFound {
                    kind: "annotation",
                    id: id.to_string(),
                },
                e => e.into(),
            })
    }

    /// An attempt's annotations, oldest first.
    pub fn annotations(&self, attempt_id: i64) -> Result<Vec<Annotation>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT {COLS} FROM annotations WHERE attempt_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![attempt_id], row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn agent_summary(&self, attempt_id: i64) -> Result<AgentSummary> {
        let a = self.attempt(attempt_id)?;
        Ok(summarize(
            a.model.as_deref(),
            &self.annotations(attempt_id)?,
        ))
    }

    /// Summaries of every attempt that has annotations, in one query.
    pub fn agent_summaries(&self) -> Result<BTreeMap<i64, AgentSummary>> {
        let mut stmt = self
            .db
            .prepare(&format!("SELECT {COLS} FROM annotations ORDER BY id"))?;
        let rows: Vec<Annotation> = stmt
            .query_map([], row)?
            .collect::<std::result::Result<_, _>>()?;
        let mut by: BTreeMap<i64, Vec<Annotation>> = BTreeMap::new();
        for r in rows {
            by.entry(r.attempt_id).or_default().push(r);
        }
        let mut out = BTreeMap::new();
        for (id, rows) in by {
            let model = self.attempt(id).ok().and_then(|a| a.model);
            out.insert(id, summarize(model.as_deref(), &rows));
        }
        Ok(out)
    }
}

/// Token totals from a Claude Code transcript (JSONL): each assistant message
/// counted once (streamed messages repeat their id), input counting cache
/// reads and writes. `None` when the file has no usage at all.
pub fn transcript_tokens(path: &Path) -> Option<(i64, i64)> {
    let f = std::fs::File::open(path).ok()?;
    let mut by_id: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut anon = (0i64, 0i64);
    let mut any = false;
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let msg = &v["message"];
        let u = &msg["usage"];
        if !u.is_object() {
            continue;
        }
        let n = |k: &str| u[k].as_i64().unwrap_or(0);
        let tin =
            n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
        let tout = n("output_tokens");
        any = true;
        match msg["id"].as_str() {
            Some(id) => {
                by_id.insert(id.to_string(), (tin, tout));
            }
            None => {
                anon.0 += tin;
                anon.1 += tout;
            }
        }
    }
    if !any {
        return None;
    }
    let (i, o) = by_id.values().fold(anon, |(a, b), (x, y)| (a + x, b + y));
    Some((i, o))
}

/// What `claude -p --output-format json` reports at the end of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeResult {
    pub session: Option<String>,
    pub cost_usd: f64,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
}

/// Cost and usage from the result line `claude -p --output-format json`
/// (or `stream-json`) prints last: (session id, cost, tokens in, tokens out).
pub fn claude_result(log_tail: &str) -> Option<ClaudeResult> {
    for line in log_tail.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') || !line.contains("total_cost_usd") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let cost = v["total_cost_usd"].as_f64()?;
        let u = &v["usage"];
        let n = |k: &str| u[k].as_i64();
        let tin = n("input_tokens").map(|i| {
            i + n("cache_creation_input_tokens").unwrap_or(0)
                + n("cache_read_input_tokens").unwrap_or(0)
        });
        return Some(ClaudeResult {
            session: v["session_id"].as_str().map(str::to_string),
            cost_usd: cost,
            tokens_in: tin,
            tokens_out: n("output_tokens"),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;

    fn req() -> AnnotateRequest {
        AnnotateRequest {
            actor: "agent:test".into(),
            ..Default::default()
        }
    }

    fn space() -> (crate::testutil::TestDir, Space, i64) {
        let dir = crate::testutil::tempdir();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let space = Space::init(dir.path()).unwrap();
        let a = space
            .fork(&ForkRequest {
                actor: "agent".into(),
                ..Default::default()
            })
            .unwrap()
            .attempt
            .id;
        (dir, space, a)
    }

    #[test]
    fn sums_cost_and_tokens_and_keeps_notes() {
        let (_d, space, a) = space();
        space
            .annotate(
                a,
                &AnnotateRequest {
                    model: Some("opus".into()),
                    cost_usd: Some(0.25),
                    tokens_in: Some(1000),
                    note: Some("first try".into()),
                    ..req()
                },
            )
            .unwrap();
        space
            .annotate(
                a,
                &AnnotateRequest {
                    cost_usd: Some(0.17),
                    tokens_out: Some(50),
                    transcript: Some("https://example.com/t".into()),
                    ..req()
                },
            )
            .unwrap();
        let s = space.agent_summary(a).unwrap();
        assert!((s.cost_usd.unwrap() - 0.42).abs() < 1e-9);
        assert_eq!(s.tokens_in, Some(1000));
        assert_eq!(s.tokens_out, Some(50));
        assert_eq!(s.models, vec!["opus"]);
        assert_eq!(s.notes[0].text, "first try");
        assert_eq!(s.transcripts, vec!["https://example.com/t"]);
        assert_eq!(s.line(), "$0.42 · 1.0k in / 50 out · 1 note");
        // The attempt had no model, so it takes the annotation's.
        assert_eq!(space.attempt(a).unwrap().model.as_deref(), Some("opus"));
        let ops = space.operations(None).unwrap();
        assert_eq!(ops.iter().filter(|o| o.kind == "annotate").count(), 2);
    }

    #[test]
    fn session_rows_are_running_totals() {
        let (_d, space, a) = space();
        for (cost, tin) in [(Some(0.1), 100), (None, 300)] {
            space
                .annotate(
                    a,
                    &AnnotateRequest {
                        session: Some("s1".into()),
                        cost_usd: cost,
                        tokens_in: Some(tin),
                        ..req()
                    },
                )
                .unwrap();
        }
        space
            .annotate(
                a,
                &AnnotateRequest {
                    session: Some("s2".into()),
                    tokens_in: Some(5),
                    ..req()
                },
            )
            .unwrap();
        let s = space.agent_summary(a).unwrap();
        assert_eq!(s.tokens_in, Some(305));
        assert_eq!(s.cost_usd, Some(0.1));
        assert_eq!(space.agent_summaries().unwrap()[&a], s);
    }

    #[test]
    fn refuses_empty_and_negative() {
        let (_d, space, a) = space();
        let e = space.annotate(a, &req()).unwrap_err();
        assert_eq!(e.code(), "bad_annotation");
        let e = space
            .annotate(
                a,
                &AnnotateRequest {
                    cost_usd: Some(-1.0),
                    ..req()
                },
            )
            .unwrap_err();
        assert_eq!(e.code(), "bad_annotation");
    }

    #[test]
    fn reads_transcript_and_result_usage() {
        let dir = crate::testutil::tempdir();
        let p = dir.path().join("t.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"user","message":{"content":"hi"}}"#, "\n",
                r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":1}}}"#, "\n",
                r#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":7}}}"#, "\n",
                r#"{"type":"assistant","message":{"id":"m2","usage":{"input_tokens":5,"output_tokens":3}}}"#, "\n",
            ),
        )
        .unwrap();
        assert_eq!(transcript_tokens(&p), Some((105, 10)));
        let tail = "noise\n{\"type\":\"result\",\"session_id\":\"abc\",\"total_cost_usd\":0.5,\"usage\":{\"input_tokens\":2,\"cache_read_input_tokens\":3,\"output_tokens\":4}}\n";
        assert_eq!(
            claude_result(tail),
            Some(ClaudeResult {
                session: Some("abc".into()),
                cost_usd: 0.5,
                tokens_in: Some(5),
                tokens_out: Some(4)
            })
        );
        assert_eq!(claude_result("no json"), None);
    }
}
