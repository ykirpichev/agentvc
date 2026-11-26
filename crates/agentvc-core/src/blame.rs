//! Blame lines to attempts (#73): `agentvc blame <path> [aN]`.
//!
//! git blame stops at the squashed PR commit; this goes below it. The file's
//! history is the attempt's lineage: the snapshots of every attempt it was
//! forked from (up to the fork point), then its own, oldest first, then its
//! unsnapshotted working state. Each line is attributed to the snapshot that
//! last changed it, with that attempt's actor, model and trigger. Lines that
//! were already in the base commit, or that a `rebase` brought in from main,
//! fall through to `git blame` of that commit.

use std::collections::HashMap;
use std::process::{Command, Stdio};

use serde::Serialize;
use similar::{DiffOp, TextDiff};

use crate::error::{Error, Result};
use crate::export::HostRepo;
use crate::objects::{Mode, ObjectStore, Oid};
use crate::space::{Space, ROOT_ATTEMPT, SPACE_DIR};

/// Where a line came from.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// Written in an agentvc snapshot.
    Snapshot {
        snapshot: i64,
        attempt: i64,
        actor: String,
        model: Option<String>,
        trigger: String,
        created_at: i64,
        /// For a `take` or `merge` snapshot: the attempt the line came from.
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// Already in a git commit (the base, or what a rebase brought in).
    Git {
        commit: String,
        author: String,
        time: i64,
        summary: String,
    },
    /// In the view, not yet snapshotted.
    Working { attempt: i64 },
}

#[derive(Clone, Debug, Serialize)]
pub struct BlameLine {
    /// 1-based.
    pub line: usize,
    pub text: String,
    pub origin: Origin,
}

#[derive(Clone, Debug, Serialize)]
pub struct Blame {
    pub attempt: i64,
    pub path: String,
    /// The snapshot blamed (`None` with the working state).
    pub snapshot: Option<i64>,
    pub working: bool,
    pub lines: Vec<BlameLine>,
}

/// Working origin while walking: git lines are resolved at the end.
#[derive(Clone, Debug)]
enum Pending {
    Git { commit: String, line: usize },
    Snap(i64),
    Working,
}

/// The blob at `path` in `tree`, walking only the directories on the path.
pub fn blob_at(store: &ObjectStore, tree: &Oid, path: &str) -> Result<Option<(Mode, Oid)>> {
    let mut cur = *tree;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        let Some(e) = store.read_tree(&cur)?.into_iter().find(|e| e.name == *part) else {
            return Ok(None);
        };
        let last = i + 1 == parts.len();
        match (last, e.mode) {
            (true, Mode::Tree) => return Ok(None),
            (true, m) => return Ok(Some((m, e.oid))),
            (false, Mode::Tree) => cur = e.oid,
            (false, _) => return Ok(None),
        }
    }
    Ok(None)
}

fn lines_of(path: &str, data: &[u8]) -> Result<Vec<String>> {
    let text = std::str::from_utf8(data)
        .ok()
        .filter(|t| !t.as_bytes()[..t.len().min(8000)].contains(&0))
        .ok_or_else(|| Error::BadQuery(format!("{path} is binary; blame needs text")))?;
    Ok(text.lines().map(String::from).collect())
}

/// Carry origins from `old` lines to `new` lines; new or rewritten lines get
/// `fresh(new_index)`.
fn carry(
    old: &[String],
    old_origins: &[Pending],
    new: &[String],
    mut fresh: impl FnMut(usize) -> Pending,
) -> Vec<Pending> {
    let o: Vec<&str> = old.iter().map(String::as_str).collect();
    let n: Vec<&str> = new.iter().map(String::as_str).collect();
    let diff = TextDiff::configure()
        .algorithm(similar::Algorithm::Patience)
        .diff_slices(&o, &n);
    let mut out = Vec::with_capacity(new.len());
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { old_index, len, .. } => {
                out.extend(old_origins[old_index..old_index + len].iter().cloned())
            }
            DiffOp::Delete { .. } => {}
            DiffOp::Insert {
                new_index, new_len, ..
            }
            | DiffOp::Replace {
                new_index, new_len, ..
            } => out.extend((new_index..new_index + new_len).map(&mut fresh)),
        }
    }
    out
}

/// For each line of `text`, its index in the same file at `commit`, where
/// the line is unchanged.
fn map_to_commit(repo: &HostRepo, commit: &str, path: &str, text: &[String]) -> Vec<Option<usize>> {
    let committed: Vec<String> = repo
        .git(&["show", &format!("{commit}:{path}")], None)
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default();
    let mut out = vec![None; text.len()];
    let o: Vec<&str> = committed.iter().map(String::as_str).collect();
    let n: Vec<&str> = text.iter().map(String::as_str).collect();
    for op in TextDiff::configure()
        .algorithm(similar::Algorithm::Patience)
        .diff_slices(&o, &n)
        .ops()
    {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = *op
        {
            for k in 0..len {
                out[new_index + k] = Some(old_index + k);
            }
        }
    }
    out
}

/// `git blame --line-porcelain` of `path` at `commit`: per line, its origin.
fn git_blame(repo: &HostRepo, commit: &str, path: &str) -> Vec<Origin> {
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo.workdir)
        .args(["blame", "--line-porcelain", commit, "--", path])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Some(out) = out.ok().filter(|o| o.status.success()) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = Vec::new();
    let (mut sha, mut author, mut time, mut summary) =
        (String::new(), String::new(), 0, String::new());
    let mut header = true;
    for l in text.lines() {
        if header {
            sha = l.split(' ').next().unwrap_or_default().to_string();
            header = false;
        } else if let Some(v) = l.strip_prefix("author ") {
            author = v.to_string();
        } else if let Some(v) = l.strip_prefix("author-time ") {
            time = v.parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("summary ") {
            summary = v.to_string();
        } else if l.starts_with('\t') {
            lines.push(Origin::Git {
                commit: sha.clone(),
                author: author.clone(),
                time,
                summary: summary.clone(),
            });
            header = true;
        }
    }
    lines
}

impl Space {
    /// Blame `path` in attempt `attempt_id`: its head snapshot, or with
    /// `working` its view as it is now.
    pub fn blame(&self, attempt_id: i64, path: &str, working: bool) -> Result<Blame> {
        let path = path.trim_start_matches("./").to_string();
        let target = self.attempt(attempt_id)?;
        // The lineage, oldest attempt first, each with the last snapshot
        // that counts (the next attempt's fork point).
        let mut chain = vec![(target.clone(), None::<i64>)];
        let mut cur = target.clone();
        while let (Some(p), Some(ps)) = (cur.parent_attempt, cur.parent_snapshot) {
            if p == ROOT_ATTEMPT || chain.len() > 100_000 {
                break;
            }
            let parent = self.attempt(p)?;
            chain.push((parent.clone(), Some(ps)));
            cur = parent;
        }
        chain.reverse();
        let first = &chain[0].0;
        let start = match first.parent_snapshot {
            Some(s) => self.snapshot_by_id(s)?,
            None => self.head_snapshot(first.id)?,
        };
        let repo = HostRepo::open(self.workdir()).ok();
        let store = self.objects();

        let read = |tree: &str| -> Result<Option<Vec<String>>> {
            match blob_at(store, &Oid::from_hex(tree)?, &path)? {
                Some((_, oid)) => Ok(Some(lines_of(&path, &store.read_blob(&oid)?)?)),
                None => Ok(None),
            }
        };
        let to_git = |commit: Option<String>, text: &[String], fallback: Pending| -> Vec<Pending> {
            match (&repo, commit) {
                (Some(r), Some(c)) => map_to_commit(r, &c, &path, text)
                    .into_iter()
                    .map(|m| match m {
                        Some(line) => Pending::Git {
                            commit: c.clone(),
                            line,
                        },
                        None => fallback.clone(),
                    })
                    .collect(),
                _ => vec![fallback; text.len()],
            }
        };

        // Version 0: the content the lineage started from.
        let mut text = read(&start.tree)?.unwrap_or_default();
        let mut origins = to_git(
            self.git_base_commit(&start)?,
            &text,
            Pending::Snap(start.id),
        );
        let mut last_tree_blob: Option<Option<(Mode, Oid)>> =
            Some(blob_at(store, &Oid::from_hex(&start.tree)?, &path)?);
        let mut snaps = HashMap::new();
        let mut head = None;
        for (a, upto) in &chain {
            for s in self.snapshots(a.id)? {
                if upto.is_some_and(|u| s.id > u) {
                    break;
                }
                head = Some(s.id);
                let blob = blob_at(store, &Oid::from_hex(&s.tree)?, &path)?;
                let rebase = s.trigger == "rebase";
                if last_tree_blob.as_ref() == Some(&blob) && !rebase {
                    continue;
                }
                last_tree_blob = Some(blob);
                let new = read(&s.tree)?.unwrap_or_default();
                let next = if rebase {
                    // Lines from the new main go to git; merged lines to the
                    // rebase snapshot. Lines the lineage did not write itself
                    // (older git lines, or the starting snapshot's) and that
                    // are in the new main are re-blamed there too, so a stale
                    // base commit does not leave them on the root attempt.
                    let onto = s
                        .meta
                        .as_ref()
                        .and_then(|m| m["onto"].as_str())
                        .map(String::from);
                    let via_git = to_git(onto, &new, Pending::Snap(s.id));
                    let mut next = carry(&text, &origins, &new, |i| via_git[i].clone());
                    for (o, g) in next.iter_mut().zip(&via_git) {
                        let inherited = matches!(o, Pending::Git { .. })
                            || matches!(o, Pending::Snap(id) if *id == start.id);
                        if inherited && matches!(g, Pending::Git { .. }) {
                            *o = g.clone();
                        }
                    }
                    next
                } else {
                    carry(&text, &origins, &new, |_| Pending::Snap(s.id))
                };
                snaps.insert(s.id, (a.clone(), s));
                text = new;
                origins = next;
            }
        }
        let mut blamed_working = false;
        if working && target.status.is_open() && self.ensure_view(&target)? {
            let index = self
                .workdir()
                .join(SPACE_DIR)
                .join("index")
                .join(format!("a{}", target.id));
            let tree = crate::snapshot::capture_indexed(store, &target.view_path, Some(&index))?;
            let blob = blob_at(store, &tree, &path)?;
            if last_tree_blob.as_ref() != Some(&blob) {
                let new = match blob {
                    Some((_, oid)) => lines_of(&path, &store.read_blob(&oid)?)?,
                    None => Vec::new(),
                };
                origins = carry(&text, &origins, &new, |_| Pending::Working);
                text = new;
                blamed_working = true;
            }
            last_tree_blob = Some(blob);
        }
        if last_tree_blob.is_some_and(|b| b.is_none()) {
            return Err(Error::NotFound {
                kind: "file",
                id: format!("{path} in a{attempt_id}"),
            });
        }

        // Resolve git lines, one `git blame` per commit.
        let mut blames: HashMap<String, Vec<Origin>> = HashMap::new();
        let mut lines = Vec::with_capacity(text.len());
        for (i, (t, o)) in text.into_iter().zip(origins).enumerate() {
            let origin = match o {
                Pending::Working => Origin::Working {
                    attempt: attempt_id,
                },
                Pending::Git { commit, line } => {
                    let b = blames.entry(commit.clone()).or_insert_with(|| {
                        repo.as_ref()
                            .map(|r| git_blame(r, &commit, &path))
                            .unwrap_or_default()
                    });
                    b.get(line).cloned().unwrap_or(Origin::Git {
                        commit,
                        author: String::new(),
                        time: 0,
                        summary: String::new(),
                    })
                }
                Pending::Snap(id) => {
                    let (a, s) = match snaps.get(&id) {
                        Some((a, s)) => (a.clone(), s.clone()),
                        None => {
                            let s = self.snapshot_by_id(id)?;
                            (self.attempt(s.attempt_id)?, s)
                        }
                    };
                    let source = matches!(s.trigger.as_str(), "take" | "merge")
                        .then(|| {
                            s.meta
                                .as_ref()
                                .and_then(|m| m["source"].as_str().map(String::from))
                        })
                        .flatten();
                    Origin::Snapshot {
                        snapshot: s.id,
                        attempt: a.id,
                        actor: a.actor.clone(),
                        model: a.model.clone(),
                        trigger: s.trigger.clone(),
                        created_at: s.created_at,
                        source,
                    }
                }
            };
            lines.push(BlameLine {
                line: i + 1,
                text: t,
                origin,
            });
        }
        Ok(Blame {
            attempt: attempt_id,
            path,
            snapshot: if blamed_working {
                None
            } else {
                head.or(Some(start.id))
            },
            working: blamed_working,
            lines,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{ForkRequest, Ref, Status};
    use crate::view::BackendChoice;
    use std::fs;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Ana")
            .env("GIT_AUTHOR_EMAIL", "a@a")
            .env("GIT_COMMITTER_NAME", "Ana")
            .env("GIT_COMMITTER_EMAIL", "a@a")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn who(o: &Origin) -> String {
        match o {
            Origin::Snapshot {
                attempt, trigger, ..
            } => format!("a{attempt}:{trigger}"),
            Origin::Git { summary, .. } => format!("git:{summary}"),
            Origin::Working { .. } => "working".into(),
        }
    }

    #[test]
    fn three_forks_each_own_their_lines() {
        let tmp = crate::testutil::tempdir();
        let w = tmp.path();
        git(w, &["init", "-q", "-b", "main"]);
        fs::write(w.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        git(w, &["add", "."]);
        git(w, &["commit", "-q", "-m", "base commit"]);
        let space = Space::init(w).unwrap();
        let task = space.create_task("t", None).unwrap();
        let fork = |from: i64, actor: &str| {
            space
                .fork(&ForkRequest {
                    task_id: Some(task.id),
                    from: Some(Ref::Attempt(from)),
                    actor: actor.into(),
                    backend: BackendChoice::Copy,
                    ..Default::default()
                })
                .unwrap()
                .attempt
        };
        let edit = |a: &crate::Attempt, from: &str, to: &str| {
            let p = a.view_path.join("f.txt");
            let t = fs::read_to_string(&p).unwrap().replace(from, to);
            fs::write(&p, t).unwrap();
            space.snapshot(a.id, "tool:Edit", None).unwrap().snapshot.id
        };
        let a = fork(ROOT_ATTEMPT, "agent:one");
        let s_a = edit(&a, "two\n", "TWO\n");
        let b = fork(a.id, "agent:two");
        let s_b = edit(&b, "three\n", "THREE\n");
        let c = fork(b.id, "agent:three");
        let s_c = edit(&c, "four\n", "four\nfive\n");
        // a keeps going after b forked: that edit is not in c's history.
        edit(&a, "one\n", "ONE\n");
        // The working state of an open attempt.
        fs::write(a.view_path.join("f.txt"), "ONE\nTWO\nnew\nthree\nfour\n").unwrap();
        let bl = space.blame(a.id, "f.txt", true).unwrap();
        assert!(bl.working);
        assert_eq!(who(&bl.lines[2].origin), "working");
        assert_eq!(who(&bl.lines[0].origin), format!("a{}:tool:Edit", a.id));

        space.set_status(c.id, Status::Promoted, None).unwrap();

        let bl = space.blame(c.id, "f.txt", false).unwrap();
        let got: Vec<(String, String)> = bl
            .lines
            .iter()
            .map(|l| (l.text.clone(), who(&l.origin)))
            .collect();
        assert_eq!(
            got,
            [
                ("one".into(), "git:base commit".into()),
                ("TWO".into(), format!("a{}:tool:Edit", a.id)),
                ("THREE".into(), format!("a{}:tool:Edit", b.id)),
                ("four".into(), "git:base commit".into()),
                ("five".into(), format!("a{}:tool:Edit", c.id)),
            ]
        );
        let snap = |i: usize| match &bl.lines[i].origin {
            Origin::Snapshot {
                snapshot, actor, ..
            } => (*snapshot, actor.clone()),
            o => panic!("{o:?}"),
        };
        assert_eq!(snap(1), (s_a, "agent:one".into()));
        assert_eq!(snap(2), (s_b, "agent:two".into()));
        assert_eq!(snap(4), (s_c, "agent:three".into()));
        match &bl.lines[0].origin {
            Origin::Git { author, .. } => assert_eq!(author, "Ana"),
            o => panic!("{o:?}"),
        }

        assert_eq!(
            space.blame(c.id, "nope.txt", false).unwrap_err().code(),
            "not_found"
        );
    }
}
