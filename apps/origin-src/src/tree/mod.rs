//! Comparing trees, merging them, and looking up what they hold.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;

use origin_platform::Platform;
use origin_shared::access::{Level, PathAccess};
use origin_shared::{
    Base, Digest, EncodedObject, EntryKind, EntryName, Inputs, ObjectId, Record, RecordId,
    RepoPath, Tree, TreeEntry, TreeId,
};

use crate::conflict::{Conflicted, Conflicts};
use crate::store::{Batch, Store, StoreError};

/// A file that differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// Where the file is.
    pub path: RepoPath,
    /// The file before, if it existed.
    pub old: Option<TreeEntry>,
    /// The file after, if it exists.
    pub new: Option<TreeEntry>,
}

impl<P: Platform> Store<P> {
    /// Whether a tree operation may read the entry at `path`, in a directory
    /// whose withheld paths are `withheld`: a file it may read, or a directory
    /// it may read something under.
    fn readable(withheld: Option<&PathAccess>, path: &RepoPath, entry: &TreeEntry) -> bool {
        withheld.is_none_or(|access| access.reads_entry(path, entry))
    }

    /// Lists the files that differ between two trees, sorted by path.
    /// Subtrees with equal ids are skipped without being read, so the cost
    /// follows the size of the change.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn diff(&self, old: TreeId, new: TreeId) -> Result<Vec<FileChange>, StoreError> {
        let mut changes = Vec::new();
        self.diff_dir(&RepoPath::root(), Some(old), Some(new), &mut changes)?;
        Ok(changes)
    }

    /// Diffs two directories, `None` standing for one that doesn't exist.
    fn diff_dir(
        &self,
        dir: &RepoPath,
        old: Option<TreeId>,
        new: Option<TreeId>,
        changes: &mut Vec<FileChange>,
    ) -> Result<(), StoreError> {
        if old == new {
            return Ok(());
        }
        let read = |id: Option<TreeId>| id.map(|id| self.get(id)).transpose();
        let (old, new): (Tree, Tree) = (
            read(old)?.unwrap_or_default(),
            read(new)?.unwrap_or_default(),
        );
        let mut names: Vec<&EntryName> =
            old.iter().chain(new.iter()).map(|(name, _)| name).collect();
        names.sort_unstable();
        names.dedup();
        for name in names {
            let (old, new) = (
                old.get(name.as_str()).copied(),
                new.get(name.as_str()).copied(),
            );
            if old == new {
                continue;
            }
            let path = dir.join(name);
            // A name can hold a file on one side and a directory on the
            // other: the file part is a change, the directory part is
            // diffed against an empty tree.
            let old_file = old.and_then(TreeEntry::file);
            let new_file = new.and_then(TreeEntry::file);
            if old_file.is_some() || new_file.is_some() {
                changes.push(FileChange {
                    path: path.clone(),
                    old: old_file,
                    new: new_file,
                });
            }
            let old_tree = old.and_then(TreeEntry::subtree);
            let new_tree = new.and_then(TreeEntry::subtree);
            self.diff_dir(&path, old_tree, new_tree, changes)?;
        }
        Ok(())
    }
}

/// Two trees merged with the changes each made to a common one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
    /// The merged tree: both sides' changes, and `ours` where they conflict.
    pub tree: TreeId,
    /// The trees the merge made, which the store may not have yet.
    pub trees: Vec<EncodedObject>,
    /// The files with conflict markers a [marked](Store::merge_marked)
    /// merge made, which the store may not have yet either.
    pub files: Vec<EncodedObject>,
    /// The paths both sides changed, each its own way, with what the merge
    /// left there.
    pub conflicts: Conflicts,
}

/// What a merge leaves where both sides changed a path, each its own way.
#[derive(Clone, Copy, Debug)]
enum OnConflict<'l> {
    /// `ours`.
    KeepOurs,
    /// Both versions between conflict markers naming the sides with these
    /// labels, see [`Conflicts::mark`].
    Mark([&'l str; 2]),
    /// [`Store::UNKNOWN`], a version neither side can have.
    Unknown,
}

impl<P: Platform> Store<P> {
    /// Merges `ours` and `theirs`, two trees made from `base`, file by file:
    /// a path one side changed takes that side's version, and a path both
    /// changed, each its own way, is a conflict, where the merge keeps
    /// `ours`. A path is a file, so two sides that changed different files
    /// of a directory merge cleanly. Without a base, every path either side
    /// has counts as changed. Subtrees with equal ids are never read, so the
    /// cost follows the size of the changes.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn merge(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
    ) -> Result<Merge, StoreError> {
        self.merge_with(base, ours, theirs, OnConflict::KeepOurs)
    }

    /// Merges like [`Store::merge`], but where both sides changed a text
    /// file, the merged one holds both versions between conflict markers
    /// naming the sides with `labels`, see [`Conflicts::mark`]. Binary files,
    /// and paths a directory is on either side of, keep `ours`.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs, or a file that conflicts, cannot be
    /// read.
    pub fn merge_marked(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
        labels: [&str; 2],
    ) -> Result<Merge, StoreError> {
        self.merge_with(base, ours, theirs, OnConflict::Mark(labels))
    }

    /// The entry a merge of merge bases has where they conflict: an id no
    /// object has, so it equals neither side of a merge from it. A merge
    /// never reads under it either: the bases may have had a directory
    /// there, so any change at or under its path conflicts.
    const UNKNOWN: TreeEntry = TreeEntry {
        kind: EntryKind::File,
        id: ObjectId::from_bytes([0; ObjectId::LEN]),
    };

    /// Merges like [`Store::merge`], but where both sides changed a path,
    /// the merged tree has [`Store::UNKNOWN`]: how merge bases are merged
    /// into one, so a merge from it conflicts there unless both sides
    /// agree.
    ///
    /// # Errors
    ///
    /// Fails if a tree that differs cannot be read.
    pub fn merge_bases(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
    ) -> Result<Merge, StoreError> {
        self.merge_with(base, ours, theirs, OnConflict::Unknown)
    }

    fn merge_with(
        &self,
        base: Option<TreeId>,
        ours: TreeId,
        theirs: TreeId,
        on_conflict: OnConflict<'_>,
    ) -> Result<Merge, StoreError> {
        let mut merge = Merge {
            tree: ours,
            trees: Vec::new(),
            files: Vec::new(),
            conflicts: Conflicts::default(),
        };
        let merged = self.merge_dir(
            &RepoPath::root(),
            base,
            Some(ours),
            Some(theirs),
            on_conflict,
            &mut merge,
        )?;
        merge.tree = if let Some(tree) = merged {
            tree
        } else {
            let empty = Tree::default().encode();
            let id = empty.id().into();
            merge.trees.push(empty);
            id
        };
        merge.trees.sort_unstable_by_key(EncodedObject::id);
        merge.trees.dedup_by_key(|tree| tree.id());
        merge.files.sort_unstable_by_key(EncodedObject::id);
        merge.files.dedup_by_key(|file| file.id());
        Ok(merge)
    }

    /// Merges three versions of the directory `dir`, `None` standing for one
    /// that doesn't exist, returning the merged one, or `None` if it's
    /// empty.
    fn merge_dir(
        &self,
        dir: &RepoPath,
        base: Option<TreeId>,
        ours: Option<TreeId>,
        theirs: Option<TreeId>,
        on_conflict: OnConflict<'_>,
        merge: &mut Merge,
    ) -> Result<Option<TreeId>, StoreError> {
        if ours == theirs || base == theirs {
            return Ok(ours);
        }
        if base == ours {
            return Ok(theirs);
        }
        let read = |id: Option<TreeId>| id.map(|id| self.get(id)).transpose();
        let (base, ours, theirs): (Tree, Tree, Tree) = (
            read(base)?.unwrap_or_default(),
            read(ours)?.unwrap_or_default(),
            read(theirs)?.unwrap_or_default(),
        );
        let mut names: Vec<&EntryName> = [&base, &ours, &theirs]
            .into_iter()
            .flat_map(|tree| tree.iter().map(|(name, _)| name))
            .collect();
        names.sort_unstable();
        names.dedup();
        let mut merged = Tree::default();
        for name in names {
            let [base, ours, theirs] =
                [&base, &ours, &theirs].map(|tree| tree.get(name.as_str()).copied());
            let entry = if ours == theirs || base == theirs {
                ours
            } else if base == ours {
                theirs
            } else if base != Some(Self::UNKNOWN)
                && ours.is_none_or(|ours| ours.kind.is_tree())
                && theirs.is_none_or(|theirs| theirs.kind.is_tree())
            {
                // Both sides changed a directory, or one changed it and the
                // other removed it: merge it file by file, unless what it was
                // is unknown, which would take what one side removed for
                // what the other added.
                let subtree = |entry: Option<TreeEntry>| entry.and_then(TreeEntry::subtree);
                self.merge_dir(
                    &dir.join(name),
                    subtree(base),
                    subtree(ours),
                    subtree(theirs),
                    on_conflict,
                    merge,
                )?
                .map(|tree| TreeEntry {
                    kind: EntryKind::Tree,
                    id: tree.into(),
                })
            } else {
                let written = match on_conflict {
                    OnConflict::KeepOurs => ours,
                    OnConflict::Mark(labels) => self.mark(ours, theirs, labels, merge)?,
                    OnConflict::Unknown => Some(Self::UNKNOWN),
                };
                // Paths come in order, each once.
                merge.conflicts.push(Conflicted {
                    path: dir.join(name),
                    written: written.map(|entry| entry.id),
                });
                written
            };
            if let Some(entry) = entry {
                merged.insert(name.clone(), entry);
            }
        }
        if merged.is_empty() {
            return Ok(None);
        }
        let encoded = merged.encode();
        let id = encoded.id().into();
        merge.trees.push(encoded);
        Ok(Some(id))
    }

    /// The entry of a file both sides changed, `ours` and `theirs`, that
    /// holds both versions between conflict markers: a file on the side
    /// that removed it is empty. Keeps `ours` if either side isn't a
    /// regular file, or is binary or missing from the store.
    fn mark(
        &self,
        ours: Option<TreeEntry>,
        theirs: Option<TreeEntry>,
        labels: [&str; 2],
        merge: &mut Merge,
    ) -> Result<Option<TreeEntry>, StoreError> {
        let content = |entry: Option<TreeEntry>| match entry {
            None => Ok(Some(Vec::new())),
            Some(entry) if matches!(entry.kind, EntryKind::File | EntryKind::Executable) => {
                self.try_blob(entry.id)
            }
            Some(_) => Ok(None),
        };
        let (Some(our_content), Some(their_content)) = (content(ours)?, content(theirs)?) else {
            return Ok(ours);
        };
        let Some(marked) = Conflicts::mark(&our_content, &their_content, labels) else {
            return Ok(ours);
        };
        let file = EncodedObject::blob(&marked);
        let kind = ours.or(theirs).map_or(EntryKind::File, |entry| entry.kind);
        let entry = TreeEntry {
            kind,
            id: file.id(),
        };
        merge.files.push(file);
        Ok(Some(entry))
    }
}

/// Looks up the entries of snapshots by path, reading each tree once,
/// however many snapshots share it.
#[derive(Debug)]
pub struct Snapshots<'a, P> {
    store: &'a Store<P>,
    trees: HashMap<TreeId, Tree>,
}

impl<'a, P: Platform> Snapshots<'a, P> {
    /// Looks up snapshots in `store`.
    pub fn new(store: &'a Store<P>) -> Self {
        Self {
            store,
            trees: HashMap::new(),
        }
    }

    /// What `snapshot` stores at `path`, or `None` if nothing is there. At
    /// the root, that's the snapshot's own tree.
    ///
    /// # Errors
    ///
    /// Fails if a tree on the way cannot be read.
    pub fn entry_at(
        &mut self,
        snapshot: TreeId,
        path: &RepoPath,
    ) -> Result<Option<TreeEntry>, StoreError> {
        let mut entry = TreeEntry {
            kind: EntryKind::Tree,
            id: snapshot.into(),
        };
        for name in path.components() {
            let Some(id) = entry.subtree() else {
                return Ok(None);
            };
            let tree = match self.trees.entry(id) {
                Entry::Occupied(tree) => tree.into_mut(),
                Entry::Vacant(vacant) => vacant.insert(self.store.get(id)?),
            };
            match tree.get(name) {
                Some(&next) => entry = next,
                None => return Ok(None),
            }
        }
        Ok(Some(entry))
    }

    /// The input digest of `snapshot`: one lookup per path of `inputs`.
    ///
    /// # Errors
    ///
    /// Fails if a tree on the way to a path cannot be read.
    pub fn input_digest(
        &mut self,
        snapshot: TreeId,
        inputs: &Inputs,
    ) -> Result<Digest, StoreError> {
        inputs.digest(|path| self.entry_at(snapshot, path))
    }
}

impl<P: Platform> Store<P> {
    /// Returns the snapshot `base` names: an acceptance's state or a revision's
    /// snapshot.
    ///
    /// # Errors
    ///
    /// Fails if the record cannot be read.
    pub fn base_snapshot(&self, base: Base) -> Result<TreeId, StoreError> {
        Ok(match base {
            Base::Acceptance(id) => self.get(id)?.body().state,
            Base::Revision(id) => self.get(id)?.snapshot,
        })
    }

    /// Returns the snapshot of a record: a revision's snapshot, an acceptance's
    /// state, or the snapshot itself. Statements have none.
    ///
    /// # Errors
    ///
    /// Fails if the record cannot be read.
    pub fn snapshot_of(&self, record: RecordId) -> Result<Option<TreeId>, StoreError> {
        Ok(match record {
            RecordId::Snapshot(id) => Some(id),
            RecordId::Revision(id) => Some(self.get(id)?.snapshot),
            RecordId::Acceptance(id) => Some(self.get(id)?.body().state),
            RecordId::Statement(_) => None,
        })
    }
}

/// A durable mark in the store's `fetching/` directory, saying a snapshot is
/// being fetched. A fetch publishes trees before their contents, so the mark
/// keeps an interrupted fetch from being taken for a whole snapshot (see
/// [`Store::holds`]). Dropping it without [`Fetching::done`] leaves the
/// mark; fetching the snapshot again completes it.
#[derive(Debug)]
pub struct Fetching {
    path: PathBuf,
}

impl Fetching {
    /// Removes the mark. Call it once everything fetched is durable. If a crash
    /// loses the removal, the snapshot is just fetched again.
    ///
    /// # Errors
    ///
    /// Fails if the mark cannot be removed.
    pub fn done(self) -> Result<(), StoreError> {
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(StoreError::io(&self.path, error))
            }
            _ => Ok(()),
        }
    }
}

impl<P: Platform> Store<P> {
    /// The directory holding the marks of snapshots being fetched.
    const FETCHING: &str = "fetching";

    fn fetching_path(&self, snapshot: TreeId) -> PathBuf {
        self.root().join(Self::FETCHING).join(snapshot.to_hex())
    }

    /// Returns whether the store holds all of `snapshot`: it has the root, and
    /// no unfinished fetch of it (see [`Fetching`]).
    ///
    /// # Errors
    ///
    /// Fails like [`Store::contains`].
    pub fn holds(&self, snapshot: TreeId) -> Result<bool, StoreError> {
        Ok(self.contains(snapshot.into())? && !self.fetching_path(snapshot).exists())
    }

    /// Returns those of `snapshots` that the store [holds](Store::holds) whole,
    /// in order.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::contains`].
    pub fn held_whole(
        &self,
        snapshots: impl IntoIterator<Item = TreeId>,
    ) -> Result<Vec<TreeId>, StoreError> {
        let mut held = Vec::new();
        for snapshot in snapshots {
            if self.holds(snapshot)? {
                held.push(snapshot);
            }
        }
        Ok(held)
    }

    /// Marks `snapshot` as being fetched. The mark becomes durable before
    /// anything `batch` stages is published, synced together with the batch.
    ///
    /// # Errors
    ///
    /// Fails if the mark, or the directory holding it, cannot be created.
    pub fn fetching(&self, snapshot: TreeId, batch: &Batch<'_, P>) -> Result<Fetching, StoreError> {
        let dir = self.root().join(Self::FETCHING);
        if !dir.is_dir() {
            fs::create_dir_all(&dir).map_err(|source| StoreError::io(&dir, source))?;
            self.sync_dir(self.root())?;
        }
        let path = dir.join(snapshot.to_hex());
        File::create(&path).map_err(|source| StoreError::io(&path, source))?;
        batch.sync_first(&path);
        batch.sync_first(dir);
        Ok(Fetching { path })
    }
}

/// An object of a snapshot and its path, as [`Store::added`] lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// Its path: the root for the snapshot's own tree.
    pub path: RepoPath,
    /// The object and its kind.
    pub entry: TreeEntry,
}

/// The part of a snapshot that [`Store::added`] walks.
struct Scope<'p>(&'p [RepoPath]);

impl Scope<'_> {
    /// Whether `path` and everything under it is in scope.
    fn covers(&self, path: &RepoPath) -> bool {
        self.0.is_empty() || self.0.iter().any(|prefix| path.starts_with(prefix))
    }

    /// Whether `path` is a directory that leads to something in scope.
    fn leads_in(&self, path: &RepoPath) -> bool {
        self.0.iter().any(|prefix| prefix.starts_with(path))
    }
}

impl<P: Platform> Store<P> {
    /// Returns the objects of `snapshot` that none of the snapshots `have` holds
    /// at the same path, parents before children: what someone holding `have`
    /// lacks. With `paths`, only the objects under them, and the trees leading
    /// to them. With `access`, only what it allows reading, and the trees
    /// leading to it; the rest keeps its tree entries, but its content is left
    /// out. Subtrees that a `have` holds unchanged are skipped without being
    /// read, so the cost follows the size of the change. Each object appears
    /// once, at its first path.
    ///
    /// # Errors
    ///
    /// Fails if a tree of `snapshot` or of a `have` cannot be read.
    pub fn added(
        &self,
        snapshot: TreeId,
        have: &[TreeId],
        paths: &[RepoPath],
        access: Option<&PathAccess>,
    ) -> Result<Vec<Placed>, StoreError> {
        let mut added = Vec::new();
        let mut seen = HashSet::new();
        let walk = Walk {
            scope: Scope(paths),
            seen: &mut seen,
            added: &mut added,
        };
        let root = RepoPath::root();
        walk.dir(self, &root, snapshot, have, Self::unsettled(access, &root))?;
        Ok(added)
    }

    /// Returns `access` if reading isn't fully allowed at and under `path`, or
    /// `None` if it is. Walks only check paths while this is some.
    fn unsettled<'a>(access: Option<&'a PathAccess>, path: &RepoPath) -> Option<&'a PathAccess> {
        access.filter(|access| access.settled(path).is_none_or(|level| level < Level::Read))
    }

    /// Returns the first path of `snapshot` whose object is neither in
    /// `supplied` nor at the same path in one of the snapshots `references`, if
    /// any. A revision may only hold content its writer supplied, or content
    /// that a snapshot it refers to holds at the same path. Subtrees a reference
    /// holds unchanged are skipped without being read.
    ///
    /// # Errors
    ///
    /// Fails if a tree of `snapshot` or of a reference cannot be read.
    pub fn unsupplied(
        &self,
        snapshot: TreeId,
        references: &[TreeId],
        supplied: &HashSet<ObjectId>,
    ) -> Result<Option<RepoPath>, StoreError> {
        self.unsupplied_dir(&RepoPath::root(), snapshot, references, supplied)
    }

    fn unsupplied_dir(
        &self,
        dir: &RepoPath,
        tree: TreeId,
        references: &[TreeId],
        supplied: &HashSet<ObjectId>,
    ) -> Result<Option<RepoPath>, StoreError> {
        if references.contains(&tree) {
            return Ok(None);
        }
        if !supplied.contains(&tree.into()) {
            return Ok(Some(dir.clone()));
        }
        let references: Vec<Tree> = references
            .iter()
            .map(|&id| self.get(id))
            .collect::<Result<_, _>>()?;
        for (name, &entry) in &self.get(tree)? {
            let path = dir.join(name);
            let held = references.iter().filter_map(|tree| tree.get(name.as_str()));
            if held.clone().any(|held| *held == entry) {
                continue;
            }
            let unsupplied = match entry.subtree() {
                Some(subtree) => {
                    let held: Vec<_> = held.filter_map(|held| held.subtree()).collect();
                    self.unsupplied_dir(&path, subtree, &held, supplied)?
                }
                None => (!supplied.contains(&entry.id)).then_some(path),
            };
            if unsupplied.is_some() {
                return Ok(unsupplied);
            }
        }
        Ok(None)
    }
}

/// A walk of [`Store::added`].
struct Walk<'w> {
    scope: Scope<'w>,
    seen: &'w mut HashSet<ObjectId>,
    added: &'w mut Vec<Placed>,
}

impl Walk<'_> {
    fn dir<P: Platform>(
        mut self,
        store: &Store<P>,
        dir: &RepoPath,
        tree: TreeId,
        have: &[TreeId],
        access: Option<&PathAccess>,
    ) -> Result<Self, StoreError> {
        if have.contains(&tree) {
            return Ok(self);
        }
        if self.seen.insert(tree.into()) {
            self.added.push(Placed {
                path: dir.clone(),
                entry: TreeEntry {
                    kind: EntryKind::Tree,
                    id: tree.into(),
                },
            });
        }
        let have: Vec<Tree> = have
            .iter()
            .map(|&id| store.get(id))
            .collect::<Result<_, _>>()?;
        for (name, &entry) in &store.get(tree)? {
            let path = dir.join(name);
            let covered = self.scope.covers(&path);
            if !(covered || entry.kind.is_tree() && self.scope.leads_in(&path)) {
                continue;
            }
            if !Store::<P>::readable(access, &path, &entry) {
                continue;
            }
            let held = have.iter().filter_map(|tree| tree.get(name.as_str()));
            if held.clone().any(|held| *held == entry) {
                continue;
            }
            match entry.subtree() {
                Some(subtree) => {
                    let held: Vec<_> = held.filter_map(|held| held.subtree()).collect();
                    let access = Store::<P>::unsettled(access, &path);
                    self = self.dir(store, &path, subtree, &held, access)?;
                }
                None if self.seen.insert(entry.id) => self.added.push(Placed { path, entry }),
                None => {}
            }
        }
        Ok(self)
    }
}

#[cfg(test)]
pub(crate) mod tests;
