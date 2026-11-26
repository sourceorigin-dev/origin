//! Pack files: many small objects stored together in a few large files, so
//! a store of a million statements holds a handful of files instead of a
//! million.
//!
//! `objects/packs/` holds:
//!
//! - `<id>.pack`: [`Pack::MAGIC`], then each object as the `u32` length of
//!   its compressed encoding followed by the encoding, compressed as in a
//!   loose file. A pack can be read on its own, from start to end.
//! - `<id>.idx`: [`Pack::INDEX_MAGIC`], the pack's object ids in ascending
//!   order, then for each object a `u64` offset and a `u32` length of its
//!   compressed encoding in the pack, and a `u64` time in seconds since the
//!   Unix epoch: when this node stored the object (the modification time of
//!   its loose file). The pack's id is the hash of its object ids; opening
//!   the pack checks it.
//! - `manifest`: the ids of the packs in use, one hex id per line, largest
//!   first. Replacing this file adds and retires packs in one step.
//! - `lock`: held by the compactor, so only one runs at a time.
//!
//! Integers are little endian.
//!
//! [`Store::compact`] gathers the small loose objects into a new pack, and
//! merges into it every pack smaller than twice what the new pack holds so
//! far. A store thus keeps a logarithmic number of packs, and each object is
//! rewritten a logarithmic number of times. Compaction writes the pack and
//! its index durably, then the manifest, and only then removes the loose
//! files and the merged packs. A crash loses nothing: the next compaction
//! removes the files the manifest does not name.
//!
//! [`Store::compact_if_due`] compacts once enough loose objects pile up. A
//! long-running process calls it as it writes.
//!
//! Readers look in the packs they loaded last, and reload the manifest when
//! an object is not there. An object that is neither loose nor in the
//! current manifest's packs is not in the store. An open pack stays readable
//! after it is retired, so readers also reload the manifest every few
//! seconds to close the packs it no longer names.

use std::array;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use origin_platform::{LockMode, LockedFile, Platform, ReadAt};
use origin_shared::{Digest, ObjectId};
use parking_lot::{Mutex, RwLock};

use super::{Store, StoreError};

/// The packs a store loaded last, shared by its clones.
#[derive(Default)]
pub(super) struct Packs {
    manifest: RwLock<Manifest>,
    /// When the manifest was last read.
    read_at: Mutex<Option<Instant>>,
}

/// A manifest and the packs it names, open.
#[derive(Default)]
struct Manifest {
    /// The manifest's contents, or `None` until it's first read.
    bytes: Option<Vec<u8>>,
    packs: Vec<Arc<Pack>>,
}

impl Packs {
    /// How long a store goes by the manifest it read before its next lookup
    /// in the packs reads it again, so that a long-running process lets go
    /// of the packs another process's compaction retired.
    const RELOAD_AFTER: Duration = Duration::from_secs(10);

    /// The pack holding `id` among those loaded, and where it is.
    pub(super) fn find(&self, id: ObjectId) -> Option<(Arc<Pack>, Location)> {
        let manifest = self.manifest.read();
        manifest
            .packs
            .iter()
            .find_map(|pack| Some((Arc::clone(pack), pack.find(id)?)))
    }

    fn loaded(&self) -> Vec<Arc<Pack>> {
        let manifest = self.manifest.read();
        manifest.packs.clone()
    }

    /// Whether the manifest is due to be read again.
    fn stale(&self) -> bool {
        let read_at = self.read_at.lock();
        read_at.is_none_or(|at| at.elapsed() >= Self::RELOAD_AFTER)
    }
}

impl fmt::Debug for Packs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.loaded()).finish()
    }
}

/// One pack and its index, open for reading.
pub(super) struct Pack {
    id: Digest,
    /// The pack file's path, for error messages.
    path: PathBuf,
    file: File,
    /// The index file's contents.
    index: Vec<u8>,
}

/// Where an object's compressed encoding is in a pack, and when this node
/// stored the object. Packing keeps that time so retention counts from the
/// first write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Location {
    offset: u64,
    len: u32,
    /// Seconds since the Unix epoch.
    stored: u64,
}

impl Location {
    /// The length of an encoded location.
    const LEN: usize = 8 + 4 + 8;

    fn encode(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.stored.to_le_bytes());
    }

    fn decode(bytes: &[u8; Self::LEN]) -> Self {
        Self {
            offset: u64::from_le_bytes(array::from_fn(|i| bytes[i])),
            len: u32::from_le_bytes(array::from_fn(|i| bytes[8 + i])),
            stored: u64::from_le_bytes(array::from_fn(|i| bytes[12 + i])),
        }
    }

    /// Returns when this node stored the object.
    pub(super) fn stored(self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.stored)
    }
}

impl Pack {
    /// How a pack file starts: its format and version.
    const MAGIC: [u8; 8] = *b"opack\0\0\x01";

    /// How an index file starts: its format and version.
    const INDEX_MAGIC: [u8; 8] = *b"oindex\0\x01";

    /// The bytes an object takes in an index.
    const ENTRY: usize = ObjectId::LEN + Location::LEN;

    /// The path of the pack `id`'s file with `extension`, `pack` or `idx`.
    fn file_path(dir: &Path, id: Digest, extension: &str) -> PathBuf {
        dir.join(format!("{id}.{extension}"))
    }

    /// The id of a pack holding `ids`, which are sorted.
    fn id_of(ids: &[u8]) -> Digest {
        Digest::of(ids)
    }

    /// Opens the pack `id` in `dir`, or returns `None` if its files are
    /// gone.
    fn open(platform: &impl ReadAt, dir: &Path, id: Digest) -> Result<Option<Self>, StoreError> {
        let index_path = Self::file_path(dir, id, "idx");
        let path = Self::file_path(dir, id, "pack");
        let (index, file) = match fs::read(&index_path).and_then(|index| {
            let file = File::open(&path)?;
            Ok((index, file))
        }) {
            Ok(opened) => opened,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::io(&index_path, source)),
        };
        let pack = Self {
            id,
            path,
            file,
            index,
        };
        if !pack.index_is_valid() {
            return Err(StoreError::Malformed(index_path));
        }
        let mut magic = [0; Self::MAGIC.len()];
        platform
            .read_exact_at(&pack.file, &mut magic, 0)
            .map_err(|source| StoreError::io(&pack.path, source))?;
        if magic != Self::MAGIC {
            return Err(StoreError::Malformed(pack.path));
        }
        Ok(Some(pack))
    }

    /// Whether the index has the right shape, ascending ids, and the ids
    /// the pack's id says.
    fn index_is_valid(&self) -> bool {
        let Some(tables) = self.index.strip_prefix(&Self::INDEX_MAGIC) else {
            return false;
        };
        tables.len().is_multiple_of(Self::ENTRY)
            && self.ids().is_sorted_by(|a, b| a < b)
            && Self::id_of(self.ids().as_flattened()) == self.id
    }

    /// How many objects the pack holds.
    fn len(&self) -> usize {
        (self.index.len() - Self::INDEX_MAGIC.len()) / Self::ENTRY
    }

    fn ids(&self) -> &[[u8; ObjectId::LEN]] {
        let start = Self::INDEX_MAGIC.len();
        self.index[start..start + self.len() * ObjectId::LEN]
            .as_chunks()
            .0
    }

    fn locations(&self) -> &[[u8; Location::LEN]] {
        let start = Self::INDEX_MAGIC.len() + self.len() * ObjectId::LEN;
        self.index[start..].as_chunks().0
    }

    /// Where the pack holds `id`, if it does.
    fn find(&self, id: ObjectId) -> Option<Location> {
        let index = self.ids().binary_search(id.as_bytes()).ok()?;
        Some(Location::decode(&self.locations()[index]))
    }

    /// Every object of the pack, in ascending order.
    fn entries(&self) -> impl Iterator<Item = (ObjectId, Location)> {
        self.ids()
            .iter()
            .zip(self.locations())
            .map(|(id, location)| (ObjectId::from_bytes(*id), Location::decode(location)))
    }

    /// Reads the compressed encoding at `location`.
    pub(super) fn read(
        &self,
        platform: &impl ReadAt,
        location: Location,
    ) -> Result<Vec<u8>, StoreError> {
        let mut bytes = vec![0; location.len as usize];
        platform
            .read_exact_at(&self.file, &mut bytes, location.offset)
            .map_err(|source| StoreError::io(&self.path, source))?;
        Ok(bytes)
    }

    /// The pack file's path.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for Pack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pack")
            .field("id", &self.id)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

/// Where an object written to a new pack comes from.
#[derive(Clone, Copy)]
enum Source {
    /// Its loose file.
    Loose,
    /// The merged pack at this position, at a location.
    Packed(usize, Location),
}

/// What a compaction did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compaction {
    /// Loose objects moved into the new pack.
    pub packed: usize,
    /// Packs merged into it.
    pub merged: usize,
    /// Loose objects removed because a pack already held them, left by a
    /// writer that didn't know of the pack yet, or by a crash.
    pub duplicates: usize,
}

impl<P: Platform> Store<P> {
    /// The largest loose file compaction packs. Larger objects are mostly
    /// file contents, which a pack merge would copy over and over.
    const PACKED_MAX: u64 = 16 * 1024;

    /// How many small loose objects make a compaction due, see
    /// [`Store::compact_if_due`]: about as many as git lets pile up.
    const COMPACT_AFTER: usize = 8 * 1024;

    /// How many objects a compaction reads at once, from every core, before
    /// appending them to the pack.
    const READ_CHUNK: usize = 16 * 1024;

    /// The packs directory's files besides the packs.
    const MANIFEST: &str = "manifest";
    const LOCK: &str = "lock";

    pub(super) fn packs_dir(&self) -> PathBuf {
        self.root.join("objects/packs")
    }

    /// The pack holding `id`, reloading the manifest if no pack loaded
    /// holds it, or if it was read too long ago.
    pub(super) fn find_packed(
        &self,
        id: ObjectId,
    ) -> Result<Option<(Arc<Pack>, Location)>, StoreError> {
        if self.packs.stale() {
            self.reload_packs()?;
        }
        if let Some(found) = self.packs.find(id) {
            return Ok(Some(found));
        }
        self.reload_packs()?;
        Ok(self.packs.find(id))
    }

    /// The id of every packed object, as of the current manifest.
    pub(super) fn packed_ids(&self) -> Result<Vec<ObjectId>, StoreError> {
        self.reload_packs()?;
        Ok(self
            .packs
            .loaded()
            .iter()
            .flat_map(|pack| pack.entries().map(|(id, _)| id))
            .collect())
    }

    /// Loads the manifest's packs, unless they're already loaded.
    pub(super) fn reload_packs(&self) -> Result<(), StoreError> {
        let dir = self.packs_dir();
        let path = dir.join(Self::MANIFEST);
        let mut missing_from = None;
        loop {
            *self.packs.read_at.lock() = Some(Instant::now());
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(source) => return Err(StoreError::io(&path, source)),
            };
            let current = |manifest: &Manifest| manifest.bytes.as_ref() == Some(&bytes);
            if current(&self.packs.manifest.read()) {
                return Ok(());
            }
            let mut manifest = self.packs.manifest.write();
            if current(&manifest) {
                return Ok(());
            }
            if let Some(packs) = self.open_packs(&dir, &bytes, &manifest.packs)? {
                *manifest = Manifest {
                    bytes: Some(bytes),
                    packs,
                };
                return Ok(());
            }
            // A compactor retires packs only after publishing a manifest
            // without them, so if the manifest didn't change, the pack is
            // lost.
            if missing_from.as_ref() == Some(&bytes) {
                return Err(StoreError::Malformed(path));
            }
            missing_from = Some(bytes);
        }
    }

    /// Opens the packs a manifest names, reusing those already open, or
    /// returns `None` if one is gone.
    fn open_packs(
        &self,
        dir: &Path,
        manifest: &[u8],
        open: &[Arc<Pack>],
    ) -> Result<Option<Vec<Arc<Pack>>>, StoreError> {
        let malformed = || StoreError::Malformed(dir.join(Self::MANIFEST));
        let mut packs = Vec::new();
        for line in str::from_utf8(manifest).map_err(|_| malformed())?.lines() {
            let id: Digest = line.parse().map_err(|_| malformed())?;
            let pack = match open.iter().find(|pack| pack.id == id) {
                Some(pack) => Arc::clone(pack),
                None => match Pack::open(&self.platform, dir, id)? {
                    Some(pack) => Arc::new(pack),
                    None => return Ok(None),
                },
            };
            packs.push(pack);
        }
        Ok(Some(packs))
    }

    /// Moves the small loose objects into a new pack, merging packs into it
    /// as the [module documentation](self) describes. Readers and writers
    /// may use the store meanwhile, from any process, so a compaction can
    /// run in the background.
    ///
    /// Returns `None` without doing anything if another compaction is
    /// running.
    ///
    /// # Errors
    ///
    /// Fails if the store's files cannot be read, written or removed, or a
    /// pack is malformed. Whatever the failure, no object is lost.
    pub fn compact(&self) -> Result<Option<Compaction>, StoreError> {
        let Some(_lock) = self.lock_compactor()? else {
            return Ok(None);
        };
        self.reload_packs()?;
        self.compact_locked()
    }

    /// Compacts like [`Store::compact`] if the store holds more than
    /// [`Store::COMPACT_AFTER`] small loose objects, estimated from one
    /// shard of the 256, as git estimates its loose objects. Cheap enough to
    /// call after every few writes: it lists that one shard.
    ///
    /// Returns `None` if the compaction isn't due, or another is running.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::compact`].
    pub fn compact_if_due(&self) -> Result<Option<Compaction>, StoreError> {
        let Some(_lock) = self.lock_compactor()? else {
            return Ok(None);
        };
        self.reload_packs()?;
        let (sampled, _) = self.small_loose(&self.packs.loaded(), &[0])?;
        if sampled.len() * 256 <= Self::COMPACT_AFTER {
            return Ok(None);
        }
        self.compact_locked()
    }

    /// Takes the compactor's lock, or returns `None` if another compactor
    /// holds it.
    fn lock_compactor(&self) -> Result<Option<LockedFile>, StoreError> {
        let lock = self.packs_dir().join(Self::LOCK);
        Ok(LockedFile::try_open(
            &self.platform,
            &lock,
            LockMode::Exclusive,
        )?)
    }

    /// [`Store::compact`], holding the lock, with the packs reloaded.
    fn compact_locked(&self) -> Result<Option<Compaction>, StoreError> {
        let dir = self.packs_dir();
        let packs = self.packs.loaded();
        Self::remove_unlisted(&dir, &packs)?;
        let shards: Vec<u8> = (0..=u8::MAX).collect();
        let (loose, duplicates) = self.small_loose(&packs, &shards)?;
        let mut compaction = Compaction {
            duplicates,
            ..Compaction::default()
        };
        if loose.is_empty() {
            return Ok(Some(compaction));
        }

        // Merge the packs smaller than twice the new pack, smallest first.
        let mut by_len = packs;
        by_len.sort_by_key(|pack| pack.len());
        let mut len = loose.len();
        let merging = by_len
            .iter()
            .take_while(|pack| {
                let merge = pack.len() < 2 * len;
                len += if merge { pack.len() } else { 0 };
                merge
            })
            .count();
        let (merged, kept) = by_len.split_at(merging);

        let mut sources: Vec<_> = loose.iter().map(|&id| (id, Source::Loose)).collect();
        for (position, pack) in merged.iter().enumerate() {
            sources.extend(
                pack.entries()
                    .map(|(id, location)| (id, Source::Packed(position, location))),
            );
        }
        sources.sort_unstable_by_key(|&(id, _)| id);
        sources.dedup_by_key(|&mut (id, _)| id);
        self.replace_packs(&dir, kept, merged, &sources)?;

        // Everything below is published in the new pack. A crash before
        // these files are gone leaves duplicates for the next compaction.
        Self::on_every_core(&loose, |ids| {
            ids.iter()
                .try_for_each(|&id| Self::remove_file(&self.path(id)))
        })
        .into_iter()
        .collect::<Result<(), _>>()?;
        compaction.packed = loose.len();
        compaction.merged = merged.len();
        Ok(Some(compaction))
    }

    /// Removes `ids` from the packs: every pack that holds one is rewritten,
    /// without them, into a single new pack. Returns the bytes of compressed
    /// encodings that freed. Waits for any running compaction.
    ///
    /// # Errors
    ///
    /// Fails like [`Store::compact`].
    pub(super) fn remove_packed(&self, ids: &HashSet<ObjectId>) -> Result<u64, StoreError> {
        let dir = self.packs_dir();
        let _lock = LockedFile::open(&self.platform, &dir.join(Self::LOCK), LockMode::Exclusive)?;
        self.reload_packs()?;
        let (rewritten, kept): (Vec<_>, Vec<_>) = self
            .packs
            .loaded()
            .into_iter()
            .partition(|pack| pack.entries().any(|(id, _)| ids.contains(&id)));
        if rewritten.is_empty() {
            return Ok(0);
        }
        let mut freed = 0;
        let mut sources = Vec::new();
        for (position, pack) in rewritten.iter().enumerate() {
            for (id, location) in pack.entries() {
                if ids.contains(&id) {
                    freed += u64::from(location.len);
                } else {
                    sources.push((id, Source::Packed(position, location)));
                }
            }
        }
        sources.sort_unstable_by_key(|&(id, _)| id);
        sources.dedup_by_key(|&mut (id, _)| id);
        self.replace_packs(&dir, &kept, &rewritten, &sources)?;
        Ok(freed)
    }

    /// Publishes a manifest naming `kept` and a new pack of `sources` sorted by
    /// id (if there are any), then removes the files of `retired`. The caller
    /// must hold the compactor's lock.
    fn replace_packs(
        &self,
        dir: &Path,
        kept: &[Arc<Pack>],
        retired: &[Arc<Pack>],
        sources: &[(ObjectId, Source)],
    ) -> Result<(), StoreError> {
        let mut manifest: Vec<_> = kept.iter().map(|pack| (pack.len(), pack.id)).collect();
        if !sources.is_empty() {
            manifest.push((sources.len(), self.write_pack(dir, sources, retired)?));
        }
        manifest.sort_by_key(|&(len, _)| std::cmp::Reverse(len));
        let manifest: String = manifest.iter().map(|(_, id)| id.to_hex() + "\n").collect();
        let write = self.durable_write();
        write.stage(dir.join(Self::MANIFEST), manifest.as_bytes())?;
        write.commit()?;
        self.reload_packs()?;
        // A crash before these files are gone leaves them for the next
        // compaction.
        for pack in retired {
            for extension in ["idx", "pack"] {
                Self::remove_file(&Pack::file_path(dir, pack.id, extension))?;
            }
        }
        Ok(())
    }

    /// Writes a pack of `sources`, sorted by id, and its index, durably,
    /// returning its id.
    fn write_pack(
        &self,
        dir: &Path,
        sources: &[(ObjectId, Source)],
        merged: &[Arc<Pack>],
    ) -> Result<Digest, StoreError> {
        let mut index = Vec::with_capacity(Pack::INDEX_MAGIC.len() + sources.len() * Pack::ENTRY);
        index.extend_from_slice(&Pack::INDEX_MAGIC);
        for (id, _) in sources {
            index.extend_from_slice(id.as_bytes());
        }
        let id = Pack::id_of(&index[Pack::INDEX_MAGIC.len()..]);

        let write = self.durable_write();
        write.stage_with(Pack::file_path(dir, id, "pack"), |file| {
            file.write_all(&Pack::MAGIC)?;
            let mut offset = Pack::MAGIC.len() as u64;
            for chunk in sources.chunks(Self::READ_CHUNK) {
                let read = Self::on_every_core(chunk, |sources| {
                    sources
                        .iter()
                        .map(|&(id, source)| self.read_source(id, source, merged))
                        .collect::<Result<Vec<_>, _>>()
                });
                for read in read {
                    for (len, stored, bytes) in read? {
                        file.write_all(&len.to_le_bytes())?;
                        file.write_all(&bytes)?;
                        offset += 4;
                        Location {
                            offset,
                            len,
                            stored,
                        }
                        .encode(&mut index);
                        offset += u64::from(len);
                    }
                }
            }
            Ok::<_, StoreError>(())
        })?;
        write.stage(Pack::file_path(dir, id, "idx"), &index)?;
        write.commit()?;
        Ok(id)
    }

    /// Reads the compressed encoding of `id` from `source`. Returns it with its
    /// length and when this node stored it, in seconds since the Unix epoch.
    fn read_source(
        &self,
        id: ObjectId,
        source: Source,
        merged: &[Arc<Pack>],
    ) -> Result<(u32, u64, Vec<u8>), StoreError> {
        match source {
            Source::Loose => {
                let path = self.path(id);
                let io_error = |source| StoreError::io(&path, source);
                let mut file = File::open(&path).map_err(io_error)?;
                let modified = file.metadata().and_then(|metadata| metadata.modified());
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map_err(io_error)?;
                let len = u32::try_from(bytes.len())
                    .map_err(|_| io_error(io::ErrorKind::FileTooLarge.into()))?;
                // A time before the epoch counts as the epoch.
                let stored = modified
                    .map_err(io_error)?
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs());
                Ok((len, stored, bytes))
            }
            Source::Packed(pack, location) => Ok((
                location.len,
                location.stored,
                merged[pack].read(&self.platform, location)?,
            )),
        }
    }

    /// Removes the files of the packs directory that no pack of `packs`
    /// owns: those of compactions that crashed before publishing, and of
    /// packs retired by compactions that crashed before removing them. Only
    /// the compactor writes there, so it must hold the lock.
    fn remove_unlisted(dir: &Path, packs: &[Arc<Pack>]) -> Result<(), StoreError> {
        let mut keep = vec![Self::MANIFEST.to_owned(), Self::LOCK.to_owned()];
        for pack in packs {
            keep.extend(["idx", "pack"].map(|extension| format!("{}.{extension}", pack.id)));
        }
        for entry in fs::read_dir(dir).map_err(|source| StoreError::io(dir, source))? {
            let entry = entry.map_err(|source| StoreError::io(dir, source))?;
            if !keep.iter().any(|name| entry.file_name() == name.as_str()) {
                Self::remove_file(&entry.path())?;
            }
        }
        Ok(())
    }

    /// The small loose objects of `shards` that no pack of `packs` holds,
    /// and how many loose duplicates of packed objects it removed.
    fn small_loose(
        &self,
        packs: &[Arc<Pack>],
        shards: &[u8],
    ) -> Result<(Vec<ObjectId>, usize), StoreError> {
        let found = Self::on_every_core(shards, |shards| {
            let mut small = Vec::new();
            let mut duplicates = 0;
            for shard in shards {
                let dir = self.loose().join(format!("{shard:02x}"));
                let io_error = |source| StoreError::io(&dir, source);
                for entry in fs::read_dir(&dir).map_err(io_error)? {
                    let entry = entry.map_err(io_error)?;
                    // Skips temporary files, and any a user left there.
                    let Some(Ok(id)) = entry
                        .file_name()
                        .to_str()
                        .map(|name| format!("{shard:02x}{name}").parse::<ObjectId>())
                    else {
                        continue;
                    };
                    if packs.iter().any(|pack| pack.find(id).is_some()) {
                        Self::remove_file(&entry.path())?;
                        duplicates += 1;
                    } else if entry.metadata().map_err(io_error)?.len() <= Self::PACKED_MAX {
                        small.push(id);
                    }
                }
            }
            Ok::<_, StoreError>((small, duplicates))
        });
        found
            .into_iter()
            .try_fold((Vec::new(), 0), |(mut small, duplicates), found| {
                let (more, more_duplicates) = found?;
                small.extend(more);
                Ok((small, duplicates + more_duplicates))
            })
    }

    /// Removes a file, unless it's already gone.
    fn remove_file(path: &Path) -> Result<(), StoreError> {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(StoreError::io(path, error))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
