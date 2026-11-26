//! Merging other spaces into this one, so every PR's attempt tree can be
//! browsed in one place (the review UI, the VS Code extension, the side panel
//! and `agentvc overview`).
//!
//! Each cloud thread keeps its space in a collapsed comment on its PR (see
//! [`crate::bundle`]). [`merge_bundle`] takes one such payload and adds its
//! tasks, attempts, snapshots, runs, exports, git bases and tags to this
//! space under fresh ids, then rebuilds each attempt's files from the
//! comment's patches. The other space's root attempt becomes an ordinary
//! closed attempt (actor [`IMPORT_ACTOR`]) that its forks hang from, so this
//! space's own `a1` is never touched. Views are not written; `agentvc path`
//! and the review UI make them on demand.
//!
//! What was imported is remembered under the meta key `import:<space id>`, so
//! importing a newer comment of the same space replaces the earlier copy
//! instead of duplicating it, and an unchanged comment is skipped.

use std::collections::BTreeMap;
use std::fs;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::bundle;
use crate::error::{IoContext, Result};
use crate::export::{HostRepo, IMPORT_BASE_KEY};
use crate::space::{Space, ROOT_ATTEMPT, SPACE_DIR};
use crate::sync::{self, CatFile};

/// Actor of the attempt that stands in for an imported space's root.
pub const IMPORT_ACTOR: &str = "import";
/// Meta key prefix that records one imported space.
pub const META_PREFIX: &str = "import:";

/// Where an imported space came from and which rows it occupies here.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImportRecord {
    pub space_id: String,
    /// `owner/repo#N`.
    pub source: String,
    /// Version of the source, e.g. the comment's `updated_at`; an import with
    /// the same version is skipped.
    pub version: String,
    /// The attempt standing in for the imported root.
    pub root: i64,
    pub tasks: Vec<i64>,
    pub attempts: Vec<i64>,
    pub snapshots: Vec<i64>,
    pub runs: Vec<i64>,
    pub exports: Vec<i64>,
    pub git_bases: Vec<i64>,
    pub tags: Vec<i64>,
    #[serde(default)]
    pub annotations: Vec<i64>,
    #[serde(default)]
    pub checks: Vec<i64>,
}

/// What [`merge_bundle`] did with one space.
#[derive(Clone, Debug, Serialize)]
pub struct MergeResult {
    pub space_id: String,
    pub source: String,
    /// `imported`, `replaced`, `unchanged` or `own` (this space itself).
    pub outcome: &'static str,
    pub tasks: Vec<String>,
    pub attempts: usize,
    /// Attempts whose files were rebuilt exactly.
    pub rebuilt: usize,
    /// Attempts that have history only.
    pub incomplete: Vec<String>,
}

fn max_id(space: &Space, table: &str) -> Result<i64> {
    Ok(space.db.query_row(
        &format!("SELECT COALESCE(MAX(id), 0) FROM {table}"),
        [],
        |r| r.get(0),
    )?)
}

/// Every space imported into this one, by space id.
pub fn imported(space: &Space) -> Result<BTreeMap<String, ImportRecord>> {
    let mut stmt = space
        .db
        .prepare("SELECT key, value FROM meta WHERE key LIKE 'import:%'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (k, v) = row?;
        if let Ok(rec) = serde_json::from_str::<ImportRecord>(&v) {
            out.insert(k[META_PREFIX.len()..].to_string(), rec);
        }
    }
    Ok(out)
}

/// Remove the rows of an earlier import.
fn remove(space: &Space, rec: &ImportRecord) -> Result<()> {
    // Machine-local rows (layers, overlays of views made since) may still
    // point at these attempts; like `insert_rows`, check keys per table only.
    space.db.pragma_update(None, "foreign_keys", "OFF")?;
    let r = remove_rows(space, rec);
    space.db.pragma_update(None, "foreign_keys", "ON")?;
    r
}

fn remove_rows(space: &Space, rec: &ImportRecord) -> Result<()> {
    let tx = space.db.unchecked_transaction()?;
    for id in &rec.attempts {
        tx.execute("DELETE FROM overlays WHERE attempt_id = ?1", params![id])?;
    }
    let del = |table: &str, ids: &[i64]| -> Result<()> {
        for id in ids {
            tx.execute(&format!("DELETE FROM {table} WHERE id = ?1"), params![id])?;
        }
        Ok(())
    };
    del("annotations", &rec.annotations)?;
    del("checks", &rec.checks)?;
    del("tags", &rec.tags)?;
    del("git_bases", &rec.git_bases)?;
    del("exports", &rec.exports)?;
    del("runs", &rec.runs)?;
    del("snapshots", &rec.snapshots)?;
    del("attempts", &rec.attempts)?;
    del("tasks", &rec.tasks)?;
    tx.execute(
        "DELETE FROM meta WHERE key = ?1",
        params![format!("{META_PREFIX}{}", rec.space_id)],
    )?;
    tx.commit()?;
    let runs = space.workdir().join(SPACE_DIR).join("runs");
    for r in &rec.runs {
        let _ = fs::remove_file(runs.join(format!("r{r}.log")));
    }
    Ok(())
}

/// Id maps for one import: old id -> new id, per table.
#[derive(Default)]
struct Ids {
    tasks: BTreeMap<i64, i64>,
    attempts: BTreeMap<i64, i64>,
    snapshots: BTreeMap<i64, i64>,
}

fn remap(v: &Value, map: &BTreeMap<i64, i64>) -> Value {
    match v.as_i64() {
        Some(old) => map.get(&old).map_or(Value::Null, |n| json!(n)),
        None => Value::Null,
    }
}

/// Rewrite attempt refs (`aN`) in free text such as a reason ("lost to a5",
/// "continued in a7") to their new ids; other words stay as they are.
fn remap_refs(text: &str, map: &BTreeMap<i64, i64>) -> String {
    let mut out = String::with_capacity(text.len());
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let word_start = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if b[i] == b'a' && word_start {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let word_end = j == b.len() || !b[j].is_ascii_alphanumeric();
            if j > i + 1 && word_end {
                if let Some(n) = text[i + 1..j].parse().ok().and_then(|o: i64| map.get(&o)) {
                    out.push_str(&format!("a{n}"));
                    i = j;
                    continue;
                }
            }
        }
        let c = text[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8().max(1);
    }
    out
}

/// Merge a decoded space comment (see [`bundle::decode`]) into `space`.
/// `source` names it (`owner/repo#N`) and `version` tells newer copies apart.
pub fn merge_bundle(
    space: &Space,
    payload: &Value,
    source: &str,
    version: &str,
) -> Result<MergeResult> {
    let doc = &payload["space"];
    let space_id = doc["space_id"].as_str().unwrap_or_default().to_string();
    let mut result = MergeResult {
        space_id: space_id.clone(),
        source: source.to_string(),
        outcome: "imported",
        tasks: Vec::new(),
        attempts: 0,
        rebuilt: 0,
        incomplete: Vec::new(),
    };
    if space_id.is_empty() || space_id == sync::space_id(space)? {
        result.outcome = "own";
        return Ok(result);
    }
    if let Some(prev) = imported(space)?.remove(&space_id) {
        if prev.version == version && prev.source == source {
            result.outcome = "unchanged";
            result.tasks = prev.tasks.iter().map(|t| format!("t{t}")).collect();
            result.attempts = prev.attempts.iter().filter(|a| **a != prev.root).count();
            return Ok(result);
        }
        remove(space, &prev)?;
        result.outcome = "replaced";
    }

    let rows = |t: &str| doc["tables"][t].as_array().cloned().unwrap_or_default();
    let mut rec = ImportRecord {
        space_id: space_id.clone(),
        source: source.to_string(),
        version: version.to_string(),
        ..Default::default()
    };
    let mut ids = Ids::default();
    let next = |table: &str| -> Result<i64> { max_id(space, table) };

    // Fresh ids, in each table's own order.
    let (mut t_next, mut a_next, mut s_next) = (
        next("tasks")? + 1,
        next("attempts")? + 1,
        next("snapshots")? + 1,
    );
    for r in rows("tasks") {
        if let Some(old) = r["id"].as_i64() {
            ids.tasks.insert(old, t_next);
            t_next += 1;
        }
    }
    for r in rows("attempts") {
        if let Some(old) = r["id"].as_i64() {
            ids.attempts.insert(old, a_next);
            a_next += 1;
        }
    }
    for r in rows("snapshots") {
        if let Some(old) = r["id"].as_i64() {
            ids.snapshots.insert(old, s_next);
            s_next += 1;
        }
    }
    rec.root = ids.attempts.get(&ROOT_ATTEMPT).copied().unwrap_or(0);

    let mut out = Map::new();
    let set = |obj: &mut Map<String, Value>, k: &str, v: Value| {
        obj.insert(k.to_string(), v);
    };
    let mut tables: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for r in rows("tasks") {
        let mut o = r.as_object().cloned().unwrap_or_default();
        let id = remap(&r["id"], &ids.tasks);
        rec.tasks.extend(id.as_i64());
        set(&mut o, "id", id);
        tables.entry("tasks").or_default().push(Value::Object(o));
    }
    for r in rows("attempts") {
        let mut o = r.as_object().cloned().unwrap_or_default();
        let old = r["id"].as_i64().unwrap_or(0);
        let id = remap(&r["id"], &ids.attempts);
        rec.attempts.extend(id.as_i64());
        set(&mut o, "id", id);
        set(&mut o, "task_id", remap(&r["task_id"], &ids.tasks));
        set(
            &mut o,
            "parent_attempt",
            remap(&r["parent_attempt"], &ids.attempts),
        );
        set(
            &mut o,
            "parent_snapshot",
            remap(&r["parent_snapshot"], &ids.snapshots),
        );
        if let Some(reason) = r["reason"].as_str() {
            set(&mut o, "reason", json!(remap_refs(reason, &ids.attempts)));
        }
        if old == ROOT_ATTEMPT {
            // The other space's working directory: closed, so nothing works in it.
            set(&mut o, "actor", json!(IMPORT_ACTOR));
            set(&mut o, "status", json!("promoted"));
            set(
                &mut o,
                "reason",
                json!(format!("root of space {space_id} from {source}")),
            );
        }
        tables.entry("attempts").or_default().push(Value::Object(o));
    }
    for r in rows("snapshots") {
        let mut o = r.as_object().cloned().unwrap_or_default();
        let id = remap(&r["id"], &ids.snapshots);
        rec.snapshots.extend(id.as_i64());
        set(&mut o, "id", id);
        set(&mut o, "attempt_id", remap(&r["attempt_id"], &ids.attempts));
        tables
            .entry("snapshots")
            .or_default()
            .push(Value::Object(o));
    }
    // Tables nothing else points at: fresh ids by offset.
    let mut run_ids: BTreeMap<i64, i64> = BTreeMap::new();
    let short = &space_id[..space_id.len().min(8)];
    for table in [
        "tags",
        "runs",
        "exports",
        "git_bases",
        "annotations",
        "checks",
    ] {
        let base = max_id(space, table)?;
        for (i, r) in rows(table).into_iter().enumerate() {
            let mut o = r.as_object().cloned().unwrap_or_default();
            let id = base + 1 + i as i64;
            if table == "runs" {
                if let Some(old) = r["id"].as_i64() {
                    run_ids.insert(old, id);
                }
            }
            set(&mut o, "id", json!(id));
            if o.contains_key("attempt_id") {
                set(&mut o, "attempt_id", remap(&r["attempt_id"], &ids.attempts));
            }
            if o.contains_key("snapshot_id") {
                set(
                    &mut o,
                    "snapshot_id",
                    remap(&r["snapshot_id"], &ids.snapshots),
                );
            }
            if table == "checks" {
                // Points at the other space's check ids.
                set(&mut o, "reused_from", Value::Null);
            }
            if table == "tags" {
                // Tag names are unique per space.
                let name = r["name"].as_str().unwrap_or_default();
                set(&mut o, "name", json!(format!("{short}/{name}")));
            }
            match table {
                "tags" => rec.tags.push(id),
                "runs" => rec.runs.push(id),
                "exports" => rec.exports.push(id),
                "annotations" => rec.annotations.push(id),
                "checks" => rec.checks.push(id),
                _ => rec.git_bases.push(id),
            }
            tables.entry(table).or_default().push(Value::Object(o));
        }
    }
    for (k, v) in tables {
        out.insert(k.to_string(), Value::Array(v));
    }
    // Operations refer to the old ids and stay behind; meta belongs to this space.
    sync::insert_rows(space, &json!({ "tables": out }))?;

    // Files: the commit the other space started from, then each attempt's
    // latest tree from the comment's patches.
    let repo = HostRepo::open(space.workdir())?;
    let mut git = CatFile::start(space.workdir())?;
    if let Some(head) = doc["meta"][IMPORT_BASE_KEY].as_str() {
        if repo.rev(head).is_none() {
            let _ = repo.git(&["fetch", "--quiet", "--no-tags", "origin", head], None);
        }
        let _ = bundle::base_tree(space, &repo, &mut git, head);
    }
    let mut patches = Map::new();
    if let Some(m) = payload["patches"].as_object() {
        for (k, p) in m {
            let Some(new) = k.parse::<i64>().ok().and_then(|o| ids.attempts.get(&o)) else {
                continue;
            };
            let mut p = p.clone();
            if let Some(o) = p.as_object_mut() {
                let from = remap(
                    &o.get("from_attempt").cloned().unwrap_or_default(),
                    &ids.attempts,
                );
                o.insert("from_attempt".into(), from);
            }
            patches.insert(new.to_string(), p);
        }
    }
    let (rebuilt, _) = bundle::rebuild_trees(space, &repo, &mut git, &Value::Object(patches));

    let runs_dir = space.workdir().join(SPACE_DIR).join("runs");
    if let Some(logs) = payload["logs"].as_object() {
        fs::create_dir_all(&runs_dir).at(&runs_dir)?;
        for (name, data) in logs {
            let new = name
                .strip_prefix('r')
                .and_then(|n| n.parse::<i64>().ok())
                .and_then(|o| run_ids.get(&o));
            if let (Some(new), Some(bytes)) = (new, data.as_str().and_then(bundle::b64_decode)) {
                let path = runs_dir.join(format!("r{new}.log"));
                fs::write(&path, bytes).at(&path)?;
            }
        }
    }

    space.set_meta(
        &format!("{META_PREFIX}{space_id}"),
        &serde_json::to_string(&rec).unwrap_or_default(),
    )?;
    space.record(
        "human",
        "space.import",
        json!({ "space_id": space_id, "source": source, "tasks": rec.tasks, "root": rec.root }),
    )?;
    result.tasks = rec.tasks.iter().map(|t| format!("t{t}")).collect();
    result.attempts = rec.attempts.iter().filter(|a| **a != rec.root).count();
    result.rebuilt = rebuilt.len();
    result.incomplete = rec
        .attempts
        .iter()
        .filter(|a| **a != rec.root && !rebuilt.contains(a))
        .map(|a| format!("a{a}"))
        .collect();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Ref, Status};
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
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
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo with a space that has one task: a winner, a loser with a
    /// reason, and a follow-up of the winner.
    fn other_space(dir: &Path, goal: &str) -> Value {
        let space = Space::init(dir).unwrap();
        let task = space.create_task(goal, Some("works")).unwrap();
        let fork = |from: i64, actor: &str| {
            space
                .fork(&ForkRequest {
                    task_id: Some(task.id),
                    from: Some(Ref::Attempt(from)),
                    actor: actor.into(),
                    backend: crate::view::BackendChoice::Copy,
                    ..Default::default()
                })
                .unwrap()
                .attempt
        };
        let a = fork(1, "agent:one");
        let b = fork(1, "agent:two");
        fs::write(
            a.view_path.join("src/lib.rs"),
            format!("// {goal}\nfn f() {{}}\n"),
        )
        .unwrap();
        space.snapshot(a.id, "tool", None).unwrap();
        fs::write(b.view_path.join("new.txt"), "b\n").unwrap();
        space.snapshot(b.id, "tool", None).unwrap();
        let c = fork(a.id, "agent:one");
        fs::write(c.view_path.join("more.txt"), "c\n").unwrap();
        space.snapshot(c.id, "tool", None).unwrap();
        space
            .set_status(b.id, Status::Abandoned, Some("slower"))
            .unwrap();
        space.set_status(c.id, Status::Promoted, None).unwrap();
        bundle::decode(&bundle::encode(&space, 7).unwrap().body).unwrap()
    }

    fn repo(dir: &Path) {
        fs::create_dir_all(dir.join("src")).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("src/lib.rs"), "fn f() {}\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "base"]);
    }

    #[test]
    fn merges_two_spaces_with_fresh_ids_and_replaces_on_reimport() {
        let tmp = crate::testutil::tempdir();
        let (x, y, here) = (
            tmp.path().join("x"),
            tmp.path().join("y"),
            tmp.path().join("h"),
        );
        repo(&x);
        for d in [&y, &here] {
            git(
                tmp.path(),
                &["clone", "-q", x.to_str().unwrap(), d.to_str().unwrap()],
            );
        }
        let px = other_space(&x, "goal x");
        let py = other_space(&y, "goal y");

        let space = Space::init(&here).unwrap();
        let own = space.create_task("my own task", None).unwrap();
        let rx = merge_bundle(&space, &px, "o/r#7", "v1").unwrap();
        let ry = merge_bundle(&space, &py, "o/r#8", "v1").unwrap();
        assert_eq!((rx.outcome, ry.outcome), ("imported", "imported"));
        assert_eq!(rx.attempts, 3);
        assert_eq!(rx.rebuilt, 3, "{rx:?}");
        assert!(rx.incomplete.is_empty());

        let tasks = space.tasks().unwrap();
        let goals: Vec<_> = tasks.iter().map(|t| t.goal.as_str()).collect();
        assert_eq!(goals, ["my own task", "goal x", "goal y"]);
        assert_eq!(tasks[0].id, own.id);
        // Our own root is untouched; each import has its own stand-in root.
        assert_eq!(space.attempt(ROOT_ATTEMPT).unwrap().actor, "human");
        let ty = &tasks[2];
        let attempts = space.attempts(Some(ty.id)).unwrap();
        assert_eq!(attempts.len(), 3);
        let roots: Vec<_> = space
            .attempts(None)
            .unwrap()
            .into_iter()
            .filter(|a| a.actor == IMPORT_ACTOR)
            .collect();
        assert_eq!(roots.len(), 2);
        assert_eq!(attempts[0].parent_attempt, Some(roots[1].id));
        let loser = attempts.iter().find(|a| a.actor == "agent:two").unwrap();
        assert_eq!(loser.status, Status::Abandoned);
        assert_eq!(loser.reason.as_deref(), Some("slower"));
        // Files came back: the follow-up has its parent's edit and its own file.
        let winner = attempts
            .iter()
            .find(|a| a.status == Status::Promoted && a.reason.is_none())
            .unwrap();
        let changes = space
            .diff(Ref::Attempt(roots[1].id), Ref::Attempt(winner.id))
            .unwrap();
        let paths: Vec<_> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["more.txt", "src/lib.rs"]);

        // Same version: nothing to do. Newer version: replaced, not duplicated.
        let again = merge_bundle(&space, &px, "o/r#7", "v1").unwrap();
        assert_eq!(again.outcome, "unchanged");
        let newer = merge_bundle(&space, &px, "o/r#9", "v2").unwrap();
        assert_eq!(newer.outcome, "replaced");
        assert_eq!(space.tasks().unwrap().len(), 3);
        assert_eq!(imported(&space).unwrap().len(), 2);

        // The overview names where each task came from.
        let o = space.overview().unwrap();
        let tx = o.tasks.iter().find(|t| t.goal == "goal x").unwrap();
        assert_eq!(tx.source.as_deref(), Some("o/r#9"));
        assert!(
            o.untasked.is_empty(),
            "import roots stay out of the overview"
        );
    }

    #[test]
    fn reasons_follow_the_new_ids() {
        let map: BTreeMap<i64, i64> = [(5, 31), (7, 40)].into();
        assert_eq!(remap_refs("lost to a5", &map), "lost to a31");
        assert_eq!(
            remap_refs("continued in a7; see a9", &map),
            "continued in a40; see a9"
        );
        assert_eq!(remap_refs("data5 a5x é a7", &map), "data5 a5x é a40");
    }

    #[test]
    fn own_space_is_not_imported() {
        let tmp = crate::testutil::tempdir();
        let x = tmp.path().join("x");
        repo(&x);
        let p = other_space(&x, "goal");
        let space = Space::discover(&x).unwrap();
        let r = merge_bundle(&space, &p, "o/r#1", "v1").unwrap();
        assert_eq!(r.outcome, "own");
    }
}
