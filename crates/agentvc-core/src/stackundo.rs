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
    /// Branches the command moved between this window and another (`give`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<Transfer>,
    /// The worktree had changes when the command ran, which it may have
    /// committed (`create -a`, `modify`): undo then moves HEAD without
    /// touching the index or the files, like `git reset --soft`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_worktree: bool,
}

/// Branches a command handed to (`outgoing`) or took back from another
/// window, whose stack undo has to change too.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Transfer {
    /// The other window's git dir.
    pub window: PathBuf,
    pub branches: Vec<String>,
    pub outgoing: bool,
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

/// What an entry carries besides its captures.
#[derive(Default)]
struct Extra {
    undoes: Option<u64>,
    transfer: Option<Transfer>,
    from_worktree: bool,
}

fn push(git_dir: &Path, command: &str, before: Capture, after: Capture, x: Extra) -> Result<u64> {
    let mut log = read_log(git_dir);
    let id = log.last().map(|e| e.id + 1).unwrap_or(1);
    log.push(Entry {
        id,
        command: command.to_string(),
        at: now(),
        before,
        after,
        undone: false,
        undoes: x.undoes,
        transfer: x.transfer,
        from_worktree: x.from_worktree,
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
    /// The worktree had uncommitted changes.
    dirty: bool,
    /// Every local branch and its tip.
    refs: BTreeMap<String, String>,
}

fn local_refs(w: &Window) -> BTreeMap<String, String> {
    w.repo
        .git(
            &[
                "for-each-ref",
                "--format=%(refname:lstrip=2) %(objectname)",
                "refs/heads",
            ],
            None,
        )
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.rsplit_once(' '))
        .map(|(n, t)| (n.to_string(), t.to_string()))
        .collect()
}

/// `before` with the tips, then, of branches the stack covers only
/// `after` (created or adopted): undo has to know which ones existed.
fn widen(mut before: Capture, after: &Capture, refs: &BTreeMap<String, String>) -> Capture {
    for n in after.tips.keys() {
        if !before.tips.contains_key(n) {
            before.tips.insert(n.clone(), refs.get(n).cloned());
        }
    }
    before
}

/// Capture the worktree at `cwd` before a changing stack command.
pub fn begin(cwd: &Path) -> Option<Begun> {
    Window::open(cwd).ok().map(|w| Begun {
        git_dir: w.git_dir.clone(),
        before: capture(&w),
        dirty: w
            .repo
            .git(&["status", "--porcelain"], None)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(true),
        refs: local_refs(&w),
    })
}

/// Record `command` for undo when it changed the stack, a branch or what
/// is checked out since `begin`. Called whether or not the command failed
/// (a restack that stopped on a conflict has still changed things).
pub fn finish(cwd: &Path, command: &str, begun: Option<Begun>) {
    finish_with(cwd, command, begun, None)
}

/// `finish` for `give`: `transfer` names the branches handed over, so
/// undo can take them back.
pub fn finish_with(cwd: &Path, command: &str, begun: Option<Begun>, transfer: Option<Transfer>) {
    let Some(b) = begun else { return };
    if let Ok(w) = Window::open(cwd) {
        let after = capture(&w);
        if after != b.before {
            let x = Extra {
                transfer,
                from_worktree: b.dirty,
                ..Default::default()
            };
            let before = widen(b.before, &after, &b.refs);
            let _ = push(&b.git_dir, command, before, after, x);
        }
    }
}

/// Record `stack continue` / `stack abort` (`command`). When the newest
/// entry is the command that stopped on the conflict and nothing changed
/// since, the outcome is folded into that entry, so one undo reverses the
/// whole restack; otherwise it is an entry of its own.
pub fn finish_resume(cwd: &Path, command: &str, begun: Option<Begun>) {
    let Some(b) = begun else { return };
    let Ok(w) = Window::open(cwd) else { return };
    let after = capture(&w);
    if after == b.before {
        return;
    }
    let mut log = read_log(&b.git_dir);
    if let Some(last) = log.last_mut() {
        if !last.undone
            && last.undoes.is_none()
            && last.after.state.pending.is_some()
            && last.after.state.pending == b.before.state.pending
            && last.after.tips == b.before.tips
        {
            last.after = after;
            last.command = format!("{} + {command}", last.command);
            let _ = write_log(&b.git_dir, &log);
            return;
        }
    }
    let x = Extra {
        from_worktree: b.dirty,
        ..Default::default()
    };
    let before = widen(b.before, &after, &b.refs);
    let _ = push(&b.git_dir, command, before, after, x);
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

fn names(s: &State) -> BTreeSet<String> {
    s.branches.iter().map(|b| b.name.clone()).collect()
}

/// Reverse entry `id`, or the newest command not undone yet (so repeated
/// undo walks back). Undoing an undo entry redoes what it undid.
///
/// Refused, changing nothing, when a later command still stands
/// (`undo_not_newest`), when a branch it touched moved or left this window
/// since (`undo_conflict`), or when a branch it would change belongs to or
/// is checked out in another window (`undo_owned_elsewhere`).
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
    // Only the newest command that still stands: undoing an older one would
    // also throw away what the later ones did. An undo of a command newer
    // than `e` (itself undone) is not in the way: the two cancel out.
    let later: Vec<&Entry> = log[pos + 1..]
        .iter()
        .filter(|x| !x.undone && x.undoes.is_none_or(|t| t < e.id))
        .collect();
    if !later.is_empty() {
        let first: Vec<String> = later
            .iter()
            .rev()
            .map(|x| format!("{} `{}`", x.id, x.command))
            .collect();
        return Err(err(
            "undo_not_newest",
            format!(
                "stack command {} `{}` is not the newest; undo {} first; nothing was undone",
                e.id,
                e.command,
                first.join(", then ")
            ),
            json!({
                "id": e.id,
                "later": later
                    .iter()
                    .rev()
                    .map(|x| json!({ "id": x.id, "command": x.command }))
                    .collect::<Vec<_>>(),
            }),
        ));
    }
    if w.rebase_in_progress() {
        return Err(err(
            "rebase_in_progress",
            "a git rebase is in progress here; finish or abort it first",
            json!({}),
        ));
    }
    let outgoing = e.transfer.as_ref().filter(|t| t.outgoing);
    let incoming = e.transfer.as_ref().filter(|t| !t.outgoing);
    // Only when nothing moved since: else undo would throw away later work.
    let now_cap = capture(w);
    let mut moved = Vec::new();
    for (name, tip) in &e.after.tips {
        if w.branch_tip(name) != *tip {
            moved.push(name.clone());
        }
    }
    for n in outgoing.iter().flat_map(|t| &t.branches) {
        if w.branch_tip(n) != e.before.tips.get(n).cloned().flatten() {
            moved.push(n.clone());
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
    // A branch the command left in this window that is gone now (given
    // away, untracked) cannot be put back from here.
    let after_names = names(&e.after.state);
    let now_names = names(&w.state);
    if let Some(n) = after_names.difference(&now_names).next() {
        if let Some(p) = w.owner_elsewhere(n) {
            return Err(err(
                "undo_owned_elsewhere",
                format!(
                    "{n} belongs to the window at {} now; nothing was undone",
                    p.display()
                ),
                json!({ "id": e.id, "branch": n, "window": p }),
            ));
        }
        return Err(err(
            "undo_conflict",
            format!(
                "{n} is no longer in this window's stack since `{}`; nothing was undone",
                e.command
            ),
            json!({ "id": e.id, "branches": [n] }),
        ));
    }
    // The branches the undo changes: refs it moves, creates or deletes,
    // and branches it starts or stops tracking.
    let before_names = names(&e.before.state);
    let all: BTreeSet<String> = e
        .before
        .tips
        .keys()
        .chain(e.after.tips.keys())
        .cloned()
        .collect();
    let mut touched: BTreeSet<String> = all
        .iter()
        .filter(|n| e.before.tips.get(*n).cloned().flatten() != w.branch_tip(n))
        .cloned()
        .collect();
    touched.extend(before_names.symmetric_difference(&now_names).cloned());
    let elsewhere = w.checkouts_elsewhere();
    let taking_back: BTreeSet<&String> = outgoing.iter().flat_map(|t| &t.branches).collect();
    for n in &touched {
        if let Some(p) = elsewhere.get(n) {
            return Err(err(
                "undo_owned_elsewhere",
                format!(
                    "{n} is checked out in the worktree at {}; nothing was undone",
                    p.display()
                ),
                json!({ "id": e.id, "branch": n, "worktree": p }),
            ));
        }
        if taking_back.contains(n) {
            continue;
        }
        if let Some(p) = w.owner_elsewhere(n) {
            return Err(err(
                "undo_owned_elsewhere",
                format!(
                    "{n} belongs to the window at {}; nothing was undone",
                    p.display()
                ),
                json!({ "id": e.id, "branch": n, "window": p }),
            ));
        }
    }
    // Undoing a give takes the branches back: only while the other window
    // still has them as given and stacked nothing of its own on them.
    let mut theirs = None;
    if let Some(t) = e.transfer.as_ref() {
        let there = t.window.parent().unwrap_or(&t.window).to_path_buf();
        let s = crate::stack::read_state(&t.window)?;
        if let Some(t) = outgoing {
            let s = s.clone().unwrap_or_else(|| State {
                branches: Vec::new(),
                ..w.state.clone()
            });
            let theirs_names = names(&s);
            for n in &t.branches {
                if !theirs_names.contains(n) {
                    return Err(err(
                        "undo_conflict",
                        format!(
                            "{n} is no longer in the stack of the window it was given to; nothing was undone"
                        ),
                        json!({ "id": e.id, "branches": [n], "window": there }),
                    ));
                }
            }
            if let Some(b) = s
                .branches
                .iter()
                .find(|b| !t.branches.contains(&b.name) && t.branches.contains(&b.parent))
            {
                return Err(err(
                    "undo_owned_elsewhere",
                    format!(
                        "{} in the window at {} is stacked on {}; nothing was undone",
                        b.name,
                        there.display(),
                        b.parent
                    ),
                    json!({ "id": e.id, "branch": b.name, "window": there }),
                ));
            }
        }
        if incoming.is_some() && !t.window.is_dir() {
            return Err(err(
                "undo_conflict",
                format!(
                    "the window at {} is gone; nothing was undone",
                    there.display()
                ),
                json!({ "id": e.id, "window": there }),
            ));
        }
        theirs = Some(s);
    }
    // Branches that joined this window outside the log (given to it) stay,
    // unless they sit on a branch the undo changes.
    let extras: Vec<_> = w
        .state
        .branches
        .iter()
        .filter(|b| !after_names.contains(&b.name) && !before_names.contains(&b.name))
        .cloned()
        .collect();
    if let Some(b) = extras.iter().find(|b| touched.contains(&b.parent)) {
        return Err(err(
            "undo_conflict",
            format!(
                "{} joined this window since `{}` and is stacked on {}; nothing was undone",
                b.name, e.command, b.parent
            ),
            json!({ "id": e.id, "branches": [b.name] }),
        ));
    }

    let refs = local_refs(w);
    let cur = w.current_branch();
    let soft = e.from_worktree;
    let mut tx = String::new();
    for n in &all {
        let want = e.before.tips.get(n).cloned().flatten();
        let have = w.branch_tip(n);
        if Some(n) == cur.as_ref() && !(soft && want.is_some()) {
            continue;
        }
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
    if soft {
        // The command committed changes from the worktree: move HEAD only,
        // so they come back as staged changes and nothing is lost.
        match target_head.as_deref() {
            Some(h) if h.starts_with('@') => {
                if now_cap.head.as_deref() != Some(h) {
                    w.repo.git(
                        &[
                            "update-ref",
                            "--no-deref",
                            "-m",
                            "agentvc stack undo",
                            "HEAD",
                            &h[1..],
                        ],
                        None,
                    )?;
                }
            }
            Some(h) if cur.as_deref() != Some(h) && w.branch_tip(h).is_some() => {
                w.repo
                    .git(&["symbolic-ref", "HEAD", &format!("refs/heads/{h}")], None)?;
            }
            _ => {}
        }
        if let Some(c) = &cur {
            let created = all.contains(c) && e.before.tips.get(c).cloned().flatten().is_none();
            if created && w.current_branch().as_deref() != Some(c.as_str()) {
                w.repo.git(&["branch", "-D", c], None)?;
            }
        }
    } else {
        if let Some(c) = &cur {
            if all.contains(c) {
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
    }
    // The stack as it was, keeping what is newer than the log: PR numbers
    // and status (a push cannot be undone), agent settings, and branches
    // given to this window since.
    let mut state = e.before.state.clone();
    for b in &mut state.branches {
        if let Some(n) = w.state.branches.iter().find(|x| x.name == b.name) {
            b.pr = n.pr;
            b.pr_state = n.pr_state.clone();
            b.pr_status = n.pr_status.clone();
        }
    }
    state.agents = w.state.agents.clone();
    state.branches.extend(extras);
    if let (Some(t), Some(s)) = (e.transfer.as_ref(), theirs) {
        let mut s = s.unwrap_or_else(|| State {
            branches: Vec::new(),
            pending: None,
            ..w.state.clone()
        });
        if t.outgoing {
            s.branches.retain(|b| !t.branches.contains(&b.name));
        } else {
            for n in &t.branches {
                if let Some(b) = w.state.branches.iter().find(|b| &b.name == n) {
                    s.branches.retain(|x| x.name != b.name);
                    s.branches.push(b.clone());
                }
            }
        }
        crate::stack::write_state(&t.window, &s)?;
    }
    w.state = state;
    w.save()?;
    let after = capture(w);
    let now_cap = widen(now_cap, &after, &refs);
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
        Extra {
            undoes: Some(e.id),
            transfer: e.transfer.clone().map(|t| Transfer {
                outgoing: !t.outgoing,
                ..t
            }),
            from_worktree: e.from_worktree,
        },
    )?;
    Ok(Undone {
        undid: e.id,
        command: e.command,
        entry,
        branches: changed(&now_cap, &after),
        head: after.head,
    })
}
