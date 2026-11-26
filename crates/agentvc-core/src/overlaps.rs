//! Conflict prevention (#94): which open attempts change the same files.
//!
//! The cheapest conflict is the one found while both attempts are still
//! open. For every pair of open attempts of different tasks (siblings of one
//! task are meant to overlap, and an attempt never overlaps its own
//! ancestors or descendants), [`Space::overlaps`] lists the files both
//! changed relative to their bases, and which of those already conflict in
//! a trial three-way merge after auto-resolve. `agentvc overlaps`, `agentvc
//! status`, the daemon's `/api/overlaps` and a one-time note in the agent's
//! hook output surface it.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::error::Result;
use crate::objects::{Mode, Oid};
use crate::resolve::{merge_file, Rules, Sides};
use crate::snapshot;
use crate::space::{Attempt, Space, ROOT_ATTEMPT};

/// Two open attempts that changed the same files.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Overlap {
    pub attempt: i64,
    pub other: i64,
    pub other_task: Option<i64>,
    pub other_actor: String,
    /// Files both changed.
    pub files: Vec<String>,
    /// Of those, the ones that would conflict if both landed, even after
    /// auto-resolve.
    pub conflicting: Vec<String>,
}

/// An open attempt's changed files: path -> (mode, blob) now, `None` when
/// removed; plus the tree they are relative to.
struct Changes {
    base: BTreeMap<String, (Mode, Oid)>,
    files: BTreeMap<String, Option<(Mode, Oid)>>,
}

impl Space {
    fn attempt_changes(&self, a: &Attempt) -> Result<Option<Changes>> {
        let base_tree = match self.own_git_base(a.id)? {
            Some(b) => b.tree,
            None => match a.parent_snapshot {
                Some(p) => {
                    // Up the chain to the first snapshot with a git base,
                    // else the root's: what `export` would diff against.
                    let p = self.snapshot_by_id(p)?;
                    match self.git_base(&p)? {
                        Some(b) => b.tree,
                        None => p.tree,
                    }
                }
                None => return Ok(None),
            },
        };
        let head = self.head_snapshot(a.id)?;
        let store = self.objects();
        let (base_oid, head_oid) = (Oid::from_hex(&base_tree)?, head.tree_oid()?);
        if !store.contains(&base_oid) {
            return Ok(None);
        }
        let base = snapshot::flatten(store, &base_oid)?;
        let now = snapshot::flatten(store, &head_oid)?;
        let mut files = BTreeMap::new();
        for c in snapshot::diff(store, &base_oid, &head_oid)? {
            files.insert(c.path.clone(), now.get(&c.path).cloned());
        }
        Ok(Some(Changes { base, files }))
    }

    /// Overlaps of `attempt` with the other open attempts, or of every pair
    /// when `None`. Based on each attempt's latest snapshot.
    pub fn overlaps(&self, attempt: Option<i64>) -> Result<Vec<Overlap>> {
        let open: Vec<Attempt> = self
            .attempts(None)?
            .into_iter()
            .filter(|a| a.id != ROOT_ATTEMPT && a.status.is_open())
            .collect();
        let mut changes: BTreeMap<i64, Changes> = BTreeMap::new();
        for a in &open {
            if let Some(c) = self.attempt_changes(a)? {
                changes.insert(a.id, c);
            }
        }
        let rules = Rules::new();
        let mut out = Vec::new();
        for (i, a) in open.iter().enumerate() {
            for b in open.iter().skip(i + 1) {
                if let Some(x) = attempt {
                    if a.id != x && b.id != x {
                        continue;
                    }
                }
                if a.task_id.is_some() && a.task_id == b.task_id {
                    continue;
                }
                if self.descends_from(a.id, b.id)? || self.descends_from(b.id, a.id)? {
                    continue;
                }
                let (Some(ca), Some(cb)) = (changes.get(&a.id), changes.get(&b.id)) else {
                    continue;
                };
                let both: BTreeSet<&String> = ca
                    .files
                    .keys()
                    .filter(|p| cb.files.contains_key(*p))
                    .collect();
                if both.is_empty() {
                    continue;
                }
                let mut conflicting = Vec::new();
                for p in &both {
                    let (fa, fb) = (&ca.files[*p], &cb.files[*p]);
                    if fa == fb {
                        continue;
                    }
                    let text = |f: Option<&(Mode, Oid)>| -> Option<String> {
                        match f {
                            None => Some(String::new()),
                            Some((_, oid)) => {
                                String::from_utf8(self.objects().read_blob(oid).ok()?)
                                    .ok()
                                    .filter(|t| !t.contains('\0'))
                            }
                        }
                    };
                    let base = text(ca.base.get(*p));
                    let (ta, tb) = (text(fa.as_ref()), text(fb.as_ref()));
                    let clean = match (base, ta, tb, fa, fb) {
                        (Some(base), Some(ta), Some(tb), Some(_), Some(_)) => {
                            let m = merge_file(
                                Some(&base),
                                &ta,
                                &tb,
                                rules.strategy_for(p),
                                None,
                                &Sides::default(),
                            );
                            m.conflicts == 0
                        }
                        _ => false,
                    };
                    if !clean {
                        conflicting.push((*p).clone());
                    }
                }
                let files: Vec<String> = both.into_iter().cloned().collect();
                let entry = |me: &Attempt, other: &Attempt| Overlap {
                    attempt: me.id,
                    other: other.id,
                    other_task: other.task_id,
                    other_actor: other.actor.clone(),
                    files: files.clone(),
                    conflicting: conflicting.clone(),
                };
                match attempt {
                    Some(x) if x == b.id => out.push(entry(b, a)),
                    _ => out.push(entry(a, b)),
                }
            }
        }
        Ok(out)
    }

    /// A note for the agent working in `attempt` the first time it changes
    /// `path` while another open attempt changes it too; `None` after that,
    /// or when nothing overlaps.
    pub fn overlap_notice(&self, attempt: i64, path: &str) -> Result<Option<String>> {
        let hits: Vec<Overlap> = self
            .overlaps(Some(attempt))?
            .into_iter()
            .filter(|o| o.files.iter().any(|f| f == path))
            .collect();
        let mut fresh = Vec::new();
        for o in &hits {
            let key = format!("overlap_noted.a{attempt}.a{}.{path}", o.other);
            if self.meta(&key)?.is_none() {
                self.set_meta(&key, "1")?;
                fresh.push(o);
            }
        }
        if fresh.is_empty() {
            return Ok(None);
        }
        let others: Vec<String> = fresh
            .iter()
            .map(|o| {
                let task = o
                    .other_task
                    .map(|t| format!(", task t{t}"))
                    .unwrap_or_default();
                let clash = if o.conflicting.iter().any(|f| f == path) {
                    " and the two versions already conflict"
                } else {
                    ", and so far they merge cleanly"
                };
                format!(
                    "a{} ({}{task}) also changes {path}{clash}",
                    o.other, o.other_actor
                )
            })
            .collect();
        Ok(Some(format!(
            "agentvc: {}. Keep edits to this file small and local, or coordinate; `agentvc overlaps a{attempt}` lists every shared file.",
            others.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ForkRequest;
    use std::fs;

    fn fork(space: &Space, task: i64) -> Attempt {
        space
            .fork(&ForkRequest {
                actor: "agent:test".into(),
                task_id: Some(task),
                backend: crate::view::BackendChoice::Copy,
                ..Default::default()
            })
            .unwrap()
            .attempt
    }

    #[test]
    fn open_attempts_of_different_tasks_overlap_on_shared_files() {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        fs::write(w.join("list.txt"), "alpha\n").unwrap();
        fs::write(w.join("one.txt"), "1\n").unwrap();
        fs::write(w.join("lib.rs"), "fn f() -> u32 { 1 }\n").unwrap();
        let space = Space::init(w).unwrap();
        let t1 = space.create_task("one", None).unwrap().id;
        let t2 = space.create_task("two", None).unwrap().id;
        let a = fork(&space, t1);
        let b = fork(&space, t2);
        let sib = fork(&space, t1);
        let write = |at: &Attempt, f: &str, t: &str| {
            fs::write(at.view_path.join(f), t).unwrap();
            space.snapshot(at.id, "manual", None).unwrap();
        };
        write(&a, "list.txt", "alpha\nbeta\n");
        write(&b, "list.txt", "alpha\ngamma\n");
        write(&a, "lib.rs", "fn f() -> u32 { 2 }\n");
        write(&b, "lib.rs", "fn f() -> u32 { 3 }\n");
        write(&b, "one.txt", "2\n");
        write(&sib, "list.txt", "zeta\n");

        let o = space.overlaps(Some(a.id)).unwrap();
        assert_eq!(o.len(), 1, "{o:?}");
        assert_eq!((o[0].attempt, o[0].other), (a.id, b.id));
        assert_eq!(o[0].files, ["lib.rs", "list.txt"]);
        // Both added after alpha: auto-resolves. Same line twice: conflicts.
        assert_eq!(o[0].conflicting, ["lib.rs"]);
        assert_eq!(space.overlaps(None).unwrap().len(), 2); // a-b, b-sib

        let n = space.overlap_notice(a.id, "lib.rs").unwrap().unwrap();
        assert!(n.contains(&format!("a{} (agent:test, task t{t2}) also changes lib.rs and the two versions already conflict", b.id)), "{n}");
        // Only once per file and other attempt.
        assert_eq!(space.overlap_notice(a.id, "lib.rs").unwrap(), None);
        assert_eq!(space.overlap_notice(a.id, "one.txt").unwrap(), None);

        space
            .set_status(b.id, crate::space::Status::Abandoned, Some("done"))
            .unwrap();
        assert!(space.overlaps(Some(a.id)).unwrap().is_empty());
    }
}
