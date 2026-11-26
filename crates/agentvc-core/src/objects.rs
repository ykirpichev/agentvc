//! Content-addressed object store using git's loose object format, so a
//! snapshot's tree hash is the same hash git would compute and export to git
//! is a copy of objects plus a ref update.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Digest, Sha1};

use crate::error::{Error, IoContext, Result};

/// A 20-byte git object id, shown as 40 hex characters.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<Oid> {
        let bytes = hex::decode(s).map_err(|_| Error::BadObject(s.to_string()))?;
        let arr: [u8; 20] = bytes
            .try_into()
            .map_err(|_| Error::BadObject(s.to_string()))?;
        Ok(Oid(arr))
    }
}

impl std::fmt::Display for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl std::fmt::Debug for Oid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Oid({})", &self.to_hex()[..12])
    }
}

/// File mode of a tree entry, matching git's modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    File,
    Executable,
    Symlink,
    Tree,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::File => "100644",
            Mode::Executable => "100755",
            Mode::Symlink => "120000",
            Mode::Tree => "40000",
        }
    }

    fn parse(s: &str) -> Option<Mode> {
        match s {
            "100644" | "100664" => Some(Mode::File),
            "100755" => Some(Mode::Executable),
            "120000" => Some(Mode::Symlink),
            "40000" | "040000" => Some(Mode::Tree),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub mode: Mode,
    pub oid: Oid,
}

pub struct ObjectStore {
    root: PathBuf,
    packs: RwLock<Vec<Arc<Pack>>>,
}

/// One git packfile (`objects/pack/pack-*.pack`) with its v2 index.
struct Pack {
    name: String,
    idx: Vec<u8>,
    count: usize,
    file: fs::File,
}

/// Longest delta chain followed when reading a packed object.
const MAX_DELTA_DEPTH: usize = 128;

impl Pack {
    fn open(idx_path: &Path) -> Option<Pack> {
        let idx = fs::read(idx_path).ok()?;
        if idx.len() < 8 + 1024 || idx[..4] != [0xff, b't', b'O', b'c'] || idx[4..8] != [0, 0, 0, 2]
        {
            return None;
        }
        let count = u32::from_be_bytes(idx[8 + 255 * 4..8 + 256 * 4].try_into().ok()?) as usize;
        if idx.len() < 8 + 1024 + count * 28 + 40 {
            return None;
        }
        let file = fs::File::open(idx_path.with_extension("pack")).ok()?;
        let name = idx_path.file_stem()?.to_string_lossy().into_owned();
        Some(Pack {
            name,
            idx,
            count,
            file,
        })
    }

    fn fanout(&self, i: usize) -> usize {
        let at = 8 + i * 4;
        u32::from_be_bytes(self.idx[at..at + 4].try_into().expect("4 bytes")) as usize
    }

    fn name_at(&self, i: usize) -> &[u8] {
        let at = 8 + 1024 + i * 20;
        &self.idx[at..at + 20]
    }

    /// Position of `oid` in the index.
    fn find(&self, oid: &Oid) -> Option<usize> {
        let first = oid.0[0] as usize;
        let mut lo = if first == 0 {
            0
        } else {
            self.fanout(first - 1)
        };
        let mut hi = self.fanout(first);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.name_at(mid).cmp(&oid.0[..]) {
                std::cmp::Ordering::Equal => return Some(mid),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }

    fn offset_at(&self, i: usize) -> Option<u64> {
        let base = 8 + 1024 + self.count * 24;
        let at = base + i * 4;
        let v = u32::from_be_bytes(self.idx.get(at..at + 4)?.try_into().ok()?);
        if v & 0x8000_0000 == 0 {
            return Some(v as u64);
        }
        let big = base + self.count * 4 + (v & 0x7fff_ffff) as usize * 8;
        Some(u64::from_be_bytes(
            self.idx.get(big..big + 8)?.try_into().ok()?,
        ))
    }

    fn oids(&self) -> impl Iterator<Item = Oid> + '_ {
        (0..self.count).map(|i| Oid(self.name_at(i).try_into().expect("20 bytes")))
    }

    /// Read the object at `offset`, resolving deltas.
    fn read_at(&self, store: &ObjectStore, offset: u64, depth: usize) -> Option<(u8, Vec<u8>)> {
        use std::os::unix::fs::FileExt;
        if depth > MAX_DELTA_DEPTH {
            return None;
        }
        let mut head = [0u8; 32];
        let n = self.file.read_at(&mut head, offset).ok()?;
        let head = &head[..n];
        let mut p = 0;
        let mut c = *head.get(p)?;
        p += 1;
        let kind = (c >> 4) & 7;
        let mut size = (c & 15) as usize;
        let mut shift = 4;
        while c & 0x80 != 0 {
            c = *head.get(p)?;
            p += 1;
            size |= ((c & 0x7f) as usize) << shift;
            shift += 7;
        }
        let mut base = None;
        if kind == 6 {
            c = *head.get(p)?;
            p += 1;
            let mut off = (c & 0x7f) as u64;
            while c & 0x80 != 0 {
                c = *head.get(p)?;
                p += 1;
                off = ((off + 1) << 7) | (c & 0x7f) as u64;
            }
            base = Some(self.read_at(store, offset.checked_sub(off)?, depth + 1)?);
        } else if kind == 7 {
            let id = Oid(head.get(p..p + 20)?.try_into().ok()?);
            p += 20;
            let (k, data) = store.read(&id).ok()?;
            base = Some((kind_code(&k)?, data));
        }
        let reader = std::io::BufReader::new(PackReader {
            file: &self.file,
            pos: offset + p as u64,
        });
        let mut data = vec![0u8; size];
        ZlibDecoder::new(reader).read_exact(&mut data).ok()?;
        match base {
            None => Some((kind, data)),
            Some((bkind, bdata)) => Some((bkind, apply_delta(&bdata, &data)?)),
        }
    }
}

struct PackReader<'a> {
    file: &'a fs::File,
    pos: u64,
}

impl Read for PackReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let n = self.file.read_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

fn kind_code(kind: &str) -> Option<u8> {
    Some(match kind {
        "commit" => 1,
        "tree" => 2,
        "blob" => 3,
        "tag" => 4,
        _ => return None,
    })
}

fn kind_name(code: u8) -> Option<&'static str> {
    Some(match code {
        1 => "commit",
        2 => "tree",
        3 => "blob",
        4 => "tag",
        _ => return None,
    })
}

fn delta_varint(d: &[u8], p: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    let mut shift = 0;
    loop {
        let c = *d.get(*p)?;
        *p += 1;
        v |= ((c & 0x7f) as usize) << shift;
        shift += 7;
        if c & 0x80 == 0 {
            return Some(v);
        }
    }
}

/// Apply a git delta to `base`.
fn apply_delta(base: &[u8], delta: &[u8]) -> Option<Vec<u8>> {
    let mut p = 0;
    if delta_varint(delta, &mut p)? != base.len() {
        return None;
    }
    let size = delta_varint(delta, &mut p)?;
    let mut out = Vec::with_capacity(size);
    while p < delta.len() {
        let op = delta[p];
        p += 1;
        if op & 0x80 != 0 {
            let (mut off, mut len) = (0usize, 0usize);
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    off |= (*delta.get(p)? as usize) << (8 * i);
                    p += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    len |= (*delta.get(p)? as usize) << (8 * i);
                    p += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            out.extend_from_slice(base.get(off..off.checked_add(len)?)?);
        } else if op != 0 {
            let n = op as usize;
            out.extend_from_slice(delta.get(p..p + n)?);
            p += n;
        } else {
            return None;
        }
    }
    (out.len() == size).then_some(out)
}

/// Hash of an object with git's header.
pub fn hash_object(kind: &str, data: &[u8]) -> Oid {
    let mut hasher = Sha1::new();
    hasher.update(format!("{kind} {}\0", data.len()).as_bytes());
    hasher.update(data);
    Oid(hasher.finalize().into())
}

/// How an object is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    Loose,
    Packed,
    Missing,
}

impl ObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> ObjectStore {
        let store = ObjectStore {
            root: root.into(),
            packs: RwLock::new(Vec::new()),
        };
        store.reload_packs();
        store
    }

    /// Directory holding packfiles.
    pub fn pack_dir(&self) -> PathBuf {
        self.root.join("pack")
    }

    /// Re-read the list of packfiles (after a `gc` in this or another
    /// process). Packs already open stay open.
    pub fn reload_packs(&self) {
        let mut found = Vec::new();
        if let Ok(rd) = fs::read_dir(self.pack_dir()) {
            let old = self.packs.read().expect("lock").clone();
            for e in rd.flatten() {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "idx") {
                    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
                    if let Some(p) = old.iter().find(|p| Some(&p.name) == stem.as_ref()) {
                        found.push(p.clone());
                    } else if let Some(p) = Pack::open(&path) {
                        found.push(Arc::new(p));
                    }
                }
            }
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        *self.packs.write().expect("lock") = found;
    }

    fn packs(&self) -> Vec<Arc<Pack>> {
        self.packs.read().expect("lock").clone()
    }

    fn find_packed(&self, oid: &Oid) -> Option<(Arc<Pack>, u64)> {
        self.packs()
            .into_iter()
            .find_map(|p| p.find(oid).and_then(|i| p.offset_at(i)).map(|o| (p, o)))
    }

    fn read_packed(&self, oid: &Oid) -> Option<(String, Vec<u8>)> {
        for attempt in 0..2 {
            if attempt == 1 {
                self.reload_packs();
            }
            if let Some((pack, off)) = self.find_packed(oid) {
                if let Some((k, data)) = pack.read_at(self, off, 0) {
                    return Some((kind_name(k)?.to_string(), data));
                }
            }
        }
        None
    }

    /// Where an object is stored.
    pub fn locate(&self, oid: &Oid) -> Location {
        if self.path_for(oid).exists() {
            return Location::Loose;
        }
        if self.find_packed(oid).is_some() {
            return Location::Packed;
        }
        self.reload_packs();
        if self.find_packed(oid).is_some() {
            Location::Packed
        } else if self.path_for(oid).exists() {
            Location::Loose
        } else {
            Location::Missing
        }
    }

    /// Number of packfiles and the objects in them.
    pub fn pack_stats(&self) -> (usize, usize) {
        let packs = self.packs();
        (packs.len(), packs.iter().map(|p| p.count).sum())
    }

    /// Every packed object id (may repeat across packs).
    pub fn packed_oids(&self) -> Vec<Oid> {
        self.packs()
            .iter()
            .flat_map(|p| p.oids().collect::<Vec<_>>())
            .collect()
    }

    /// Path of the loose file of an object (which may not exist).
    pub fn loose_path(&self, oid: &Oid) -> PathBuf {
        self.path_for(oid)
    }

    /// Every loose object with its file's size and modification time.
    pub fn loose_objects(&self) -> Vec<(Oid, u64, SystemTime)> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(&self.root) else {
            return out;
        };
        for d in rd.flatten() {
            let dname = d.file_name().to_string_lossy().into_owned();
            if dname.len() != 2 || !dname.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let Ok(files) = fs::read_dir(d.path()) else {
                continue;
            };
            for f in files.flatten() {
                let fname = f.file_name().to_string_lossy().into_owned();
                let Ok(oid) = Oid::from_hex(&format!("{dname}{fname}")) else {
                    continue;
                };
                if let Ok(m) = f.metadata() {
                    out.push((oid, m.len(), m.modified().unwrap_or(UNIX_EPOCH)));
                }
            }
        }
        out
    }

    /// Bytes of a loose object as git stores it (zlib of header and body),
    /// for copying it into a git object directory. Packed objects are
    /// recompressed.
    pub fn loose_bytes(&self, oid: &Oid) -> Result<Vec<u8>> {
        if let Ok(b) = fs::read(self.path_for(oid)) {
            return Ok(b);
        }
        let (kind, data) = self.read(oid)?;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
        let header = format!("{kind} {}\0", data.len());
        enc.write_all(header.as_bytes()).at(self.path_for(oid))?;
        enc.write_all(&data).at(self.path_for(oid))?;
        enc.finish().at(self.path_for(oid))
    }

    /// Read an object and check that its content hashes to its id.
    pub fn verify(&self, oid: &Oid) -> Result<String> {
        let (kind, data) = self.read(oid)?;
        if hash_object(&kind, &data) != *oid {
            return Err(Error::BadObject(oid.to_hex()));
        }
        Ok(kind)
    }

    fn path_for(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    fn write_object(&self, kind: &str, data: &[u8]) -> Result<Oid> {
        let oid = hash_object(kind, data);
        let path = self.path_for(&oid);
        if let Ok(m) = fs::metadata(&path) {
            freshen(&path, &m);
            return Ok(oid);
        }
        if self.find_packed(&oid).is_some() {
            return Ok(oid);
        }
        self.write_loose(&oid, kind, data)?;
        Ok(oid)
    }

    fn write_loose(&self, oid: &Oid, kind: &str, data: &[u8]) -> Result<()> {
        let header = format!("{kind} {}\0", data.len());
        let path = self.path_for(oid);
        let dir = path.parent().expect("object path has a parent");
        fs::create_dir_all(dir).at(dir)?;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(header.as_bytes()).at(&path)?;
        enc.write_all(data).at(&path)?;
        let compressed = enc.finish().at(&path)?;
        // Write to a temp file then rename, so a crash never leaves a torn object.
        let tmp = dir.join(format!(".tmp-{}", std::process::id()));
        fs::write(&tmp, compressed).at(&tmp)?;
        fs::rename(&tmp, &path).at(&path)?;
        Ok(())
    }

    /// Write an object as a loose file even if a copy exists (in a pack, or
    /// a corrupt loose file), for `doctor --repair`.
    pub fn rewrite(&self, kind: &str, data: &[u8]) -> Result<Oid> {
        let oid = hash_object(kind, data);
        let _ = fs::remove_file(self.path_for(&oid));
        self.write_loose(&oid, kind, data)?;
        Ok(oid)
    }

    /// Store an object whose body is already in git's format, such as a
    /// tree read from a git repository.
    pub fn write_raw(&self, kind: &str, data: &[u8]) -> Result<Oid> {
        self.write_object(kind, data)
    }

    /// True when the object is in the store.
    pub fn contains(&self, oid: &Oid) -> bool {
        self.locate(oid) != Location::Missing
    }

    pub fn write_blob(&self, data: &[u8]) -> Result<Oid> {
        self.write_object("blob", data)
    }

    /// Write a tree. Entries are sorted in git order before hashing.
    pub fn write_tree(&self, entries: &mut [TreeEntry]) -> Result<Oid> {
        entries.sort_by_key(git_sort_key);
        let mut data = Vec::new();
        for e in entries.iter() {
            data.extend_from_slice(e.mode.as_str().as_bytes());
            data.push(b' ');
            data.extend_from_slice(e.name.as_bytes());
            data.push(0);
            data.extend_from_slice(&e.oid.0);
        }
        self.write_object("tree", &data)
    }

    pub fn read(&self, oid: &Oid) -> Result<(String, Vec<u8>)> {
        let path = self.path_for(oid);
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => {
                return self.read_packed(oid).ok_or(Error::NotFound {
                    kind: "object",
                    id: oid.to_hex(),
                })
            }
        };
        let mut raw = Vec::new();
        ZlibDecoder::new(file).read_to_end(&mut raw).at(&path)?;
        let nul = raw
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::BadObject(oid.to_hex()))?;
        let header =
            std::str::from_utf8(&raw[..nul]).map_err(|_| Error::BadObject(oid.to_hex()))?;
        let kind = header
            .split(' ')
            .next()
            .ok_or_else(|| Error::BadObject(oid.to_hex()))?
            .to_string();
        Ok((kind, raw[nul + 1..].to_vec()))
    }

    pub fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>> {
        match self.read(oid)? {
            (k, data) if k == "blob" => Ok(data),
            _ => Err(Error::BadObject(oid.to_hex())),
        }
    }

    pub fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>> {
        let (kind, data) = self.read(oid)?;
        if kind != "tree" {
            return Err(Error::BadObject(oid.to_hex()));
        }
        parse_tree(oid, &data)
    }

    /// The root directory of the store.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Loose objects older than this get their mtime bumped when written again,
/// so `gc` (which keeps recent loose objects) sees them as in use.
const FRESHEN_AFTER: std::time::Duration = std::time::Duration::from_secs(600);

fn freshen(path: &Path, m: &fs::Metadata) {
    let old = m
        .modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > FRESHEN_AFTER);
    if old {
        if let Ok(f) = fs::File::options().append(true).open(path) {
            let _ = f.set_modified(SystemTime::now());
        }
    }
}

/// Parse the body of tree object `oid`.
pub fn parse_tree(oid: &Oid, data: &[u8]) -> Result<Vec<TreeEntry>> {
    let bad = || Error::BadObject(oid.to_hex());
    let mut entries = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let sp = rest.iter().position(|&b| b == b' ').ok_or_else(bad)?;
        let nul = rest.iter().position(|&b| b == 0).ok_or_else(bad)?;
        let mode = std::str::from_utf8(&rest[..sp]).map_err(|_| bad())?;
        let name = std::str::from_utf8(&rest[sp + 1..nul]).map_err(|_| bad())?;
        let id: [u8; 20] = rest
            .get(nul + 1..nul + 21)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?;
        entries.push(TreeEntry {
            name: name.to_string(),
            mode: Mode::parse(mode).ok_or_else(bad)?,
            oid: Oid(id),
        });
        rest = &rest[nul + 21..];
    }
    Ok(entries)
}

/// Git sorts tree entries by name, comparing directories as if they ended in '/'.
fn git_sort_key(e: &TreeEntry) -> Vec<u8> {
    let mut k = e.name.as_bytes().to_vec();
    if e.mode == Mode::Tree {
        k.push(b'/');
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_hash_matches_git() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        // `printf 'hello\n' | git hash-object --stdin`
        let oid = store.write_blob(b"hello\n").unwrap();
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(store.read_blob(&oid).unwrap(), b"hello\n");
    }

    #[test]
    fn empty_tree_hash_matches_git() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let oid = store.write_tree(&mut []).unwrap();
        assert_eq!(oid.to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    }

    #[test]
    fn tree_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(dir.path());
        let blob = store.write_blob(b"x").unwrap();
        let mut entries = vec![
            TreeEntry {
                name: "b".into(),
                mode: Mode::Executable,
                oid: blob,
            },
            TreeEntry {
                name: "a".into(),
                mode: Mode::File,
                oid: blob,
            },
        ];
        let oid = store.write_tree(&mut entries).unwrap();
        let back = store.read_tree(&oid).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "a");
        assert_eq!(back[1].mode, Mode::Executable);
    }
}
