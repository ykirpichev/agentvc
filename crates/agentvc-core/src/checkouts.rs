//! Checkouts of one space (#160): the same repo cloned into several folders
//! on one machine, shown and edited together.
//!
//! A monorepo's build often cannot run twice in one checkout, so people
//! clone it into several folders, one workspace (and one PR stack) each.
//! Every checkout says which *space* it belongs to; checkouts on this
//! machine with the same space list (and change) each other's workspaces.
//!
//! - The space name is git config `agentvc.space` of the checkout, set with
//!   `agentvc workspace space NAME`. Without it, it is the checkout's remote
//!   URL, normalized (`git@github.com:o/r.git` -> `github.com/o/r`), so
//!   clones of one repo join one space on their own. `none` opts out.
//! - This machine's checkouts are remembered in `checkouts.json` under
//!   `$AGENTVC_HOME` (default `~/.agentvc`). A checkout registers itself
//!   whenever its workspaces are listed; entries whose folder is gone are
//!   dropped.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::space::now;
use crate::stack::{canon, Window};

/// Git config key holding a checkout's space name.
pub const CONFIG_KEY: &str = "agentvc.space";
/// The space name that means "this checkout joins no space".
pub const NONE: &str = "none";
const REGISTRY_VERSION: u32 = 1;

/// One checkout this machine has seen.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Its main worktree.
    pub path: PathBuf,
    /// Its git common dir, which tells checkouts apart.
    pub common_dir: PathBuf,
    /// Its space when last seen; `None` if it joins none.
    pub space: Option<String>,
    pub seen_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    version: u32,
    checkouts: Vec<Entry>,
}

/// `$AGENTVC_HOME`, else `~/.agentvc`.
pub fn home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("AGENTVC_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    // Unit tests never touch the real registry.
    if cfg!(test) {
        return None;
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".agentvc"))
}

fn registry_file() -> Option<PathBuf> {
    home().map(|h| h.join("checkouts.json"))
}

fn read_registry(file: &Path) -> Registry {
    fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_registry(file: &Path, r: &Registry) -> Result<()> {
    let dir = file.parent().expect("has a parent");
    fs::create_dir_all(dir).at(dir)?;
    let tmp = dir.join(format!(".checkouts.json.{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(r).expect("serializes") + "\n";
    fs::write(&tmp, text).at(&tmp)?;
    fs::rename(&tmp, file).at(file)
}

/// `git@github.com:o/r.git`, `https://user@github.com/o/r/`,
/// `ssh://git@github.com:22/o/r.git` -> `github.com/o/r`. A local path is
/// made absolute (relative to `base`) and canonical.
pub fn normalize_url(url: &str, base: &Path) -> String {
    let url = url.trim();
    let strip = |s: &str| -> String {
        let s = s.trim_end_matches('/');
        let s = s.strip_suffix(".git").unwrap_or(s);
        s.trim_end_matches('/').to_string()
    };
    if let Some((scheme, rest)) = url.split_once("://") {
        if scheme == "file" {
            return canon(Path::new(rest)).to_string_lossy().into_owned();
        }
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = host.split(':').next().unwrap_or(host).to_lowercase();
        return strip(&format!("{host}/{path}"));
    }
    // scp-like `user@host:path` (but not a Windows drive `C:\...`).
    if let Some((left, path)) = url.split_once(':') {
        if !left.contains('/') && left.len() > 1 {
            let host = left.split_once('@').map(|(_, h)| h).unwrap_or(left);
            return strip(&format!(
                "{}/{}",
                host.to_lowercase(),
                path.trim_start_matches('/')
            ));
        }
    }
    let p = Path::new(url);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    canon(&p).to_string_lossy().into_owned()
}

/// Where the space name came from.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `agentvc workspace space NAME` (git config `agentvc.space`).
    Config,
    /// The remote's URL.
    Remote,
    /// Neither: the checkout joins no space.
    None,
}

/// The space of the checkout `w` belongs to, and where that came from.
pub fn space_of(w: &Window) -> (Option<String>, Source) {
    let set = w
        .repo
        .git(&["config", "--get", CONFIG_KEY], None)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match set {
        Some(s) if s == NONE => (None, Source::Config),
        Some(s) => (Some(s), Source::Config),
        None => {
            let remote = if w.state.remote.is_empty() {
                "origin"
            } else {
                &w.state.remote
            };
            match w.repo.git(&["remote", "get-url", remote], None) {
                Ok(u) if !u.trim().is_empty() => {
                    (Some(normalize_url(&u, w.path())), Source::Remote)
                }
                _ => (None, Source::None),
            }
        }
    }
}

/// Set (or with `None`, clear back to the remote's URL) the space of the
/// checkout `w`. `none` makes it join no space.
pub fn set_space(w: &Window, name: Option<&str>) -> Result<()> {
    match name.map(str::trim) {
        Some(n) if n.is_empty() || n.chars().any(|c| c.is_control()) => Err(Error::Stack {
            code: "bad_space",
            message: format!("{n:?} is not a space name"),
            details: None,
        }),
        Some(n) => w
            .repo
            .git(&["config", "--local", CONFIG_KEY, n], None)
            .map(|_| ()),
        None => {
            // Unset fails when it was never set; that is fine.
            let _ = w
                .repo
                .git(&["config", "--local", "--unset", CONFIG_KEY], None);
            Ok(())
        }
    }
}

/// The main worktree of the checkout `w` belongs to.
pub fn main_path(w: &Window) -> PathBuf {
    w.worktrees()
        .into_iter()
        .next()
        .map(|(_, p)| p)
        .unwrap_or_else(|| w.path().to_path_buf())
}

/// Remember the checkout `w` belongs to in this machine's registry (best
/// effort: no home directory or an unwritable one registers nothing).
/// Returns its space.
pub fn register(w: &Window) -> Option<String> {
    let (space, _) = space_of(w);
    if let Some(file) = registry_file() {
        let _ = register_at(&file, &main_path(w), &w.common_dir, space.clone());
    }
    space
}

fn register_at(file: &Path, path: &Path, common_dir: &Path, space: Option<String>) -> Result<()> {
    let mut r = read_registry(file);
    let before = r.checkouts.clone();
    r.checkouts.retain(|e| e.common_dir.exists());
    let now = now();
    match r.checkouts.iter_mut().find(|e| e.common_dir == common_dir) {
        Some(e) => {
            // Rewrite only when something changed (or once a day), so
            // listing does not write a file every time.
            if e.space != space || e.path != path || now - e.seen_at > 86_400 {
                e.space = space;
                e.path = path.to_path_buf();
                e.seen_at = now;
            }
        }
        None => r.checkouts.push(Entry {
            path: path.to_path_buf(),
            common_dir: common_dir.to_path_buf(),
            space,
            seen_at: now,
        }),
    }
    if r.checkouts == before {
        return Ok(());
    }
    r.version = REGISTRY_VERSION;
    write_registry(file, &r)
}

/// Every checkout this machine knows, as last registered.
pub fn known() -> Vec<Entry> {
    registry_file()
        .map(|f| read_registry(&f).checkouts)
        .unwrap_or_default()
}

/// The other checkouts on this machine in the same space as `w` (main
/// worktree paths), as the registry says. Cheap: reads one small file.
pub fn siblings(w: &Window, space: Option<&str>) -> Vec<PathBuf> {
    let Some(space) = space else {
        return Vec::new();
    };
    let Some(file) = registry_file() else {
        return Vec::new();
    };
    siblings_in(&read_registry(&file), &w.common_dir, space)
}

fn siblings_in(r: &Registry, common_dir: &Path, space: &str) -> Vec<PathBuf> {
    r.checkouts
        .iter()
        .filter(|e| e.space.as_deref() == Some(space))
        .filter(|e| e.common_dir != common_dir && e.common_dir.exists() && e.path.exists())
        .map(|e| e.path.clone())
        .collect()
}

/// [`siblings`] by the space the registry holds for `w`, without asking
/// git (for pollers). Empty until `w` was registered.
pub fn registered_siblings(w: &Window) -> Vec<PathBuf> {
    let Some(file) = registry_file() else {
        return Vec::new();
    };
    let r = read_registry(&file);
    let Some(space) = r
        .checkouts
        .iter()
        .find(|e| e.common_dir == w.common_dir)
        .and_then(|e| e.space.clone())
    else {
        return Vec::new();
    };
    siblings_in(&r, &w.common_dir, &space)
}

/// The space of `w` and its sibling checkouts, registering `w` first.
pub fn space_and_siblings(w: &Window) -> (Option<String>, Vec<PathBuf>) {
    let space = register(w);
    let sib = siblings(w, space.as_deref());
    (space, sib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_normalize() {
        let b = Path::new("/");
        for u in [
            "git@github.com:Org/repo.git",
            "https://github.com/Org/repo",
            "https://user:tok@GitHub.com/Org/repo.git/",
            "ssh://git@github.com:22/Org/repo.git",
            "ssh://git@github.com/Org/repo",
        ] {
            assert_eq!(normalize_url(u, b), "github.com/Org/repo", "{u}");
        }
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("origin.git");
        fs::create_dir(&p).unwrap();
        let want = canon(&p).to_string_lossy().into_owned();
        assert_eq!(normalize_url(&p.to_string_lossy(), b), want);
        assert_eq!(normalize_url("origin.git", tmp.path()), want);
        assert_eq!(normalize_url(&format!("file://{}", p.display()), b), want);
    }

    #[test]
    fn registry_keeps_one_entry_per_checkout_and_finds_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("home/checkouts.json");
        let mk = |n: &str| {
            let d = tmp.path().join(n);
            fs::create_dir_all(d.join(".git")).unwrap();
            (d.clone(), d.join(".git"))
        };
        let (a, ag) = mk("a");
        let (b, bg) = mk("b");
        let (c, cg) = mk("c");
        let s = || Some("github.com/o/mono".to_string());
        register_at(&file, &a, &ag, s()).unwrap();
        register_at(&file, &b, &bg, s()).unwrap();
        register_at(&file, &c, &cg, Some("other".into())).unwrap();
        register_at(&file, &a, &ag, s()).unwrap();
        let r = read_registry(&file);
        assert_eq!(r.checkouts.len(), 3);
        assert_eq!(siblings_in(&r, &ag, "github.com/o/mono"), vec![b.clone()]);
        assert!(siblings_in(&r, &cg, "other").is_empty());
        // A checkout that moved to another space leaves this one.
        register_at(&file, &b, &bg, None).unwrap();
        assert!(siblings_in(&read_registry(&file), &ag, "github.com/o/mono").is_empty());
        // A deleted checkout is dropped at the next registration.
        fs::remove_dir_all(&c).unwrap();
        register_at(&file, &a, &ag, s()).unwrap();
        assert_eq!(read_registry(&file).checkouts.len(), 2);
    }
}
