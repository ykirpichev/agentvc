//! Content-addressed object store using git's loose object format, so a
//! snapshot's tree hash is the same hash git would compute and export to git
//! is a copy of objects plus a ref update.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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
    fn as_str(self) -> &'static str {
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
}

impl ObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> ObjectStore {
        ObjectStore { root: root.into() }
    }

    fn path_for(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    fn write_object(&self, kind: &str, data: &[u8]) -> Result<Oid> {
        let header = format!("{kind} {}\0", data.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(data);
        let oid = Oid(hasher.finalize().into());
        let path = self.path_for(&oid);
        if path.exists() {
            return Ok(oid);
        }
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
        Ok(oid)
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
        let file = fs::File::open(&path).map_err(|_| Error::NotFound {
            kind: "object",
            id: oid.to_hex(),
        })?;
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
        let bad = || Error::BadObject(oid.to_hex());
        let mut entries = Vec::new();
        let mut rest = &data[..];
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

    /// The root directory of the store.
    pub fn root(&self) -> &Path {
        &self.root
    }
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
