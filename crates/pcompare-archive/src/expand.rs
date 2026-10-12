//! "Expand archives as folders": a wrapper file system under which every
//! archive file found while listing (zip, jar, tar.gz, 7z, ...) shows up as a
//! folder with the archive's contents, nested archives included.
//!
//! Paths keep the wrapped file system's syntax, continued past the archive
//! with its `join` (`C:\x\lib.jar\META-INF\MANIFEST.MF`), so the paths the
//! Folder Compare view builds open in the viewers through [`Archives`]'s
//! path resolution.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use pcompare_core::vfs::{ListOptions, Vfs, VfsEntry};
use pcompare_core::{CoreError, Result};

use crate::{hash_key, ArchiveFs, ArchiveKind, Archives, CrcSource};

/// Where a found archive's bytes come from.
enum Source {
    /// A file of the wrapped file system (with its size and date, which key
    /// the cached copy of a non-local file).
    Inner { size: u64, modified_ms: Option<i64> },
    /// A file inside another archive.
    Nested {
        parent: Arc<ArchiveFs>,
        path: String,
    },
}

struct Found {
    kind: ArchiveKind,
    source: Source,
    fs: OnceLock<std::result::Result<Arc<ArchiveFs>, String>>,
}

pub struct ExpandFs {
    inner: Arc<dyn Vfs>,
    /// The wrapped file system is the local disk, so archives open in place.
    local: bool,
    archives: Arc<Archives>,
    inner_crcs: Option<Arc<dyn CrcSource>>,
    /// Archives seen in listings, by their path in this file system.
    found: Mutex<HashMap<String, Arc<Found>>>,
}

/// Where a path of this file system lives.
enum Located {
    Inner(String),
    Archive(Arc<ArchiveFs>, String),
}

impl ExpandFs {
    pub(crate) fn new(
        inner: Arc<dyn Vfs>,
        local: bool,
        inner_crcs: Option<Arc<dyn CrcSource>>,
        archives: Arc<Archives>,
    ) -> Self {
        Self {
            inner,
            local,
            archives,
            inner_crcs,
            found: Mutex::new(HashMap::new()),
        }
    }

    fn open_found(&self, key: &str, found: &Found) -> Result<Arc<ArchiveFs>> {
        let opened = found.fs.get_or_init(|| {
            let read_only =
                Some("archives expanded in a folder comparison are read-only".to_owned());
            let file = match &found.source {
                Source::Inner { .. } if self.local => Ok(Path::new(key).to_path_buf()),
                Source::Inner { size, modified_ms } => {
                    let name = key.rsplit(['/', '\\']).next().unwrap_or("archive");
                    let cache_key = hash_key((key, size, modified_ms));
                    self.archives
                        .copy_into_cache(&cache_key, name, || self.inner.open(key))
                }
                Source::Nested { parent, path } => {
                    self.archives.materialize(&**parent, parent.label(), path)
                }
            };
            file.and_then(|f| self.archives.open_archive(&f, key, found.kind, read_only))
                .map_err(|e| e.to_string())
        });
        opened.clone().map_err(CoreError::Other)
    }

    fn locate(&self, path: &str) -> Result<Located> {
        let best = {
            let map = self.found.lock().unwrap_or_else(|e| e.into_inner());
            map.iter()
                .filter(|(k, _)| {
                    path == k.as_str()
                        || (path.starts_with(k.as_str())
                            && path[k.len()..].starts_with(['/', '\\']))
                })
                .max_by_key(|(k, _)| k.len())
                .map(|(k, f)| (k.clone(), f.clone()))
        };
        match best {
            None => Ok(Located::Inner(path.to_owned())),
            Some((key, found)) => {
                let fs = self.open_found(&key, &found)?;
                let rel = path[key.len()..]
                    .trim_start_matches(['/', '\\'])
                    .replace('\\', "/");
                Ok(Located::Archive(fs, rel))
            }
        }
    }

    /// Turns archive files in a listing into folders, remembering where
    /// each one is.
    fn mark(
        &self,
        dir: &str,
        list: &mut [VfsEntry],
        parent: Option<&Arc<ArchiveFs>>,
        parent_rel: &str,
    ) {
        let mut map = self.found.lock().unwrap_or_else(|e| e.into_inner());
        for e in list.iter_mut() {
            if e.meta.is_dir || e.meta.link_target.is_some() {
                continue;
            }
            let Some(kind) = ArchiveKind::from_name(&e.name, false) else {
                continue;
            };
            let key = self.join(dir, &e.name);
            let source = match parent {
                None => Source::Inner {
                    size: e.meta.size,
                    modified_ms: e.meta.modified_ms,
                },
                Some(p) => Source::Nested {
                    parent: p.clone(),
                    path: crate::tree::join(parent_rel, &e.name),
                },
            };
            map.insert(
                key,
                Arc::new(Found {
                    kind,
                    source,
                    fs: OnceLock::new(),
                }),
            );
            e.meta.is_dir = true;
            e.meta.size = 0;
        }
    }
}

impl Vfs for ExpandFs {
    fn read_dir(&self, dir: &str) -> Result<Vec<VfsEntry>> {
        self.read_dir_with(dir, ListOptions::default())
    }

    fn read_dir_with(&self, dir: &str, opts: ListOptions) -> Result<Vec<VfsEntry>> {
        match self.locate(dir)? {
            Located::Inner(p) => {
                let mut list = self.inner.read_dir_with(&p, opts)?;
                self.mark(dir, &mut list, None, "");
                Ok(list)
            }
            Located::Archive(fs, rel) => {
                let mut list = fs.read_dir(&rel)?;
                self.mark(dir, &mut list, Some(&fs), &rel);
                Ok(list)
            }
        }
    }

    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>> {
        match self.locate(path)? {
            Located::Inner(p) => self.inner.open(&p),
            Located::Archive(fs, rel) => fs.open(&rel),
        }
    }

    fn join(&self, dir: &str, name: &str) -> String {
        self.inner.join(dir, name)
    }

    fn canonical(&self, path: &str) -> Option<String> {
        match self.locate(path).ok()? {
            Located::Inner(p) => self.inner.canonical(&p),
            Located::Archive(..) => None,
        }
    }
}

impl CrcSource for ExpandFs {
    fn stored_crc(&self, path: &str) -> Option<u32> {
        match self.locate(path).ok()? {
            Located::Inner(p) => self.inner_crcs.as_ref()?.stored_crc(&p),
            Located::Archive(fs, rel) => fs.stored_crc(&rel),
        }
    }

    fn covers(&self, path: &str) -> bool {
        match self.locate(path) {
            Ok(Located::Inner(p)) => self.inner_crcs.as_ref().is_some_and(|c| c.covers(&p)),
            _ => true,
        }
    }
}
