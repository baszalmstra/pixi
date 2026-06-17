//! In-memory path, node, directory, file, and query index.
//!
//! Public VFS APIs use `Path`/`PathBuf`, but the hot paths use compact ids.
//! `PathId` interns the path string once, `NodeId` identifies either a file or
//! directory, and `FileId`/`DirId` address type-specific records. The index is
//! intentionally append-heavy: file records can be marked non-existing, but ids
//! remain stable so active glob aggregates can keep references to matched files.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{GlobQueryKey, QueryState};

/// Interned path id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PathId(pub(crate) usize);

/// Type-erased node id. A node is either a directory or a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct NodeId(pub(crate) usize);

/// Directory record id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DirId(pub(crate) usize);

/// File record id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct FileId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum VfsEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Whether a directory's `children` map is authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListingState {
    /// The directory is known to exist, but its complete child list has not
    /// been read. Glob walks may create directories in this state while only
    /// indexing matched files.
    Unknown,
    /// The directory's immediate children were read completely from the
    /// backend and can satisfy `read_dir` without another backend call.
    Clean,
}

/// Normalized directory entry ready to commit into the index.
#[derive(Clone, Debug)]
pub(crate) struct EntryInfo {
    pub(crate) path: PathBuf,
    pub(crate) kind: VfsEntryKind,
    pub(crate) size: Option<u64>,
    pub(crate) modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Dir(DirId),
    File(FileId),
}

#[derive(Debug)]
pub(crate) struct DirRecord {
    pub(crate) path: PathId,
    pub(crate) parent: Option<DirId>,
    /// Child name to node id. This map is only complete when
    /// `listing_state == ListingState::Clean`.
    pub(crate) children: BTreeMap<String, NodeId>,
    pub(crate) listing_state: ListingState,
}

#[derive(Debug)]
pub(crate) struct FileRecord {
    pub(crate) path: PathId,
    pub(crate) parent: Option<DirId>,
    pub(crate) kind: VfsEntryKind,
    pub(crate) size: Option<u64>,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) exists: bool,
    pub(crate) dirty: bool,
}

#[derive(Default)]
pub(crate) struct Index {
    paths: Vec<PathBuf>,
    path_ids: HashMap<PathBuf, PathId>,
    path_to_node: HashMap<PathId, NodeId>,
    nodes: Vec<NodeKind>,
    dirs: Vec<DirRecord>,
    files: Vec<FileRecord>,
    /// Active glob aggregates keyed by `(root, pattern)`.
    queries: HashMap<GlobQueryKey, QueryState>,
}

impl Index {
    pub(crate) fn path_count(&self) -> usize {
        self.paths.len()
    }

    pub(crate) fn dir_count(&self) -> usize {
        self.dirs.len()
    }

    pub(crate) fn file_count(&self) -> usize {
        self.files.iter().filter(|file| file.exists).count()
    }

    pub(crate) fn query_count(&self) -> usize {
        self.queries.len()
    }

    pub(crate) fn path(&self, id: PathId) -> &Path {
        &self.paths[id.0]
    }

    pub(crate) fn file(&self, id: FileId) -> &FileRecord {
        &self.files[id.0]
    }

    pub(crate) fn file_mut(&mut self, id: FileId) -> &mut FileRecord {
        &mut self.files[id.0]
    }

    pub(crate) fn dir(&self, id: DirId) -> &DirRecord {
        &self.dirs[id.0]
    }

    pub(crate) fn file_path(&self, id: FileId) -> &Path {
        self.path(self.file(id).path)
    }

    pub(crate) fn dir_path(&self, id: DirId) -> &Path {
        self.path(self.dir(id).path)
    }

    pub(crate) fn node_dir(&self, node: NodeId) -> Option<DirId> {
        match self.nodes[node.0] {
            NodeKind::Dir(dir) => Some(dir),
            NodeKind::File(_) => None,
        }
    }

    pub(crate) fn node_file(&self, node: NodeId) -> Option<FileId> {
        match self.nodes[node.0] {
            NodeKind::Dir(_) => None,
            NodeKind::File(file) => Some(file),
        }
    }

    pub(crate) fn node_path(&self, node: NodeId) -> &Path {
        match self.nodes[node.0] {
            NodeKind::Dir(dir) => self.dir_path(dir),
            NodeKind::File(file) => self.file_path(file),
        }
    }

    pub(crate) fn node_kind(&self, node: NodeId) -> VfsEntryKind {
        match self.nodes[node.0] {
            NodeKind::Dir(_) => VfsEntryKind::Directory,
            NodeKind::File(file) => self.files[file.0].kind,
        }
    }

    pub(crate) fn file_id_for_path(&self, path: &Path) -> Option<FileId> {
        let path_id = self.path_ids.get(path)?;
        let node = self.path_to_node.get(path_id)?;
        self.node_file(*node)
    }

    pub(crate) fn dir_id_for_path(&self, path: &Path) -> Option<DirId> {
        let path_id = self.path_ids.get(path)?;
        let node = self.path_to_node.get(path_id)?;
        self.node_dir(*node)
    }

    pub(crate) fn store_query(&mut self, key: GlobQueryKey, query: QueryState) {
        self.queries.insert(key, query);
    }

    pub(crate) fn query(&self, key: &GlobQueryKey) -> Option<&QueryState> {
        self.queries.get(key)
    }

    pub(crate) fn query_mut(&mut self, key: &GlobQueryKey) -> Option<&mut QueryState> {
        self.queries.get_mut(key)
    }

    pub(crate) fn remove_query(&mut self, key: &GlobQueryKey) -> bool {
        self.queries.remove(key).is_some()
    }

    pub(crate) fn query_keys_for_path(&self, path: &Path) -> Vec<GlobQueryKey> {
        // A file event can only affect queries rooted at an ancestor of the
        // changed path. Pattern matching is checked later for each candidate.
        self.queries
            .keys()
            .filter(|key| path.starts_with(&key.root))
            .cloned()
            .collect()
    }

    /// Return a stable id for `path`, inserting it if needed.
    pub(crate) fn intern_path(&mut self, path: PathBuf) -> PathId {
        if let Some(id) = self.path_ids.get(&path) {
            return *id;
        }
        let id = PathId(self.paths.len());
        self.paths.push(path.clone());
        self.path_ids.insert(path, id);
        id
    }

    /// Ensure a directory node exists.
    ///
    /// Parent links may be discovered after the directory itself; when a later
    /// call provides a parent for a previously parentless directory, the link is
    /// filled in without changing the directory id.
    pub(crate) fn ensure_dir(&mut self, path: PathBuf, parent: Option<DirId>) -> DirId {
        let path_id = self.intern_path(path.clone());
        if let Some(node) = self.path_to_node.get(&path_id).copied()
            && let Some(dir) = self.node_dir(node)
        {
            if self.dirs[dir.0].parent.is_none() {
                self.dirs[dir.0].parent = parent;
            }
            return dir;
        }

        let dir = DirId(self.dirs.len());
        let node = NodeId(self.nodes.len());
        self.nodes.push(NodeKind::Dir(dir));
        self.path_to_node.insert(path_id, node);
        self.dirs.push(DirRecord {
            path: path_id,
            parent,
            children: BTreeMap::new(),
            listing_state: ListingState::Unknown,
        });
        if let Some(parent) = parent {
            let name = file_name_string(&path);
            self.dirs[parent.0].children.insert(name, node);
        }
        dir
    }

    /// Insert or update file metadata and link it into its parent if known.
    pub(crate) fn upsert_file_metadata(
        &mut self,
        path: PathBuf,
        kind: VfsEntryKind,
        size: Option<u64>,
        modified: Option<SystemTime>,
        exists: bool,
    ) -> FileId {
        let parent = path
            .parent()
            .map(|parent| self.ensure_dir(parent.to_path_buf(), None));
        let path_id = self.intern_path(path.clone());
        if let Some(node) = self.path_to_node.get(&path_id).copied()
            && let Some(file) = self.node_file(node)
        {
            let record = &mut self.files[file.0];
            record.parent = parent;
            record.kind = kind;
            record.size = size;
            record.modified = modified;
            record.exists = exists;
            record.dirty = false;
            if let Some(parent) = parent {
                self.dirs[parent.0]
                    .children
                    .insert(file_name_string(&path), node);
            }
            return file;
        }

        let file = FileId(self.files.len());
        let node = NodeId(self.nodes.len());
        self.nodes.push(NodeKind::File(file));
        self.path_to_node.insert(path_id, node);
        self.files.push(FileRecord {
            path: path_id,
            parent,
            kind,
            size,
            modified,
            exists,
            dirty: false,
        });
        if let Some(parent) = parent {
            self.dirs[parent.0]
                .children
                .insert(file_name_string(&path), node);
        }
        file
    }

    /// Ensure a directory discovered by a query exists without marking its
    /// listing complete.
    pub(crate) fn upsert_query_dir(&mut self, path: PathBuf) -> DirId {
        let parent = path
            .parent()
            .and_then(|parent| self.dir_id_for_path(parent));
        self.ensure_dir(path, parent)
    }

    /// Replace a directory's children with a complete backend listing.
    pub(crate) fn upsert_directory_listing(
        &mut self,
        path: PathBuf,
        entries: Vec<EntryInfo>,
    ) -> DirId {
        let parent = path
            .parent()
            .and_then(|parent| self.dir_id_for_path(parent));
        let dir = self.ensure_dir(path, parent);
        let mut children = BTreeMap::new();
        for entry in entries {
            let node = match entry.kind {
                VfsEntryKind::Directory => {
                    let child = self.ensure_dir(entry.path.clone(), Some(dir));
                    self.path_to_node[&self.dirs[child.0].path]
                }
                kind => {
                    let file = self.upsert_file_metadata(
                        entry.path.clone(),
                        kind,
                        entry.size,
                        entry.modified,
                        true,
                    );
                    self.path_to_node[&self.files[file.0].path]
                }
            };
            children.insert(file_name_string(&entry.path), node);
        }
        self.dirs[dir.0].children = children;
        self.dirs[dir.0].listing_state = ListingState::Clean;
        dir
    }

    /// Insert or update a file found by a glob query.
    ///
    /// Unlike `upsert_file_metadata`, this does not add the file to the
    /// parent directory's `children` map. Query walks intentionally index only
    /// matched files on the cold path; adding them to a partial child map would
    /// make an unknown listing look more complete than it is and adds avoidable
    /// commit cost.
    pub(crate) fn upsert_query_file(
        &mut self,
        path: PathBuf,
        kind: VfsEntryKind,
        size: Option<u64>,
        modified: Option<SystemTime>,
    ) -> FileId {
        let parent = path
            .parent()
            .and_then(|parent| self.dir_id_for_path(parent));
        let path_id = self.intern_path(path);
        if let Some(node) = self.path_to_node.get(&path_id).copied()
            && let Some(file) = self.node_file(node)
        {
            let record = &mut self.files[file.0];
            record.parent = parent;
            record.kind = kind;
            record.size = size;
            record.modified = modified;
            record.exists = true;
            record.dirty = false;
            return file;
        }

        let file = FileId(self.files.len());
        let node = NodeId(self.nodes.len());
        self.nodes.push(NodeKind::File(file));
        self.path_to_node.insert(path_id, node);
        self.files.push(FileRecord {
            path: path_id,
            parent,
            kind,
            size,
            modified,
            exists: true,
            dirty: false,
        });
        file
    }

    /// Mark a directory listing as unknown so the next read repairs it from the
    /// backend.
    pub(crate) fn mark_directory_listing_unknown(&mut self, path: &Path) -> bool {
        let Some(dir) = self.dir_id_for_path(path) else {
            return false;
        };
        self.dirs[dir.0].listing_state = ListingState::Unknown;
        true
    }

    /// Mark a file as missing and unlink it from a known parent listing.
    ///
    /// The file record is retained so existing query aggregates can remove or
    /// replace it by id.
    pub(crate) fn remove_file_path(&mut self, path: &Path) -> Option<FileId> {
        let file = self.file_id_for_path(path)?;
        self.files[file.0].exists = false;
        self.files[file.0].dirty = false;
        if let Some(parent) = self.files[file.0].parent {
            self.dirs[parent.0].children.remove(&file_name_string(path));
        }
        Some(file)
    }

    /// Return children only when the directory listing is known complete.
    pub(crate) fn clean_dir_children(&self, dir: DirId) -> Option<Vec<NodeId>> {
        if self.dirs[dir.0].listing_state != ListingState::Clean {
            return None;
        }
        Some(self.dirs[dir.0].children.values().copied().collect())
    }

    pub(crate) fn clean_dir_entries(&self, dir: DirId) -> Option<Vec<(PathBuf, VfsEntryKind)>> {
        let children = self.clean_dir_children(dir)?;
        Some(
            children
                .into_iter()
                .map(|node| (self.node_path(node).to_path_buf(), self.node_kind(node)))
                .collect(),
        )
    }
}

fn file_name_string(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
