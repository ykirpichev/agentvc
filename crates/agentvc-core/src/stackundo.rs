//! `agentvc stack undo` (#128): every changing stack command records, in
//! the worktree's own git dir, the stack and the branch tips before and
//! after it ran; undo puts the before back. Per worktree, like the stack.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, IoContext, Result};
use crate::stack::{State, Window};

/// Entries kept per worktree.
const KEEP: usize = 50;

/// The stack and the refs it covers at one moment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capture {
    pub state: State,
    /// Tracked branch -> tip (`None`: the branch does not exist).
    pub tips: BTreeMap<String, Option<String>>,
    /// The checked-out branch, or the detached commit as `@<sha>`.
    pub head: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub command: String,
    pub at: i64,
    pub before: Capture,
    pub after: Capture,
    /// Reversed by a later undo.
    #[serde(default)]
    pub undone: bool,
    /// This entry is an undo of entry N (undo it to redo N).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undoes: Option<u64>,
}

/// One row of `stack undo --list`.
#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub id: u64,
    pub command: String,
    pub at: i64,
    pub undone: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undoes: Option<u64>,
    /// Branches whose tip the command changed.
    pub branches: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Undone {
    /// The entry that was reversed.
    pub undid: u64,
    pub command: String,
    /// The new entry recording this undo (undo it to redo).
    pub entry: u64,
    pub branches: Vec<String>,
    pub head: Option<String>,
}

fn err(code: &'static str, message: impl Into<String>, details: serde_json::Value) -> Error {
    Error::Stack {
        code,
        message: message.into(),
        details: Some(details),
    }
}

fn log_path(git_dir: &Path) -> PathBuf {
    git_dir.join("agentvc").join("stack-undo.json")
}

fn read_log(git_dir: &Path) -> Vec<Entry> {
    fs::read_to_string(log_path(git_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_log(git_dir: &Path, log: &[Entry]) -> Result<()> {
    let path = log_path(git_dir);
    let dir = path.parent().expect("has a parent");
    fs::create_dir_all(dir).at(dir)?;
    let tmp = dir.join(format!("stack-undo.json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_string(log).expect("serializes")).at(&tmp)?;
    fs::rename(&tmp, &path).at(&path)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn capture(w: &Window) -> Capture {
    let tips = w
        .state
        .branches
        .iter()
        .map(|b| (b.name.clone(), w.branch_tip(&b.name)))
        .collect();
    let head = w
        .current_branch()
        .or_else(|| w.repo.rev("HEAD").map(|h| format!("@{h}")));
    Capture {
        state: w.state.clone(),
        tips,
        head,
    }
}

fn changed(before: &Capture, after: &Capture) -> Vec<String> {
    let names: BTreeSet<&String> = before.tips.keys().chain(after.tips.keys()).collect();
    names
        .into_iter()
        .filter(|n| before.tips.get(*n).cloned().flatten() != after.tips.get(*n).cloned().flatten())
        .cloned()
        .collect()
}

fn push(
    git_dir: &Path,
    command: &str,
    before: Capture,
    after: Capture,
    undoes: Option<u64>,
) -> Result<u64> {
    let mut log = read_log(git_dir);
    let id = log.last().map(|e| e.id + 1).unwrap_or(1);
    log.push(Entry {
        id,
        command: command.to_string(),
        at: now(),
        before,
        after,
        undone: false,
        undoes,
    });
    if log.len() > KEEP {
        let cut = log.len() - KEEP;
        log.drain(..cut);
    }
    write_log(git_dir, &log)?;
    Ok(id)
}

/// What `finish` compares against: the worktree's stack before a command.
pub struct Begun {
    git_dir: PathBuf,
    before: Capture,
}

/// Capture the worktree at `cwd` before a changing stack command.
pub fn begin(cwd: &Path) -> Option<Begun> {
    Window::open(cwd).ok().map(|w| Begun {
        git_dir: w.git_dir.clone(),
        before: capture(&w),
    })
}

/// Record `command` for undo when it changed the stack, a branch or what
/// is checked out since `begin`. Called whether or not the command failed
/// (a restack that stopped on a conflict has still changed things).
pub fn finish(cwd: &Path, command: &str, begun: Option<Begun>) {
    let Some(b) = begun else { return };
    if let Ok(w) = Window::open(cwd) {
        let after = capture(&w);
        if after != b.before {
            let _ = push(&b.git_dir, command, b.before, after, None);
        }
    }
}

/// Run `f` and record it (see `begin` and `finish`).
pub fn recorded<T>(cwd: &Path, command: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let b = begin(cwd);
    let out = f();
    finish(cwd, command, b);
    out
}

pub fn list(w: &Window) -> Vec<Row> {
    read_log(&w.git_dir)
        .into_iter()
        .rev()
        .map(|e| Row {
            branches: changed(&e.before, &e.after),
            id: e.id,
            command: e.command,
            at: e.at,
            undone: e.undone,
            undoes: e.undoes,
        })
        .collect()
}

/// Reverse entry `id`, or the newest command not undone yet (so repeated
/// undo walks back). Undoing an undo entry redoes what it undid.
pub fn undo(w: &mut Window, id: Option<u64>) -> Result<Undone> {
    let mut log = read_log(&w.git_dir);
    let pos = match id {
        Some(id) => log.iter().position(|e| e.id == id).ok_or_else(|| {
            err(
                "no_undo",
                format!("no stack command {id} in this window's undo log"),
                json!({ "id": id }),
            )
        })?,
        None => log
            .iter()
            .rposition(|e| !e.undone && e.undoes.is_none())
            .ok_or_else(|| err("no_undo", "nothing to undo in this window", json!({})))?,
    };
    let e = log[pos].clone();
    if e.undone {
        return Err(err(
            "already_undone",
            format!("stack command {} was already undone", e.id),
            json!({ "id": e.id }),
        ));
    }
    if w.rebase_in_progress() {
        return Err(err(
            "rebase_in_progress",
            "a git rebase is in progress here; finish or abort it first",
            json!({}),
        ));
    }
    // Only when nothing moved since: else undo would throw away later work.
    let now_cap = capture(w);
    let mut moved = Vec::new();
    for (name, tip) in &e.after.tips {
        if w.branch_tip(name) != *tip {
            moved.push(name.clone());
        }
    }
    if !moved.is_empty() {
        return Err(err(
            "undo_conflict",
            format!(
                "{} changed since `{}`; nothing was undone",
                moved.join(", "),
                e.command
            ),
            json!({ "id": e.id, "branches": moved }),
        ));
    }
    let names: BTreeSet<String> = e
        .before
        .tips
        .keys()
        .chain(e.after.tips.keys())
        .cloned()
        .collect();
    let cur = w.current_branch();
    let mut tx = String::new();
    for n in &names {
        if Some(n) == cur.as_ref() {
            continue;
        }
        let want = e.before.tips.get(n).cloned().flatten();
        let have = w.branch_tip(n);
        match (&want, &have) {
            (Some(t), _) if Some(t) != have.as_ref() => {
                tx += &format!("update refs/heads/{n} {t}\n")
            }
            (None, Some(h)) => tx += &format!("delete refs/heads/{n} {h}\n"),
            _ => {}
        }
    }
    if !tx.is_empty() {
        w.repo.git(
            &["update-ref", "-m", "agentvc stack undo", "--stdin"],
            Some(&tx),
        )?;
    }
    let target_head = e.before.head.clone();
    if let Some(c) = &cur {
        if names.contains(c) {
            match e.before.tips.get(c).cloned().flatten() {
                Some(t) => {
                    if w.branch_tip(c).as_deref() != Some(t.as_str()) {
                        w.repo.git(&["reset", "--keep", &t], None)?;
                    }
                }
                None => {
                    // Created by the command: leave it, then delete it.
                    match target_head.as_deref() {
                        Some(h) if h.starts_with('@') => w
                            .repo
                            .git(&["switch", "--quiet", "--detach", &h[1..]], None)?,
                        Some(h) => w.repo.git(&["switch", "--quiet", h], None)?,
                        None => String::new(),
                    };
                    w.repo.git(&["branch", "-D", c], None)?;
                }
            }
        }
    }
    let head_now = w.current_branch();
    match target_head.as_deref() {
        Some(h) if h.starts_with('@') => {
            if head_now.is_some() {
                w.repo
                    .git(&["switch", "--quiet", "--detach", &h[1..]], None)?;
            }
        }
        Some(h) if head_now.as_deref() != Some(h) && w.branch_tip(h).is_some() => {
            w.repo.git(&["switch", "--quiet", h], None)?;
        }
        _ => {}
    }
    w.state = e.before.state.clone();
    w.save()?;
    let after = capture(w);
    log[pos].undone = true;
    if let Some(orig) = e.undoes {
        // Redo: the original counts as done again.
        if let Some(o) = log.iter_mut().find(|x| x.id == orig) {
            o.undone = false;
        }
    }
    write_log(&w.git_dir, &log)?;
    let entry = push(
        &w.git_dir,
        &format!("undo {}", e.id),
        now_cap.clone(),
        after.clone(),
        Some(e.id),
    )?;
    Ok(Undone {
        undid: e.id,
        command: e.command,
        entry,
        branches: changed(&now_cap, &after),
        head: after.head,
    })
}
