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

/// Flushes written data to stable storage, so it survives a crash or a power
/// loss.
///
/// The default methods use the standard library, which issues each
/// platform's strongest sync. Each platform syncs batches its own way.
pub trait Durability {
    /// Flushes a file's contents, and the metadata needed to read them back.
    ///
    /// # Errors
    ///
    /// Fails if the file system reports a write error.
    fn sync_file(&self, file: &File) -> io::Result<()> {
        file.sync_data()
    }

    /// Flushes a directory's entries, making the files created in, renamed
    /// into or removed from it durable.
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

/// Files written durably as a group, at the cost of one batch sync and one
/// sync per directory involved.
///
/// [`DurableWrite::stage`] writes each file under a temporary name in its
/// destination's directory, which [`DurableWrite::is_temporary`] recognizes.
/// [`DurableWrite::commit`] then makes them all durable with
/// [`Durability::sync_batch`], renames them into place from several threads
/// and syncs their directories. Readers see either the previous file or the
/// complete new one, and once `commit` returns a crash loses none of them.
/// Dropping the write before committing removes its temporary files; a crash
/// leaves them behind, so code that lists a directory written this way skips
/// them, and removes the stale ones.
///
/// Staging beside the destination keeps each rename within one directory,
/// so writes to different directories never contend on a shared one.
///
/// [`DurableWrite::sync_with`] adds a file written in place, such as an
/// append-only log, to the batch sync, so it becomes durable no later than
/// the batch is published, at no extra sync.
///
/// Files can be staged from several threads at once.
#[derive(Debug)]
pub struct DurableWrite<'a, P> {
    platform: &'a P,
    staged: Mutex<Staged>,
}

/// The files staged so far.
#[derive(Debug, Default)]
struct Staged {
    /// Each temporary file and the path it's published to.
    files: Vec<(PathBuf, PathBuf)>,
    /// The directories whose entries must be durable: the published files'
    /// parents, and those added with [`DurableWrite::sync_dir_with`].
    dirs: BTreeSet<PathBuf>,
    /// Files written in place, synced with the batch but never renamed.
    in_place: Vec<PathBuf>,
}

/// Distinguishes the temporary files of one process.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

impl<P> DurableWrite<'_, P> {
    /// How temporary file names start: `.tmp-<pid>-<n>`.
    const TEMP_PREFIX: &str = ".tmp-";

    /// The fewest renames worth a thread of their own in
    /// [`DurableWrite::commit`]: below that, spawning costs more than it
    /// saves.
    const RENAMES_PER_THREAD: usize = 64;

    /// Whether `name` is the name of a temporary file: one being written, or
    /// left behind by a write that crashed.
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

    /// Writes `contents` as the next version of `path`, and creates its
    /// missing parent directories. The file appears at `path` on
    /// [`DurableWrite::commit`], replacing any file there.
    ///
    /// Each directory created is durable at once, whatever becomes of the
    /// write, so a later write into it never relies on this one committing.
    /// A directory found already there may not be: another write may have
    /// just created it, or died before syncing its parent. So the write
    /// syncs the parent of the deepest one it found too: at once if it
    /// creates directories in it, with the batch otherwise.
    ///
    /// # Errors
    ///
    /// Fails if a directory or the temporary file cannot be created or
    /// written.
    pub fn stage(&self, path: impl Into<PathBuf>, contents: &[u8]) -> Result<(), Error> {
        self.stage_with(path, |file| file.write_all(contents))
    }

    /// Stages `path` like [`DurableWrite::stage`], with the contents `write`
    /// writes through a buffered [`StagedFile`], so a file never needs to
    /// fit in memory. If `write` fails, nothing is staged.
    ///
    /// # Errors
    ///
    /// Fails like [`DurableWrite::stage`], or with the error of `write`.
    pub fn stage_with<E: From<Error>>(
        &self,
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
        staged.dirs.insert(Self::parent(&path).to_owned());
        staged.dirs.extend(relied_on);
        staged.files.push((temp, path));
        Ok(())
    }

    /// Makes the entries of `dir` durable with the batch, as for a file
    /// staged in it: for a file another write published, which this one
    /// relies on, and may not have synced yet.
    pub fn sync_dir_with(&self, dir: impl Into<PathBuf>) {
        let mut staged = self.staged.lock();
        staged.dirs.insert(dir.into());
    }

    /// Makes `path`, a file already written in place, durable with the
    /// batch: [`DurableWrite::commit`] syncs it with the staged files, before
    /// publishing any of them.
    pub fn sync_with(&self, path: impl Into<PathBuf>) {
        let mut staged = self.staged.lock();
        staged.in_place.push(path.into());
    }

    /// Creates the missing parent directories of `path`, outermost first,
    /// syncing the directory each is created in right after creating it.
    ///
    /// The deepest directory that already exists may not be durable yet, if
    /// another write created it, so its parent is synced too: before
    /// creating anything in it, so a directory this write creates is only
    /// ever reachable through durable entries, or else returned, for the
    /// batch to sync.
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

    /// The directory containing `path`, which is `.` for a bare file name.
    #[must_use]
    pub fn parent(path: &Path) -> &Path {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    }

    /// The directory holding the entry of the directory `dir`, unless it
    /// can't be named: for `/`, or `.`.
    fn holder(dir: &Path) -> Option<&Path> {
        (dir != Path::new(".") && dir.parent().is_some()).then(|| Self::parent(dir))
    }

    /// Syncs every staged file and every file added with
    /// [`DurableWrite::sync_with`], renames the staged files into place, then
    /// syncs each directory involved once.
    ///
    /// # Errors
    ///
    /// Fails if the files cannot be synced, in which case none is published,
    /// or if a file cannot be renamed or a directory cannot be synced. Files
    /// renamed before such a failure are visible but may not be durable.
    pub fn commit(mut self) -> Result<(), Error> {
        let staged = self.staged.get_mut();
        let temps = staged.files.iter().map(|(temp, _)| temp.as_path());
        self.platform
            .sync_batch(temps.chain(staged.in_place.iter().map(PathBuf::as_path)))?;
        Self::rename_all(&staged.files)?;
        staged.files.clear();
        for dir in &staged.dirs {
            self.platform
                .sync_dir(dir)
                .map_err(|source| Error::new(Op::Sync, dir, source))?;
        }
        Ok(())
    }

    /// Renames each temporary file to its path, spread over the cores. Each
    /// rename locks its directory, and a batch usually spans many.
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
        for (temp, _) in &staged.files {
            let _ = fs::remove_file(temp);
        }
    }
}

/// A temporary file being written by [`DurableWrite::stage_with`].
#[derive(Debug)]
pub struct StagedFile {
    file: BufWriter<File>,
    temp: PathBuf,
}

impl StagedFile {
    /// Creates `temp` and fills it with `write`.
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
