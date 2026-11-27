use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::num::NonZero;
use std::panic;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use parking_lot::Mutex;

use crate::{Error, Op};

/// Flushes written data to stable storage, so it survives a crash or a
/// power loss.
///
/// The default methods use the standard library, which issues each
/// platform's strongest sync. Each platform implements `sync_batch` its own
/// way.
pub trait Durability {
    /// Flushes a file's contents, and the metadata needed to read them back.
    ///
    /// # Errors
    ///
    /// Fails if the file system reports a write error.
    fn sync_file(&self, file: &File) -> io::Result<()> {
        file.sync_data()
    }

    /// Flushes a directory's entries, so the files created in, renamed into or
    /// removed from it survive a crash.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be opened or the file system reports a
    /// write error.
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    /// Flushes the contents of many files at once, more cheaply than
    /// syncing each. The files must be on one file system.
    ///
    /// # Errors
    ///
    /// Fails if a file cannot be opened or the file system reports a write
    /// error, naming the file involved.
    fn sync_batch<'f>(&self, files: impl IntoIterator<Item = &'f Path>) -> Result<(), Error>;
}

/// Files written durably as a group, at the cost of one batch sync plus one
/// sync per directory involved.
///
/// [`DurableWrite::stage`] writes each file under a temporary name next to its
/// destination ([`DurableWrite::is_temporary`] recognizes them), and
/// [`DurableWrite::commit`] makes all the files durable with
/// [`Durability::sync_batch`], renames them into place, and syncs their
/// directories. Readers see the previous file or the complete new one, and a
/// crash loses none once `commit` returns. Dropping the write before
/// committing removes its temporary files; `docs/design.md` has the rest.
#[derive(Debug)]
pub struct DurableWrite<'a, P> {
    platform: &'a P,
    staged: Mutex<Staged>,
}

/// The files staged so far.
#[derive(Debug, Default)]
struct Staged {
    /// The files of each layer, in the order the layers are published.
    layers: Vec<Layer>,
    /// Files written in place: synced with the batch, never renamed.
    in_place: Vec<PathBuf>,
}

/// The files of one layer of a [`DurableWrite`].
#[derive(Debug, Default)]
struct Layer {
    /// Each temporary file, and the path it is renamed to on commit.
    files: Vec<(PathBuf, PathBuf)>,
    /// The directories whose entries must be durable before the next layer is
    /// published: the parents of the published files and, for the first layer,
    /// the directories added with [`DurableWrite::sync_dir_with`].
    dirs: BTreeSet<PathBuf>,
}

/// A counter that gives each temporary file of this process a unique
/// name.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

impl<P> DurableWrite<'_, P> {
    /// The prefix of temporary file names, which are `.tmp-<pid>-<n>`.
    const TEMP_PREFIX: &str = ".tmp-";

    /// The fewest renames each thread of [`DurableWrite::commit`] handles. A
    /// smaller batch is renamed on the current thread, since spawning threads
    /// would cost more than it saves.
    const RENAMES_PER_THREAD: usize = 64;

    /// Returns whether `name` is a temporary file's name: a file still being
    /// written, or one left behind by a write that crashed.
    #[must_use]
    pub fn is_temporary(name: &OsStr) -> bool {
        name.as_encoded_bytes()
            .starts_with(Self::TEMP_PREFIX.as_bytes())
    }
}

impl<'a, P: Durability> DurableWrite<'a, P> {
    /// Starts a write on `platform`.
    pub fn new(platform: &'a P) -> Self {
        Self {
            platform,
            staged: Mutex::default(),
        }
    }

    /// Writes `contents` to a temporary file that replaces `path` on
    /// [`DurableWrite::commit`]. Creates any missing parent directories.
    ///
    /// The directories it creates are synced right away, so they stay durable
    /// even if this write is never committed. An existing directory may not be
    /// durable yet, since another write may have just created it, so the parent
    /// of the deepest existing directory is synced too: right away if this
    /// write creates directories in it, otherwise on commit.
    ///
    /// # Errors
    ///
    /// Fails if a directory or the temporary file cannot be created or
    /// written.
    pub fn stage(&self, path: impl Into<PathBuf>, contents: &[u8]) -> Result<(), Error> {
        self.stage_with(path, |file| file.write_all(contents))
    }

    /// Stages `path` like [`DurableWrite::stage`], with the contents `write`
    /// writes through a buffered [`StagedFile`], so the file never has to fit
    /// in memory. If `write` fails, nothing is staged.
    ///
    /// # Errors
    ///
    /// Fails like [`DurableWrite::stage`], or with the error `write` returns.
    pub fn stage_with<E: From<Error>>(
        &self,
        path: impl Into<PathBuf>,
        write: impl FnOnce(&mut StagedFile) -> Result<(), E>,
    ) -> Result<(), E> {
        self.stage_in(0, path, write)
    }

    /// Stages `path` like [`DurableWrite::stage_with`], in `layer`. The file is
    /// published once every file of the earlier layers is durable.
    ///
    /// # Errors
    ///
    /// Fails like [`DurableWrite::stage_with`].
    pub fn stage_in<E: From<Error>>(
        &self,
        layer: usize,
        path: impl Into<PathBuf>,
        write: impl FnOnce(&mut StagedFile) -> Result<(), E>,
    ) -> Result<(), E> {
        let path = path.into();
        let relied_on = self.create_parents(&path)?;
        let temp = Self::parent(&path).join(format!(
            "{}{}-{}",
            Self::TEMP_PREFIX,
            process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        if let Err(error) = StagedFile::write(&temp, write) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        let mut staged = self.staged.lock();
        let layer = staged.layer(layer);
        layer.dirs.insert(Self::parent(&path).to_owned());
        layer.dirs.extend(relied_on);
        layer.files.push((temp, path));
        Ok(())
    }

    /// Makes the entries of `dir` durable with the batch's first layer, as if a
    /// file were staged in it. Use it for a file that another write published,
    /// that this write relies on, and that may not have been synced yet.
    pub fn sync_dir_with(&self, dir: impl Into<PathBuf>) {
        let mut staged = self.staged.lock();
        staged.layer(0).dirs.insert(dir.into());
    }

    /// Adds `path`, a file already written in place, to the batch sync:
    /// [`DurableWrite::commit`] syncs it with the staged files, before renaming
    /// any of them.
    pub fn sync_with(&self, path: impl Into<PathBuf>) {
        let mut staged = self.staged.lock();
        staged.in_place.push(path.into());
    }

    /// Creates the missing parent directories of `path`, outermost first, and
    /// syncs each one's parent right after creating it.
    ///
    /// The deepest existing directory may not be durable yet, so the directory
    /// holding it is synced too: right away, before anything is created in it,
    /// or, when nothing needs creating, returned for the commit to sync.
    /// Returns `None` when nothing is left to sync.
    fn create_parents(&self, path: &Path) -> Result<Option<PathBuf>, Error> {
        let mut missing = Vec::new();
        let mut dir = Self::parent(path);
        while !dir.is_dir() {
            missing.push(dir);
            dir = Self::parent(dir);
        }
        let Some(holder) = Self::holder(dir) else {
            return Ok(None);
        };
        if missing.is_empty() {
            return Ok(Some(holder.to_owned()));
        }
        self.platform
            .sync_dir(holder)
            .map_err(|source| Error::new(Op::Sync, holder, source))?;
        for dir in missing.into_iter().rev() {
            match fs::create_dir(dir) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                result => result.map_err(|source| Error::new(Op::CreateDir, dir, source))?,
            }
            let parent = Self::parent(dir);
            self.platform
                .sync_dir(parent)
                .map_err(|source| Error::new(Op::Sync, parent, source))?;
        }
        Ok(None)
    }

    /// Returns the directory containing `path`, or `.` for a bare file name.
    #[must_use]
    pub fn parent(path: &Path) -> &Path {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    }

    /// Returns the directory holding the entry of the directory `dir`, or
    /// `None` when it can't be named: for `/` and `.`.
    fn holder(dir: &Path) -> Option<&Path> {
        (dir != Path::new(".") && dir.parent().is_some()).then(|| Self::parent(dir))
    }

    /// Syncs every staged file and every file added with
    /// [`DurableWrite::sync_with`]. Then, one layer at a time, renames the
    /// layer's files into place and syncs each directory involved once.
    ///
    /// # Errors
    ///
    /// Fails if the files cannot be synced, and then publishes none of them.
    /// Fails if a file cannot be renamed or a directory cannot be synced, and
    /// then publishes none of the later layers. Files of the failed layer that
    /// were renamed before the failure are visible but may not be durable.
    pub fn commit(mut self) -> Result<(), Error> {
        let staged = self.staged.get_mut();
        let temps = staged
            .layers
            .iter()
            .flat_map(|layer| &layer.files)
            .map(|(temp, _)| temp.as_path());
        self.platform
            .sync_batch(temps.chain(staged.in_place.iter().map(PathBuf::as_path)))?;
        for layer in &mut staged.layers {
            Self::rename_all(&layer.files)?;
            layer.files.clear();
            for dir in &layer.dirs {
                self.platform
                    .sync_dir(dir)
                    .map_err(|source| Error::new(Op::Sync, dir, source))?;
            }
        }
        Ok(())
    }

    /// Renames each temporary file to its path, on several threads when there
    /// are enough files to be worth it.
    fn rename_all(files: &[(PathBuf, PathBuf)]) -> Result<(), Error> {
        let rename = |files: &[(PathBuf, PathBuf)]| {
            files.iter().try_for_each(|(temp, path)| {
                fs::rename(temp, path).map_err(|source| Error::new(Op::Publish, path, source))
            })
        };
        let most = files.len() / Self::RENAMES_PER_THREAD;
        if most <= 1 {
            return rename(files);
        }
        let threads = thread::available_parallelism()
            .map_or(1, NonZero::get)
            .min(most);
        thread::scope(|scope| {
            files
                .chunks(files.len().div_ceil(threads))
                .map(|chunk| scope.spawn(move || rename(chunk)))
                .collect::<Vec<_>>()
                .into_iter()
                .try_for_each(|thread| {
                    thread
                        .join()
                        .unwrap_or_else(|panic| panic::resume_unwind(panic))
                })
        })
    }
}

impl<P> Drop for DurableWrite<'_, P> {
    /// Removes the temporary files that were not published.
    fn drop(&mut self) {
        let staged = self.staged.get_mut();
        for (temp, _) in staged.layers.iter().flat_map(|layer| &layer.files) {
            let _ = fs::remove_file(temp);
        }
    }
}

impl Staged {
    /// Returns the layer numbered `layer`, creating it and any missing layers
    /// before it.
    fn layer(&mut self, layer: usize) -> &mut Layer {
        if self.layers.len() <= layer {
            self.layers.resize_with(layer + 1, Layer::default);
        }
        &mut self.layers[layer]
    }
}

/// A temporary file being written by [`DurableWrite::stage_with`].
#[derive(Debug)]
pub struct StagedFile {
    file: BufWriter<File>,
    temp: PathBuf,
}

impl StagedFile {
    /// Creates `temp`, fills it with `write`, and flushes the buffer.
    fn write<E: From<Error>>(
        temp: &Path,
        write: impl FnOnce(&mut Self) -> Result<(), E>,
    ) -> Result<(), E> {
        let file = File::create(temp).map_err(|source| Error::new(Op::Write, temp, source))?;
        let mut staged = Self {
            file: BufWriter::new(file),
            temp: temp.to_owned(),
        };
        write(&mut staged)?;
        staged
            .file
            .flush()
            .map_err(|source| Error::new(Op::Write, temp, source))?;
        Ok(())
    }

    /// Appends `bytes` to the file.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be written.
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file
            .write_all(bytes)
            .map_err(|source| Error::new(Op::Write, &self.temp, source))
    }
}

#[cfg(test)]
mod tests;
