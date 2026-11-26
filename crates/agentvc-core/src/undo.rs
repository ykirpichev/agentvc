//! The operation log and `agentvc undo` (#4).
//!
//! Every change to a space is appended to the `operations` table. The
//! operations that change state an agent or a human may regret (a task
//! created by mistake, a fork, a status change, a promote that closed the
//! siblings) are undoable: undo computes the [`Change`]s that reverse them,
//! checks that the space still looks the way the operation left it, applies
//! them and records an `undo` operation holding exactly what it applied.
//! Undoing that `undo` reverses it again, so redo is just undo of an undo.
//!
//! Undo never deletes data. Undoing a fork closes the attempt as
//! `abandoned (fork undone)` and puts its view aside (an overlay view is
//! unmounted and keeps its upper layer; a copied view moves to
//! `.agentvc/trash/aN`), so undoing the undo brings it back as it was.

use std::fs;
use std::path::PathBuf;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, IoContext, Result};
use crate::space::{Space, Status, SPACE_DIR};

/// An attempt's status and reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub status: String,
    pub reason: Option<String>,
}

/// A task row, kept whole so a removed task comes back with its id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: i64,
    pub goal: String,
    pub criteria: Option<String>,
    pub created_at: i64,
}

/// One reversible change to a space.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Change {
    /// An attempt went from one outcome to another.
    Status {
        attempt: i64,
        from: Outcome,
        to: Outcome,
    },
    /// A task was added (or comes back).
    TaskAdd { task: TaskRow },
    /// A task was removed.
    TaskRemove { task: TaskRow },
    /// An attempt moved from one task to another (or to none).
    AttemptTask {
        attempt: i64,
        from: Option<i64>,
        to: Option<i64>,
    },
    /// An attempt's view was put aside.
    ViewPark { attempt: i64 },
    /// An attempt's view was brought back.
    ViewRestore { attempt: i64 },
}

impl Change {
    pub(crate) fn status(
        attempt: i64,
        from: Status,
        from_reason: Option<String>,
        to: Status,
        to_reason: Option<String>,
    ) -> Change {
        Change::Status {
            attempt,
            from: Outcome {
                status: from.as_str().into(),
                reason: from_reason,
            },
            to: Outcome {
                status: to.as_str().into(),
                reason: to_reason,
            },
        }
    }

    /// The change that reverses this one.
    pub fn inverse(&self) -> Change {
        match self.clone() {
            Change::Status { attempt, from, to } => Change::Status {
                attempt,
                from: to,
                to: from,
            },
            Change::AttemptTask { attempt, from, to } => Change::AttemptTask {
                attempt,
                from: to,
                to: from,
            },
            Change::TaskAdd { task } => Change::TaskRemove { task },
            Change::TaskRemove { task } => Change::TaskAdd { task },
            Change::ViewPark { attempt } => Change::ViewRestore { attempt },
            Change::ViewRestore { attempt } => Change::ViewPark { attempt },
        }
    }

    fn describe(&self) -> String {
        match self {
            Change::Status { attempt, from, to } => {
                format!("a{attempt}: {} -> {}", outcome(from), outcome(to))
            }
            Change::AttemptTask { attempt, to, .. } => match to {
                Some(t) => format!("a{attempt}: back in t{t}"),
                None => format!("a{attempt}: out of its task"),
            },
            Change::TaskAdd { task } => format!("t{} restored", task.id),
            Change::TaskRemove { task } => format!("t{} removed", task.id),
            Change::ViewPark { attempt } => format!("a{attempt}: view put aside"),
            Change::ViewRestore { attempt } => format!("a{attempt}: view restored"),
        }
    }
}

fn outcome(o: &Outcome) -> String {
    match &o.reason {
        Some(r) => format!("{} ({r})", o.status),
        None => o.status.clone(),
    }
}

/// One entry of the operation log.
#[derive(Clone, Debug, Serialize)]
pub struct Operation {
    pub id: i64,
    pub actor: String,
    pub kind: String,
    pub payload: Value,
    pub created_at: i64,
    /// One line for humans.
    pub summary: String,
    /// Whether `agentvc undo <id>` can reverse it (ignoring whether it
    /// already was).
    pub undoable: bool,
    /// The `undo` operation that reversed it, while that undo stands.
    pub undone_by: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UndoResult {
    /// The new `undo` operation; `agentvc undo <op>` reverses it.
    pub op: i64,
    /// The operation that was reversed.
    pub undid: Operation,
    /// What changed, in the order applied.
    pub changes: Vec<Change>,
    /// Things that could not be done but do not make the undo fail (a busy
    /// view that stays mounted).
    pub warnings: Vec<String>,
}

fn is_status_kind(kind: &str) -> bool {
    kind.strip_prefix("attempt.")
        .and_then(Status::parse)
        .is_some()
}

fn undoable(kind: &str, payload: &Value) -> bool {
    // Merged from another machine (#77): its ids are that machine's.
    if payload.get("merged_from").is_some() {
        return false;
    }
    match kind {
        "task.create" | "attempt.fork" => true,
        "undo" => payload.get("changes").is_some_and(Value::is_array),
        k if is_status_kind(k) => payload.get("changes").is_some_and(Value::is_array),
        _ => false,
    }
}

fn summary(kind: &str, p: &Value) -> String {
    let a = |k: &str| p.get(k).and_then(Value::as_i64);
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    match kind {
        "task.create" => format!(
            "create t{} {:?}",
            a("task").unwrap_or(0),
            s("goal").unwrap_or("")
        ),
        "attempt.fork" => format!(
            "fork a{} from {} ({})",
            a("attempt").unwrap_or(0),
            s("from").unwrap_or("?"),
            s("backend").unwrap_or("?")
        ),
        "undo" => format!(
            "undo op {} ({})",
            a("op").unwrap_or(0),
            s("summary").or(s("kind")).unwrap_or("?")
        ),
        k if is_status_kind(k) => {
            let mut out = format!("{} a{}", &k["attempt.".len()..], a("attempt").unwrap_or(0));
            if let Some(r) = s("reason") {
                out += &format!(" ({r})");
            }
            let closed: Vec<String> = p
                .get("closed_siblings")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64)
                .map(|i| format!("a{i}"))
                .collect();
            if !closed.is_empty() {
                out += &format!(", closed {}", closed.join(", "));
            }
            out
        }
        _ => match a("attempt") {
            Some(id) => format!("{kind} a{id}"),
            None => kind.to_string(),
        },
    }
}

/// (id, actor, kind, payload, created_at) of an `operations` row.
type OpRow = (i64, String, String, String, i64);

fn conflict(op: i64, reason: impl Into<String>) -> Error {
    Error::UndoConflict {
        op,
        reason: reason.into(),
    }
}

impl Space {
    pub(crate) fn trash_dir(&self, attempt: i64) -> PathBuf {
        self.workdir()
            .join(SPACE_DIR)
            .join("trash")
            .join(format!("a{attempt}"))
    }

    fn raw_operations(&self, sql_tail: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<OpRow>> {
        let mut stmt = self.db.prepare(&format!(
            "SELECT id, actor, kind, payload, created_at FROM operations {sql_tail}"
        ))?;
        let rows = stmt.query_map(args, |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// For each operation reversed by an `undo` that still stands, that undo.
    /// An undo stands unless a later undo reversed it, so walking the undos
    /// newest first settles each one before the ones it targets.
    fn undone_map(&self) -> Result<std::collections::HashMap<i64, i64>> {
        let mut undone = std::collections::HashMap::new();
        for (id, _, _, payload, _) in
            self.raw_operations("WHERE kind = 'undo' ORDER BY id DESC", &[])?
        {
            if undone.contains_key(&id) {
                continue;
            }
            let target = serde_json::from_str::<Value>(&payload)
                .ok()
                .and_then(|p| p.get("op").and_then(Value::as_i64));
            if let Some(t) = target {
                undone.entry(t).or_insert(id);
            }
        }
        Ok(undone)
    }

    fn to_operation(
        undone: &std::collections::HashMap<i64, i64>,
        (id, actor, kind, payload, created_at): OpRow,
    ) -> Operation {
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        Operation {
            summary: summary(&kind, &payload),
            undoable: undoable(&kind, &payload),
            undone_by: undone.get(&id).copied(),
            id,
            actor,
            kind,
            payload,
            created_at,
        }
    }

    /// The operation log, newest first; `limit` keeps only the newest ones.
    pub fn operations(&self, limit: Option<usize>) -> Result<Vec<Operation>> {
        let undone = self.undone_map()?;
        let limit = limit.map_or(-1, |l| l as i64);
        Ok(self
            .raw_operations("ORDER BY id DESC LIMIT ?1", &[&limit])?
            .into_iter()
            .map(|r| Self::to_operation(&undone, r))
            .collect())
    }

    /// One operation by id.
    pub fn operation(&self, id: i64) -> Result<Operation> {
        let undone = self.undone_map()?;
        self.raw_operations("WHERE id = ?1", &[&id])?
            .into_iter()
            .next()
            .map(|r| Self::to_operation(&undone, r))
            .ok_or(Error::NotFound {
                kind: "operation",
                id: id.to_string(),
            })
    }

    /// Reverse operation `op`, or the newest undoable operation that is not
    /// an undo and still stands. Recorded as an `undo` operation by `actor`.
    pub fn undo(&self, actor: &str, op: Option<i64>) -> Result<UndoResult> {
        let target = match op {
            Some(id) => self.operation(id)?,
            None => self
                .operations(None)?
                .into_iter()
                .find(|o| o.undoable && o.undone_by.is_none() && o.kind != "undo")
                .ok_or(Error::NothingToUndo)?,
        };
        if !target.undoable {
            return Err(Error::NotUndoable {
                op: target.id,
                kind: target.kind.clone(),
            });
        }
        if let Some(by) = target.undone_by {
            return Err(Error::AlreadyUndone { op: target.id, by });
        }
        let changes = self.reversal(&target)?;
        for c in &changes {
            self.check(target.id, c)?;
        }
        let mut warnings = Vec::new();
        for c in &changes {
            self.apply(c, &mut warnings)?;
        }
        let id = self.record(
            actor,
            "undo",
            json!({
                "op": target.id, "kind": target.kind, "summary": target.summary,
                "changes": changes,
            }),
        )?;
        let undid = self.operation(target.id)?;
        Ok(UndoResult {
            op: id,
            undid,
            changes,
            warnings,
        })
    }

    /// The changes that reverse `op`, in the order to apply them.
    fn reversal(&self, op: &Operation) -> Result<Vec<Change>> {
        let attempt = op.payload.get("attempt").and_then(Value::as_i64);
        match op.kind.as_str() {
            "task.create" => {
                let id = op.payload.get("task").and_then(Value::as_i64).unwrap_or(0);
                let task = self
                    .task(id)
                    .map_err(|_| conflict(op.id, format!("t{id} no longer exists")))?;
                // Attempts whose fork was undone leave the task with it; any
                // other attempt keeps it.
                let mut changes = Vec::new();
                let mut used = Vec::new();
                for a in self.attempts(Some(id))? {
                    if a.status == Status::Abandoned
                        && a.reason
                            .as_deref()
                            .is_some_and(|r| r.starts_with("fork undone"))
                    {
                        changes.push(Change::AttemptTask {
                            attempt: a.id,
                            from: Some(id),
                            to: None,
                        });
                    } else {
                        used.push(format!("a{}", a.id));
                    }
                }
                if !used.is_empty() {
                    return Err(conflict(
                        op.id,
                        format!(
                            "t{id} has attempts {}; undo their forks first",
                            used.join(", ")
                        ),
                    ));
                }
                changes.push(Change::TaskRemove {
                    task: TaskRow {
                        id: task.id,
                        goal: task.goal,
                        criteria: task.criteria,
                        created_at: task.created_at,
                    },
                });
                Ok(changes)
            }
            "attempt.fork" => {
                let id = attempt.unwrap_or(0);
                let a = self.attempt(id)?;
                if !a.status.is_open() {
                    return Err(conflict(
                        op.id,
                        format!(
                            "a{id} is {} now; undo the operation that closed it first",
                            a.status
                        ),
                    ));
                }
                Ok(vec![
                    Change::status(
                        id,
                        a.status,
                        a.reason,
                        Status::Abandoned,
                        Some(format!("fork undone (op {})", op.id)),
                    ),
                    Change::ViewPark { attempt: id },
                ])
            }
            _ => {
                let recorded: Vec<Change> = serde_json::from_value(op.payload["changes"].clone())
                    .map_err(|e| {
                    conflict(op.id, format!("its recorded changes are unreadable: {e}"))
                })?;
                Ok(recorded.iter().rev().map(Change::inverse).collect())
            }
        }
    }

    /// Refuse a change the space no longer allows, before anything changes.
    fn check(&self, op: i64, c: &Change) -> Result<()> {
        match c {
            Change::Status { attempt, from, .. } => {
                let a = self.attempt(*attempt)?;
                let now = Outcome {
                    status: a.status.as_str().into(),
                    reason: a.reason,
                };
                if &now != from {
                    return Err(conflict(
                        op,
                        format!(
                            "a{attempt} is {} now, not {}; undo the later operations on it first",
                            outcome(&now),
                            outcome(from)
                        ),
                    ));
                }
            }
            Change::AttemptTask { attempt, from, .. } => {
                let a = self.attempt(*attempt)?;
                if &a.task_id != from {
                    return Err(conflict(op, format!("a{attempt} moved to another task")));
                }
            }
            Change::TaskRemove { task } => {
                // The attempts still on it are moved off by changes before
                // this one; `reversal` refuses when it cannot.
                self.task(task.id)
                    .map_err(|_| conflict(op, format!("t{} no longer exists", task.id)))?;
            }
            Change::TaskAdd { task } => {
                if self.task(task.id).is_ok() {
                    return Err(conflict(op, format!("t{} exists", task.id)));
                }
            }
            Change::ViewPark { attempt } => {
                if *attempt == crate::space::ROOT_ATTEMPT {
                    return Err(conflict(op, "a1 is the host checkout"));
                }
            }
            Change::ViewRestore { attempt } => {
                let a = self.attempt(*attempt)?;
                if a.backend == crate::gc::REMOVED {
                    return Err(conflict(
                        op,
                        format!("gc removed the view of a{attempt}; fork from it instead"),
                    ));
                }
                if a.backend != "overlay"
                    && !a.view_path.exists()
                    && !self.trash_dir(*attempt).exists()
                {
                    return Err(conflict(
                        op,
                        format!(
                            "the view of a{attempt} is gone from {}",
                            self.trash_dir(*attempt).display()
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn apply(&self, c: &Change, warnings: &mut Vec<String>) -> Result<()> {
        match c {
            Change::Status { attempt, to, .. } => {
                let status = Status::parse(&to.status).unwrap_or(Status::Exploring);
                let closed_at = (!status.is_open()).then(crate::space::now);
                self.db.execute(
                    "UPDATE attempts SET status = ?1, reason = ?2, closed_at = ?3 WHERE id = ?4",
                    params![to.status, to.reason, closed_at, attempt],
                )?;
                let a = self.attempt(*attempt)?;
                if status.is_open() {
                    if a.backend == crate::gc::REMOVED {
                        warnings.push(format!(
                            "gc removed the view of a{attempt}; `agentvc doctor --repair` writes it out from its latest snapshot"
                        ));
                    } else if self.ensure_view(&a).unwrap_or(false) {
                        // Reopened views get their git link back (#68).
                        self.ensure_view_git(&a);
                    }
                } else {
                    // As when closing it by hand: abandoned views lose their
                    // git link, closed overlay views are unmounted.
                    if status == Status::Abandoned {
                        let _ = self.unlink_view_git(*attempt);
                    }
                    #[cfg(target_os = "linux")]
                    self.release_views(&[*attempt]);
                }
            }
            Change::AttemptTask { attempt, to, .. } => {
                self.db.execute(
                    "UPDATE attempts SET task_id = ?1 WHERE id = ?2",
                    params![to, attempt],
                )?;
            }
            Change::TaskRemove { task } => {
                self.db
                    .execute("DELETE FROM tasks WHERE id = ?1", params![task.id])?;
            }
            Change::TaskAdd { task } => {
                self.db.execute(
                    "INSERT INTO tasks (id, goal, criteria, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![task.id, task.goal, task.criteria, task.created_at],
                )?;
            }
            Change::ViewPark { attempt } => {
                let a = self.attempt(*attempt)?;
                if a.backend == "overlay" {
                    #[cfg(target_os = "linux")]
                    if let Some((_, m)) = self.overlay_of(a.id)? {
                        if crate::overlay::is_mounted(&a.view_path) {
                            if let Err(e) = crate::overlay::unmount(m, &a.view_path) {
                                warnings.push(format!("the view of a{} stays mounted: {e}", a.id));
                            }
                        }
                        // The empty mount point; ensure_view makes it again.
                        let _ = fs::remove_dir(&a.view_path);
                    }
                } else if a.view_path.exists() {
                    let trash = self.trash_dir(a.id);
                    if trash.exists() {
                        fs::remove_dir_all(&trash).at(&trash)?;
                    }
                    fs::create_dir_all(trash.parent().expect("has parent"))
                        .at(trash.parent().expect("has parent"))?;
                    fs::rename(&a.view_path, &trash).at(&a.view_path)?;
                }
            }
            Change::ViewRestore { attempt } => {
                let a = self.attempt(*attempt)?;
                if a.backend == "overlay" {
                    if let Err(e) = self.ensure_view(&a) {
                        warnings.push(format!("the view of a{} did not mount: {e}", a.id));
                    }
                } else {
                    let trash = self.trash_dir(a.id);
                    if trash.exists() && !a.view_path.exists() {
                        fs::rename(&trash, &a.view_path).at(&trash)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// One line per change, for text output.
pub fn describe(changes: &[Change]) -> Vec<String> {
    changes.iter().map(Change::describe).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Ref};
    use crate::view::BackendChoice;

    fn repo() -> (crate::testutil::TestDir, Space) {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        fs::write(w.join("main.rs"), "fn main() {}\n").unwrap();
        let space = Space::init(w).unwrap();
        (tmp, space)
    }

    fn fork(space: &Space, task: i64, from: Option<i64>, backend: BackendChoice) -> i64 {
        space
            .fork(&ForkRequest {
                task_id: Some(task),
                from: from.map(Ref::Attempt),
                actor: "agent".into(),
                backend,
                ..Default::default()
            })
            .unwrap()
            .attempt
            .id
    }

    fn status(space: &Space, id: i64) -> (Status, Option<String>) {
        let a = space.attempt(id).unwrap();
        (a.status, a.reason)
    }

    #[test]
    fn op_log_lists_operations_newest_first() {
        let (_tmp, space) = repo();
        let t = space.create_task("goal", None).unwrap();
        let a = fork(&space, t.id, None, BackendChoice::Copy);
        let ops = space.operations(None).unwrap();
        let kinds: Vec<&str> = ops.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(kinds, ["attempt.fork", "task.create", "init"]);
        assert!(ops[0].id > ops[1].id);
        assert_eq!(ops[0].summary, format!("fork a{a} from a1 (copy)"));
        assert_eq!(ops[1].summary, "create t1 \"goal\"");
        assert!(ops[0].undoable && ops[1].undoable && !ops[2].undoable);
        assert_eq!(space.operations(Some(1)).unwrap().len(), 1);
    }

    #[test]
    fn undo_status_change_and_redo() {
        let (_tmp, space) = repo();
        let t = space.create_task("goal", None).unwrap();
        let a = fork(&space, t.id, None, BackendChoice::Copy);
        space
            .set_status(a, Status::Abandoned, Some("bad idea"))
            .unwrap();
        let r = space.undo("human", None).unwrap();
        assert_eq!(r.undid.kind, "attempt.abandoned");
        assert_eq!(r.undid.undone_by, Some(r.op));
        assert_eq!(status(&space, a), (Status::Exploring, None));
        // Undoing the same operation twice is refused, with the way to redo.
        let err = space.undo("human", Some(r.undid.id)).unwrap_err();
        assert_eq!(err.code(), "already_undone");
        // Undo of the undo is redo.
        let redo = space.undo("human", Some(r.op)).unwrap();
        assert_eq!(redo.undid.kind, "undo");
        assert_eq!(
            status(&space, a),
            (Status::Abandoned, Some("bad idea".into()))
        );
        assert_eq!(space.operation(r.undid.id).unwrap().undone_by, None);
        // And the redo can be undone again.
        space.undo("human", Some(redo.op)).unwrap();
        assert_eq!(status(&space, a), (Status::Exploring, None));
        assert_eq!(space.operation(r.undid.id).unwrap().undone_by, Some(r.op));
    }

    #[test]
    fn undo_promote_restores_siblings_and_ancestors() {
        let (_tmp, space) = repo();
        let t = space.create_task("goal", None).unwrap();
        let parent = fork(&space, t.id, None, BackendChoice::Auto);
        let sibling = fork(&space, t.id, None, BackendChoice::Auto);
        space.set_status(sibling, Status::Passed, None).unwrap();
        let child = fork(&space, t.id, Some(parent), BackendChoice::Auto);
        space.set_status(child, Status::Promoted, None).unwrap();
        assert_eq!(status(&space, sibling).0, Status::Abandoned);
        assert_eq!(status(&space, parent).0, Status::Promoted);

        let r = space.undo("human", None).unwrap();
        assert_eq!(r.undid.kind, "attempt.promoted");
        assert_eq!(status(&space, child), (Status::Exploring, None));
        assert_eq!(status(&space, sibling), (Status::Passed, None));
        assert_eq!(status(&space, parent), (Status::Exploring, None));
        // The reopened views work again.
        for id in [child, sibling, parent] {
            let a = space.attempt(id).unwrap();
            assert!(space.ensure_view(&a).unwrap());
            assert!(a.view_path.join("main.rs").exists());
        }
    }

    #[test]
    fn undo_refuses_when_later_operations_changed_the_attempt() {
        let (_tmp, space) = repo();
        let t = space.create_task("goal", None).unwrap();
        let a = fork(&space, t.id, None, BackendChoice::Copy);
        space.set_status(a, Status::Passed, None).unwrap();
        let passed = space.operations(Some(1)).unwrap()[0].id;
        space.set_status(a, Status::Promoted, None).unwrap();
        let err = space.undo("human", Some(passed)).unwrap_err();
        assert_eq!(err.code(), "undo_conflict");
        assert_eq!(status(&space, a).0, Status::Promoted);
        // Old-style and informational operations are not undoable.
        assert_eq!(
            space.undo("human", Some(1)).unwrap_err().code(),
            "not_undoable"
        );
    }

    #[test]
    fn undo_fork_puts_the_view_aside_and_redo_brings_it_back() {
        for backend in [BackendChoice::Copy, BackendChoice::Auto] {
            let (_tmp, space) = repo();
            let t = space.create_task("goal", None).unwrap();
            let a = fork(&space, t.id, None, backend);
            let view = space.attempt(a).unwrap().view_path;
            fs::write(view.join("work.txt"), "unsaved").unwrap();

            let r = space.undo("human", None).unwrap();
            assert_eq!(r.undid.kind, "attempt.fork");
            let (s, why) = status(&space, a);
            assert_eq!(s, Status::Abandoned);
            assert!(why.unwrap().starts_with("fork undone"));
            assert!(!view.join("work.txt").exists(), "{backend:?}");

            space.undo("human", Some(r.op)).unwrap();
            assert_eq!(status(&space, a), (Status::Exploring, None));
            let att = space.attempt(a).unwrap();
            assert!(space.ensure_view(&att).unwrap());
            assert_eq!(
                fs::read_to_string(view.join("work.txt")).unwrap(),
                "unsaved"
            );
        }
    }

    #[test]
    fn undo_task_creation() {
        let (_tmp, space) = repo();
        let t = space.create_task("goal", Some("tests pass")).unwrap();
        let a = fork(&space, t.id, None, BackendChoice::Copy);
        // A task with live attempts stays.
        let create = space.operations(None).unwrap()[1].id;
        assert_eq!(
            space.undo("human", Some(create)).unwrap_err().code(),
            "undo_conflict"
        );
        // Undo the fork, then the task: the fork leaves the task with it.
        space.undo("human", None).unwrap();
        let r = space.undo("human", None).unwrap();
        assert_eq!(r.undid.id, create);
        assert!(space.task(t.id).is_err());
        assert_eq!(space.attempt(a).unwrap().task_id, None);
        // Redo brings the task back whole, with its attempt.
        space.undo("human", Some(r.op)).unwrap();
        let back = space.task(t.id).unwrap();
        assert_eq!(back.goal, "goal");
        assert_eq!(back.criteria.as_deref(), Some("tests pass"));
        assert_eq!(space.attempt(a).unwrap().task_id, Some(t.id));
    }

    #[test]
    fn nothing_to_undo_in_a_new_space() {
        let (_tmp, space) = repo();
        assert_eq!(
            space.undo("human", None).unwrap_err().code(),
            "nothing_to_undo"
        );
    }
}
