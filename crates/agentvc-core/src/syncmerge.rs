//! Merging a synced space that another machine continued (#77).
//!
//! Row ids (`aN`, `sN`, ...) are per-machine counters, so two machines that
//! restored the same space hand out the same numbers to different attempts.
//! Every synced row therefore also carries a `uid` that is the same on every
//! machine. `agentvc sync` matches rows by uid, three-way against the state
//! this machine last synced:
//!
//! - rows only the remote has are added here, with ids free on this machine
//!   (the remote's id when it is free) and their references renumbered;
//! - a column only one side changed takes that side's value;
//! - a column both sides changed to different values is a conflict. For an
//!   attempt's `status` that is a `sync_conflict` naming the attempt and both
//!   values, unless `--prefer ours|theirs` picks a side; other columns keep
//!   this machine's value;
//! - rows this machine deleted since the last sync (an undone task) stay
//!   deleted.
//!
//! Short ids can differ between machines after a merge; the uid is what
//! stays the same.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};

use crate::error::{Error, IoContext, Result};
use crate::export::HostRepo;
use crate::objects::Oid;
use crate::space::{Space, SPACE_DIR};
use crate::sync::{self, CatFile};

/// Which side wins a column both machines changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Prefer {
    Ours,
    Theirs,
}

impl Prefer {
    pub fn parse(s: &str) -> Option<Prefer> {
        match s {
            "ours" => Some(Prefer::Ours),
            "theirs" => Some(Prefer::Theirs),
            _ => None,
        }
    }
}

/// A value both machines changed since they last agreed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldConflict {
    /// The row as this machine names it, e.g. `a3` or `tag good`.
    pub what: String,
    pub field: String,
    pub ours: String,
    pub theirs: String,
}

impl std::fmt::Display for FieldConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} is {} here but {} on the remote",
            self.what, self.field, self.ours, self.theirs
        )
    }
}

/// What a merge brought in.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MergeStats {
    /// The remote commit merged.
    pub from: String,
    pub tasks: usize,
    pub attempts: usize,
    pub snapshots: usize,
    /// Rows of any table added from the remote.
    pub rows: usize,
    /// Columns of existing rows that took the remote's value.
    pub updated: usize,
    /// Conflicts settled by `--prefer`.
    pub resolved: Vec<FieldConflict>,
    /// New remote attempts that got a view here.
    pub views: Vec<i64>,
}

/// Synced tables whose rows have an id and a uid. `story_changes` (#21)
/// is keyed by its attempt instead and merges as a whole stack.
pub(crate) const UID_TABLES: &[&str] = &[
    "tasks",
    "attempts",
    "snapshots",
    "tags",
    "runs",
    "exports",
    "git_bases",
    "operations",
];

/// References between synced tables, as (table, column, target table).
const REFS: &[(&str, &str, &str)] = &[
    ("attempts", "task_id", "tasks"),
    ("attempts", "parent_attempt", "attempts"),
    ("attempts", "parent_snapshot", "snapshots"),
    ("snapshots", "attempt_id", "attempts"),
    ("tags", "snapshot_id", "snapshots"),
    ("runs", "attempt_id", "attempts"),
    ("exports", "attempt_id", "attempts"),
    ("exports", "snapshot_id", "snapshots"),
    ("git_bases", "attempt_id", "attempts"),
    ("git_bases", "snapshot_id", "snapshots"),
];

/// Columns that describe this machine, not the space: never compared.
const LOCAL_COLUMNS: &[&str] = &["id", "uid", "view_path", "backend", "log_path", "pid"];

fn target(table: &str, col: &str) -> Option<&'static str> {
    REFS.iter()
        .find(|(t, c, _)| *t == table && *c == col)
        .map(|(_, _, to)| *to)
}

/// A fresh uid: 16 random hex digits.
pub(crate) fn new_uid() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = Sha1::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(
        COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_le_bytes(),
    );
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let mut buf = [0u8; 16];
        if f.read_exact(&mut buf).is_ok() {
            h.update(buf);
        }
    }
    hex::encode(&h.finalize()[..8])
}

/// The uid of a row synced before uids existed: derived from its id, so
/// every machine that restored the same sync derives the same one.
fn legacy_uid(table: &str, id: i64) -> String {
    format!("{table}-{id}")
}

/// Give every row without a uid one. A row that was in `base` (the state
/// last synced) before uids existed gets the uid derived from its id, as
/// every other machine with that sync will; anything newer gets a fresh one.
pub(crate) fn assign_uids(space: &Space, base: Option<&Value>) -> Result<()> {
    for t in UID_TABLES {
        let legacy: HashSet<i64> = base
            .and_then(|b| b["tables"][*t].as_array())
            .map(|rows| {
                rows.iter()
                    .filter(|r| r["uid"].is_null())
                    .filter_map(|r| r["id"].as_i64())
                    .collect()
            })
            .unwrap_or_default();
        let ids: Vec<i64> = space
            .db
            .prepare(&format!("SELECT id FROM {t} WHERE uid IS NULL"))?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in ids {
            let uid = if legacy.contains(&id) {
                legacy_uid(t, id)
            } else {
                new_uid()
            };
            space.db.execute(
                &format!("UPDATE {t} SET uid = ?1 WHERE id = ?2"),
                rusqlite::params![uid, id],
            )?;
        }
    }
    Ok(())
}

/// The rows of `table` in a dump, keyed by uid (old dumps: derived uids).
fn by_uid(doc: &Value, table: &str) -> Vec<(String, Map<String, Value>)> {
    doc["tables"][table]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let obj = r.as_object()?.clone();
                    let uid = match obj.get("uid").and_then(Value::as_str) {
                        Some(u) => u.to_string(),
                        None => legacy_uid(table, obj.get("id")?.as_i64()?),
                    };
                    Some((uid, obj))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `space.json` of a sync commit.
pub(crate) fn read_doc(repo: &HostRepo, commit: &str) -> Result<Value> {
    let text = repo.git(&["cat-file", "blob", &format!("{commit}:space.json")], None)?;
    serde_json::from_str(&text).map_err(|_| Error::BadObject(format!("{commit}:space.json")))
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "unset".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Merge the remote state `theirs` (sync commit `tip`) into this space,
/// three-way against `base`, the state this machine last synced. Changes
/// nothing when there are unresolved conflicts.
pub(crate) fn merge(
    space: &Space,
    repo: &HostRepo,
    tip: &str,
    base: Option<&Value>,
    theirs: &Value,
    prefer: Option<Prefer>,
) -> Result<MergeStats> {
    assign_uids(space, base)?;
    let ours = sync::dump(space, "")?;
    let mut stats = MergeStats {
        from: tip.to_string(),
        ..Default::default()
    };

    // uid -> local id per table, for everything that exists here.
    let mut local_ids: HashMap<&str, HashMap<String, i64>> = HashMap::new();
    let mut local_rows: HashMap<&str, HashMap<String, Map<String, Value>>> = HashMap::new();
    for t in UID_TABLES {
        let rows = by_uid(&ours, t);
        local_ids.insert(
            t,
            rows.iter()
                .map(|(u, r)| (u.clone(), r["id"].as_i64().unwrap_or(0)))
                .collect(),
        );
        local_rows.insert(t, rows.into_iter().collect());
    }
    let base_rows: HashMap<&str, HashMap<String, Map<String, Value>>> = UID_TABLES
        .iter()
        .map(|t| {
            (
                *t,
                base.map(|b| by_uid(b, t))
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
            )
        })
        .collect();

    // Ids for the remote's new rows: its own id when free here, else the
    // next free one. All ids are known before anything is written, so rows
    // can reference each other in any order.
    let mut remote_to_local: HashMap<&str, HashMap<i64, i64>> = HashMap::new();
    let mut new_rows: Vec<(&str, i64, Map<String, Value>)> = Vec::new();
    for t in UID_TABLES {
        let ids = &local_ids[t];
        let mut used: HashSet<i64> = ids.values().copied().collect();
        let mut next = used.iter().max().copied().unwrap_or(0) + 1;
        let map = remote_to_local.entry(t).or_default();
        for (uid, row) in by_uid(theirs, t) {
            let rid = row["id"].as_i64().unwrap_or(0);
            if let Some(&lid) = ids.get(&uid) {
                map.insert(rid, lid);
                continue;
            }
            if base_rows[t].contains_key(&uid) {
                continue; // deleted here since the last sync
            }
            let lid = if used.contains(&rid) {
                while used.contains(&next) {
                    next += 1;
                }
                next
            } else {
                rid
            };
            used.insert(lid);
            map.insert(rid, lid);
            new_rows.push((t, lid, row));
        }
    }
    let remap = |table: &str, col: &str, v: &Value| -> Value {
        match (target(table, col), v.as_i64()) {
            (Some(to), Some(rid)) => remote_to_local[to]
                .get(&rid)
                .map_or(Value::Null, |l| json!(l)),
            _ => v.clone(),
        }
    };

    // Changes to rows both sides have.
    let mut conflicts = Vec::new();
    let mut updates: Vec<(&str, i64, String, Value)> = Vec::new();
    for t in UID_TABLES {
        if *t == "operations" {
            continue; // never change once written
        }
        for (uid, r) in by_uid(theirs, t) {
            let Some(l) = local_rows[t].get(&uid) else {
                continue;
            };
            let b = base_rows[t].get(&uid);
            let lid = l["id"].as_i64().unwrap_or(0);
            for (col, rv) in &r {
                if LOCAL_COLUMNS.contains(&col.as_str()) || target(t, col).is_some() {
                    continue;
                }
                let lv = l.get(col).cloned().unwrap_or(Value::Null);
                if &lv == rv {
                    continue;
                }
                let bv = b.and_then(|b| b.get(col));
                if bv == Some(rv) {
                    continue; // only we changed it
                }
                if bv == Some(&lv) {
                    updates.push((t, lid, col.clone(), rv.clone()));
                    continue;
                }
                // Both changed it. Only an attempt's status is worth stopping
                // for; the rest (reasons, close times) follow it or keep ours.
                let what = match *t {
                    "attempts" => format!("a{lid}"),
                    "tags" => format!("tag {}", show(&l["name"])),
                    _ => continue,
                };
                if *t == "attempts" && col != "status" {
                    continue;
                }
                conflicts.push(FieldConflict {
                    what,
                    field: col.clone(),
                    ours: show(&lv),
                    theirs: show(rv),
                });
            }
        }
    }
    // A tag name both machines used for different snapshots.
    let local_tags: HashMap<String, String> = local_rows["tags"]
        .iter()
        .filter_map(|(u, r)| Some((r.get("name")?.as_str()?.to_string(), u.clone())))
        .collect();
    let mut skip_tags = HashSet::new();
    for (t, lid, row) in &new_rows {
        if *t != "tags" {
            continue;
        }
        if let Some(name) = row["name"].as_str() {
            if local_tags.contains_key(name) {
                conflicts.push(FieldConflict {
                    what: format!("tag {name}"),
                    field: "snapshot".into(),
                    ours: "a tag here".into(),
                    theirs: "a different tag on the remote".into(),
                });
                skip_tags.insert(*lid);
            }
        }
    }

    match prefer {
        None if !conflicts.is_empty() => {
            return Err(Error::SyncDiverged {
                tip: tip.to_string(),
                conflicts,
            })
        }
        Some(Prefer::Theirs) => {
            for c in &conflicts {
                if let Some(id) = c.what.strip_prefix('a').and_then(|n| n.parse::<i64>().ok()) {
                    let r = by_uid(theirs, "attempts")
                        .into_iter()
                        .find(|(u, _)| local_ids["attempts"].get(u) == Some(&id))
                        .map(|(_, r)| r);
                    if let Some(r) = r {
                        for col in ["status", "reason", "closed_at"] {
                            if let Some(v) = r.get(col) {
                                updates.push(("attempts", id, col.into(), v.clone()));
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    stats.resolved = conflicts;

    // A story (#21) is one stack per attempt: take the remote's when this
    // machine has none for that attempt.
    let has_story: HashSet<i64> = ours["tables"]["story_changes"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["attempt_id"].as_i64())
                .collect()
        })
        .unwrap_or_default();
    let stories: Vec<Map<String, Value>> = theirs["tables"]["story_changes"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let mut o = r.as_object()?.clone();
                    let a = remote_to_local["attempts"].get(&o.get("attempt_id")?.as_i64()?)?;
                    if has_story.contains(a) {
                        return None;
                    }
                    o.insert("attempt_id".into(), json!(a));
                    let src = o.get("source_snapshot").and_then(Value::as_i64);
                    o.insert(
                        "source_snapshot".into(),
                        src.and_then(|s| remote_to_local["snapshots"].get(&s))
                            .map_or(Value::Null, |l| json!(l)),
                    );
                    Some(o)
                })
                .collect()
        })
        .unwrap_or_default();

    // Objects before rows, so no row points at a missing tree.
    let mut git = CatFile::start(space.workdir())?;
    for c in &stories {
        for k in ["tree", "base_tree"] {
            if let Some(tree) = c.get(k).and_then(Value::as_str) {
                sync::import_tree(space, &mut git, &Oid::from_hex(tree)?)?;
            }
        }
    }
    for (t, _, row) in &new_rows {
        if *t == "snapshots" && row["pruned_at"].is_null() {
            if let Some(tree) = row["tree"].as_str() {
                sync::import_tree(space, &mut git, &Oid::from_hex(tree)?)?;
            }
        }
    }

    let dir = space.workdir().join(SPACE_DIR);
    let runs_dir = dir.join("runs");
    space.db.pragma_update(None, "foreign_keys", "OFF")?;
    let tx = space.db.unchecked_transaction()?;
    for (t, lid, row) in &new_rows {
        if *t == "tags" && skip_tags.contains(lid) {
            continue;
        }
        let rid = row["id"].as_i64().unwrap_or(0);
        let mut obj = Map::new();
        for (k, v) in row {
            obj.insert(k.clone(), remap(t, k, v));
        }
        obj.insert("id".into(), json!(lid));
        match *t {
            "attempts" => {
                obj.insert(
                    "view_path".into(),
                    json!(dir.join("views").join(format!("a{lid}"))),
                );
                obj.insert("backend".into(), json!("copy"));
                stats.attempts += 1;
            }
            "runs" if !row["log_path"].is_null() => {
                let log = runs_dir.join(format!("r{lid}.log"));
                let blob = repo
                    .git(
                        &[
                            "rev-parse",
                            "--verify",
                            "-q",
                            &format!("{tip}:logs/r{rid}.log"),
                        ],
                        None,
                    )
                    .ok()
                    .and_then(|h| Oid::from_hex(h.trim()).ok());
                if let Some(oid) = blob {
                    let (_, body) = git.read(&oid)?;
                    fs::create_dir_all(&runs_dir).at(&runs_dir)?;
                    fs::write(&log, body).at(&log)?;
                }
                obj.insert("log_path".into(), json!(log));
            }
            "operations" => {
                // Its ids are the other machine's: keep it for the record,
                // but `undo` must not act on it here.
                let mut p: Value = row["payload"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(Value::Null);
                if let Some(o) = p.as_object_mut() {
                    o.entry("merged_from").or_insert(json!(tip));
                }
                obj.insert("payload".into(), json!(p.to_string()));
            }
            "tasks" => stats.tasks += 1,
            "snapshots" => stats.snapshots += 1,
            _ => {}
        }
        let cols = sync::columns(&tx, t)?;
        let (names, values): (Vec<String>, Vec<SqlValue>) = obj
            .iter()
            .filter(|(k, _)| cols.contains(*k))
            .map(|(k, v)| (k.clone(), sync::json_to_sql(v)))
            .unzip();
        let marks: Vec<String> = (1..=names.len()).map(|i| format!("?{i}")).collect();
        tx.execute(
            &format!(
                "INSERT INTO {t} ({}) VALUES ({})",
                names.join(", "),
                marks.join(", ")
            ),
            rusqlite::params_from_iter(values),
        )?;
        stats.rows += 1;
    }
    for c in &stories {
        let cols = sync::columns(&tx, "story_changes")?;
        let (names, values): (Vec<String>, Vec<SqlValue>) = c
            .iter()
            .filter(|(k, _)| cols.contains(*k))
            .map(|(k, v)| (k.clone(), sync::json_to_sql(v)))
            .unzip();
        let marks: Vec<String> = (1..=names.len()).map(|i| format!("?{i}")).collect();
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO story_changes ({}) VALUES ({})",
                names.join(", "),
                marks.join(", ")
            ),
            rusqlite::params_from_iter(values),
        )?;
        stats.rows += 1;
    }
    for (t, id, col, v) in &updates {
        tx.execute(
            &format!("UPDATE {t} SET {col} = ?1 WHERE id = ?2"),
            rusqlite::params![sync::json_to_sql(v), id],
        )?;
        stats.updated += 1;
    }
    // Space settings the remote added (never this machine's own).
    if let Some(meta) = theirs["meta"].as_object() {
        for (k, v) in meta {
            if let (Some(v), false) = (v.as_str(), sync::is_local_meta(k)) {
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING",
                    rusqlite::params![k, v],
                )?;
            }
        }
    }
    tx.commit()?;
    space.db.pragma_update(None, "foreign_keys", "ON")?;

    stats.views = sync::open_views(space)?;
    // Not recorded as an operation: the other machine would merge that
    // record back, record its own, and the two would never settle.
    Ok(stats)
}
