//! The in-memory listing behind every archive, snapshot and repository: a
//! map from normalized entry path (`dir/file.txt`, root `""`) to metadata,
//! plus each folder's children. Formats that only list files (most zips,
//! every tar written without folder entries) get their folders implied.

use std::collections::{BTreeSet, HashMap};
use std::io;

use pcompare_core::folder::{attr, EntryMeta};
use pcompare_core::vfs::VfsEntry;
use pcompare_core::{CoreError, Result};

/// Where an entry's bytes live inside its container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Loc {
    /// A folder, or anything without content of its own.
    None,
    /// Index in the zip's central directory.
    Zip(usize),
    /// Byte offset of the data in the (decompressed) tar stream.
    Tar(u64),
    /// Index in the 7z archive's file list.
    SevenZ(usize),
    /// The one file inside a `.gz` / `.bz2` / `.xz`.
    Single,
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub meta: EntryMeta,
    pub hidden: bool,
    /// CRC-32 recorded by the container (zip and 7z headers, snapshots).
    pub crc: Option<u32>,
    pub loc: Loc,
}

#[derive(Debug, Clone)]
pub(crate) struct Tree {
    nodes: HashMap<String, Node>,
    children: HashMap<String, BTreeSet<String>>,
}

/// `a\b/./c/` → `a/b/c`. `None` for paths climbing out with `..`, which an
/// archive may carry but that must never resolve outside it.
pub(crate) fn normalize(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            p => out.push(p),
        }
    }
    Some(out.join("/"))
}

/// The parent folder and name of a normalized path.
pub(crate) fn split_parent(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn dir_meta() -> EntryMeta {
    EntryMeta {
        is_dir: true,
        ..Default::default()
    }
}

pub(crate) fn not_found(label: &str, path: &str) -> CoreError {
    CoreError::io(shown(label, path), io::ErrorKind::NotFound.into())
}

/// `label/path` as shown in messages.
pub(crate) fn shown(label: &str, path: &str) -> String {
    if path.is_empty() {
        label.to_owned()
    } else {
        format!("{}/{path}", label.trim_end_matches(['/', '\\']))
    }
}

impl Default for Tree {
    fn default() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            String::new(),
            Node {
                meta: dir_meta(),
                hidden: false,
                crc: None,
                loc: Loc::None,
            },
        );
        Tree {
            nodes,
            children: HashMap::new(),
        }
    }
}

impl Tree {
    /// Adds an entry (and any folders above it). A later entry with the same
    /// path replaces an earlier one, as tar's append-to-update expects; a
    /// folder keeps its children when re-declared.
    pub fn insert(&mut self, raw_path: &str, meta: EntryMeta, crc: Option<u32>, loc: Loc) {
        let Some(path) = normalize(raw_path) else {
            return;
        };
        if path.is_empty() {
            return;
        }
        self.ensure_dir(split_parent(&path).0);
        let name = split_parent(&path).1;
        let hidden =
            name.starts_with('.') || meta.attributes.is_some_and(|a| a & attr::HIDDEN != 0);
        if let Some(old) = self.nodes.get(&path) {
            if old.meta.is_dir && !meta.is_dir {
                // A file replacing a folder takes its contents away.
                self.remove_subtree(&path);
            }
        }
        let (parent, name) = split_parent(&path);
        self.children
            .entry(parent.to_owned())
            .or_default()
            .insert(name.to_owned());
        self.nodes.insert(
            path,
            Node {
                meta,
                hidden,
                crc,
                loc,
            },
        );
    }

    fn ensure_dir(&mut self, dir: &str) {
        if dir.is_empty() || self.nodes.get(dir).is_some_and(|n| n.meta.is_dir) {
            return;
        }
        let (parent, name) = split_parent(dir);
        self.ensure_dir(parent);
        self.children
            .entry(parent.to_owned())
            .or_default()
            .insert(name.to_owned());
        self.nodes.insert(
            dir.to_owned(),
            Node {
                meta: dir_meta(),
                hidden: name.starts_with('.'),
                crc: None,
                loc: Loc::None,
            },
        );
    }

    fn remove_subtree(&mut self, path: &str) {
        if let Some(kids) = self.children.remove(path) {
            for k in kids {
                let child = format!("{path}/{k}");
                self.remove_subtree(&child);
                self.nodes.remove(&child);
            }
        }
    }

    pub fn get(&self, path: &str) -> Option<&Node> {
        self.nodes.get(&normalize(path)?)
    }

    /// The direct children of a folder.
    pub fn list(&self, label: &str, dir: &str) -> Result<Vec<VfsEntry>> {
        let dir = normalize(dir).ok_or_else(|| not_found(label, dir))?;
        let node = self.nodes.get(&dir).ok_or_else(|| not_found(label, &dir))?;
        if !node.meta.is_dir {
            return Err(CoreError::Other(format!(
                "{} is a file, not a folder",
                shown(label, &dir)
            )));
        }
        let Some(kids) = self.children.get(&dir) else {
            return Ok(Vec::new());
        };
        Ok(kids
            .iter()
            .filter_map(|name| {
                let path = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}/{name}")
                };
                let n = self.nodes.get(&path)?;
                Some(VfsEntry {
                    name: name.clone(),
                    meta: n.meta.clone(),
                    hidden: n.hidden,
                })
            })
            .collect())
    }

    /// Every file (not folder), with its path.
    #[cfg(test)]
    pub fn files(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.nodes.iter().filter(|(_, n)| !n.meta.is_dir)
    }
}

/// Appends `name` to an entry path (the [`Vfs::join`] of every container).
///
/// [`Vfs::join`]: pcompare_core::vfs::Vfs::join
pub(crate) fn join(dir: &str, name: &str) -> String {
    let dir = dir.trim_end_matches(['/', '\\']);
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(size: u64) -> EntryMeta {
        EntryMeta {
            size,
            ..Default::default()
        }
    }

    #[test]
    fn implied_folders_and_replacement() {
        let mut t = Tree::default();
        t.insert("a/b/c.txt", file(3), Some(7), Loc::Zip(0));
        t.insert("./a\\d.txt", file(1), None, Loc::Zip(1));
        t.insert("../evil", file(1), None, Loc::Zip(2));
        let root: Vec<_> = t
            .list("x.zip", "")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(root, ["a"]);
        let a: Vec<_> = t
            .list("x.zip", "a")
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.meta.is_dir))
            .collect();
        assert_eq!(a, [("b".to_owned(), true), ("d.txt".to_owned(), false)]);
        assert_eq!(t.get("a/b/c.txt").unwrap().crc, Some(7));
        assert!(t.list("x.zip", "a/b/c.txt").is_err());
        assert!(t.list("x.zip", "nope").is_err());

        // A later file with a folder's name replaces the folder and its contents.
        t.insert("a/b", file(9), None, Loc::Zip(3));
        assert!(t.get("a/b/c.txt").is_none());
        assert_eq!(t.get("a/b").unwrap().meta.size, 9);
        assert_eq!(t.files().count(), 2);
    }
}
