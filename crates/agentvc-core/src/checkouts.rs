//! Spaces and their checkouts (#160, #162).
//!
//! A *space* is one project that humans and agents work on together. It
//! holds *workspaces*: isolated places (a worktree or a clone folder, on
//! some machine) where one agent or person works without getting in the
//! others' way, each with its own stack.
//!
//! A space is identified by its *key*: git config `agentvc.space`, else the
//! normalized remote URL. Its id is derived from the key (`s` + 8 hex of
//! its SHA-1), so every machine and user that works on the same repo agrees
//! on it without talking to each other. Its display name (default: the last
//! part of the key, e.g. `agentvc`) is kept in this machine's registry.
//!
//! The same repo cloned into several folders joins one space:
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
//!   dropped. Display names of spaces live there too.
//!
//! (Not to be confused with [`crate::Space`], one folder's attempt history
//! under `.agentvc/`.)

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::error::{Error, IoContext, Result};
use crate::space::now;
use crate::stack::{canon, Window};

/// Git config key holding a checkout's space name.
pub const CONFIG_KEY: &str = "agentvc.space";
/// The space name that means "this checkout joins no space".
pub const NONE: &str = "none";
const REGISTRY_VERSION: u32 = 2;

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

/// A space's display name, set with `agentvc space name`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Named {
    key: String,
    name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    version: u32,
    checkouts: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    names: Vec<Named>,
}

/// A space as people and the UI see it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SpaceRef {
    /// Stable id, the same on every machine: `s` + 8 hex digits of the key.
    pub id: String,
    /// What people call it.
    pub name: String,
    /// What identifies it: `agentvc.space`, else the remote's URL.
    pub key: String,
    /// Where the key came from.
    pub source: Source,
}

/// The id of the space with `key`.
pub fn space_id(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    format!("s{}", &hex::encode(h.finalize())[..8])
}

/// The name a space gets until someone names it: the key's last part.
pub fn default_name(key: &str) -> String {
    let last = key
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(key);
    last.strip_suffix(".git")
        .filter(|s| !s.is_empty())
        .unwrap_or(last)
        .to_string()
}

fn name_in(r: &Registry, key: &str) -> String {
    r.names
        .iter()
        .find(|n| n.key == key)
        .map(|n| n.name.clone())
        .unwrap_or_else(|| default_name(key))
}

fn make_ref(r: &Registry, key: String, source: Source) -> SpaceRef {
    SpaceRef {
        id: space_id(&key),
        name: name_in(r, &key),
        key,
        source,
    }
}

/// The space the checkout `w` belongs to, with its id and name.
pub fn describe(w: &Window) -> Option<SpaceRef> {
    let (key, source) = space_of(w);
    let r = registry_file()
        .map(|f| read_registry(&f))
        .unwrap_or_default();
    key.map(|k| make_ref(&r, k, source))
}

/// Give the space of `w` a display name (`None`: back to the default).
pub fn set_name(w: &Window, name: Option<&str>) -> Result<SpaceRef> {
    let Some(sp) = describe(w) else {
        return Err(Error::Stack {
            code: "no_space",
            message: "this checkout joins no space; `agentvc space join NAME` joins one".into(),
            details: None,
        });
    };
    let name = name.map(str::trim);
    if let Some(n) = name {
        if n.is_empty() || n.chars().any(|c| c.is_control()) {
            return Err(Error::Stack {
                code: "bad_space",
                message: format!("{n:?} is not a space name"),
                details: None,
            });
        }
    }
    let Some(file) = registry_file() else {
        return Err(Error::Stack {
            code: "no_home",
            message: "no home directory to keep space names in; set AGENTVC_HOME".into(),
            details: None,
        });
    };
    let _lock = lock_registry_at(&file)?;
    let mut r = read_registry(&file);
    r.names.retain(|n| n.key != sp.key);
    if let Some(n) = name.filter(|n| *n != default_name(&sp.key)) {
        r.names.push(Named {
            key: sp.key.clone(),
            name: n.to_string(),
        });
    }
    r.version = REGISTRY_VERSION;
    write_registry(&file, &r)?;
    Ok(make_ref(&r, sp.key, sp.source))
}

/// One space this machine knows, with its checkouts here.
#[derive(Clone, Debug, Serialize)]
pub struct KnownSpace {
    #[serde(flatten)]
    pub space: SpaceRef,
    /// Main worktrees of its checkouts on this machine.
    pub checkouts: Vec<PathBuf>,
}

/// Every space with a checkout on this machine, by name.
pub fn known_spaces() -> Vec<KnownSpace> {
    let r = registry_file()
        .map(|f| read_registry(&f))
        .unwrap_or_default();
    let mut out: Vec<KnownSpace> = Vec::new();
    for e in &r.checkouts {
        let Some(key) = &e.space else { continue };
        if !e.path.exists() {
            continue;
        }
        match out.iter_mut().find(|k| &k.space.key == key) {
            Some(k) => k.checkouts.push(e.path.clone()),
            None => out.push(KnownSpace {
                // The registry does not say; most spaces come from a remote.
                space: make_ref(&r, key.clone(), Source::Remote),
                checkouts: vec![e.path.clone()],
            }),
        }
    }
    out.sort_by(|a, b| a.space.name.cmp(&b.space.name));
    out
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

/// How long to wait for another process's registry lock.
const REGISTRY_LOCK_WAIT: Duration = Duration::from_secs(15);

/// Exclusive right to change this machine's registry; released on drop, or
/// by the OS when the process dies, and never taken over by age (#204).
///
/// Every change is read-modify-write of one file, so without it two
/// processes registering at once (two checkouts listed at the same time, the
/// daemon and the CLI) lose one entry, and the lost checkout's workspaces
/// drop out of its space: names stop being unique (#180).
pub struct RegistryLock {
    _held: crate::filelock::FileLock,
}

/// Take the registry lock, waiting for another process to finish. `None`
/// when there is no registry (no home directory).
pub fn lock_registry() -> Result<Option<RegistryLock>> {
    match registry_file() {
        Some(file) => lock_registry_at(&file).map(Some),
        None => Ok(None),
    }
}

fn lock_registry_at(file: &Path) -> Result<RegistryLock> {
    let dir = file.parent().expect("has a parent");
    let path = dir.join("checkouts.lock");
    match crate::filelock::acquire(&path, REGISTRY_LOCK_WAIT, Duration::from_millis(10))? {
        Ok(l) => Ok(RegistryLock { _held: l }),
        Err(holder) => {
            let by = holder
                .as_ref()
                .map(|h| format!(" by {}", h.describe()))
                .unwrap_or_default();
            Err(Error::Stack {
                code: "registry_locked",
                message: format!(
                    "another agentvc command is changing this machine's checkouts{by} ({}); try again",
                    path.display()
                ),
                details: Some(serde_json::json!({ "lock": path, "holder": holder })),
            })
        }
    }
}

/// A temp file name no other process or thread uses at the same time.
pub(crate) fn tmp_name(base: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        ".{base}.{}.{}.tmp",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// Write the registry atomically; the caller holds the [`RegistryLock`].
fn write_registry(file: &Path, r: &Registry) -> Result<()> {
    let dir = file.parent().expect("has a parent");
    fs::create_dir_all(dir).at(dir)?;
    let tmp = dir.join(tmp_name("checkouts.json"));
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
    let lock = registry_file().and_then(|f| lock_registry_at(&f).ok());
    register_locked(w, lock.as_ref())
}

/// [`register`] for a caller already holding the registry lock (`None`:
/// registers nothing, as without a home directory).
pub fn register_locked(w: &Window, lock: Option<&RegistryLock>) -> Option<String> {
    let (space, _) = space_of(w);
    if let (Some(file), Some(_)) = (registry_file(), lock) {
        let _ = register_at(&file, &main_path(w), &w.common_dir, space.clone());
    }
    space
}

/// Add or refresh one entry; the caller holds the [`RegistryLock`].
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
    fn space_ids_and_default_names() {
        assert_eq!(space_id("github.com/o/r"), space_id("github.com/o/r"));
        assert_ne!(space_id("github.com/o/r"), space_id("github.com/o/s"));
        assert!(space_id("x").starts_with('s') && space_id("x").len() == 9);
        assert_eq!(default_name("github.com/o/mono"), "mono");
        assert_eq!(default_name("/srv/git/mono/"), "mono");
        assert_eq!(default_name("team-mono"), "team-mono");
        assert_eq!(default_name("/srv/git/mono.git"), "mono");
    }

    #[test]
    fn registry_reads_version_1_files() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("checkouts.json");
        fs::write(&file, r#"{"version":1,"checkouts":[]}"#).unwrap();
        let r = read_registry(&file);
        assert!(r.names.is_empty());
        assert_eq!(name_in(&r, "github.com/o/r"), "r");
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

    #[test]
    fn checkouts_registering_at_once_are_all_kept() {
        // Each registration is read-modify-write of one file: without the
        // lock, concurrent ones lose entries (#180).
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("home/checkouts.json");
        let dirs: Vec<(PathBuf, PathBuf)> = (0..16)
            .map(|i| {
                let d = tmp.path().join(format!("c{i}"));
                fs::create_dir_all(d.join(".git")).unwrap();
                (d.clone(), d.join(".git"))
            })
            .collect();
        std::thread::scope(|sc| {
            for (d, g) in &dirs {
                let file = &file;
                sc.spawn(move || {
                    let _lock = lock_registry_at(file).unwrap();
                    register_at(file, d, g, Some("s".into())).unwrap();
                });
            }
        });
        assert_eq!(read_registry(&file).checkouts.len(), dirs.len());
        // Released: free at once (the lock file itself stays).
        drop(lock_registry_at(&file).unwrap());
    }
}
