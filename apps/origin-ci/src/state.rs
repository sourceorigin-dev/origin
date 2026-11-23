//! The state `origin-ci` keeps between runs for each repository it checks.
//! It lives in `$XDG_STATE_HOME/origin-ci/<key>/`, or
//! `~/.local/state/origin-ci/<key>/`, where the key is the start of the
//! hash of the repository's URL or path:
//!
//! ```text
//! worker.lock     held by the one worker of the repository
//! runs/<n>/       a checkout a check runs in, removed after it
//! runs/<n>.lock   held while it runs, so a crashed run's is swept away
//! runs/<n>.held   held by the commands run there while any process of
//!                 theirs lives, even past a crash
//! runs/<n>.group  the process group of the last command started there
//! ```
//!
//! Each command runs in its own process group, killed when the command
//! ends or its crashed run's checkout is swept, while a process of it holds
//! `<n>.held`. One that escapes keeps the next command in the checkout
//! waiting [`Checkout::GRACE`], then failing (see `docs/design.md`).

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use origin_platform::{LockMode, LockedFile, Native, Processes};
use origin_shared::object::DecodeError;
use origin_shared::pack::Pack;
use origin_shared::{Digest, EncodedObject, EntryKind, Object, ObjectId, Tree, TreeId};
use origin_src::remote::RemoteUrl;

/// The state kept for one repository.
#[derive(Debug)]
pub struct State {
    dir: PathBuf,
    platform: Native,
}

/// A snapshot the remote sent: its trees, and the blobs of the files the
/// caller reads. [`State::checkout`] writes it out as many times as checks
/// need.
#[derive(Debug)]
pub struct Snapshot {
    root: TreeId,
    objects: HashMap<ObjectId, EncodedObject>,
}

impl Snapshot {
    /// Creates the snapshot whose root tree is `root`, from `pack`, which
    /// holds its trees and blobs.
    #[must_use]
    pub fn new(root: TreeId, pack: Pack) -> Self {
        let objects = pack
            .objects
            .into_iter()
            .map(|object| (object.id(), object))
            .collect();
        Self { root, objects }
    }

    /// The snapshot's root tree.
    #[must_use]
    pub fn root(&self) -> TreeId {
        self.root
    }

    /// Returns the content of the file `name` at the snapshot's root, or
    /// `None` if there is no such file or the remote withholds it.
    ///
    /// # Errors
    ///
    /// Fails if the root tree is missing or malformed.
    pub fn file(&self, name: &str) -> Result<Option<Vec<u8>>, StateError> {
        let file = self
            .tree(self.root.into())?
            .iter()
            .find_map(|(entry, &at)| (entry.as_str() == name).then_some(at)?.file());
        Ok(
            file.and_then(|file| match self.objects.get(&file.id)?.decode() {
                Ok(Object::Blob(content)) => Some(content),
                _ => None,
            }),
        )
    }

    /// Writes the tree `id` into `dir`, creating `dir`.
    fn write_tree(&self, dir: &Path, id: ObjectId) -> Result<(), StateError> {
        let tree = self.tree(id)?;
        fs::create_dir(dir).map_err(|source| StateError::io(dir, source))?;
        for (name, entry) in &tree {
            let path = dir.join(name.as_str());
            if entry.kind == EntryKind::Tree {
                self.write_tree(&path, entry.id)?;
                continue;
            }
            let Some(Ok(Object::Blob(content))) =
                self.objects.get(&entry.id).map(EncodedObject::decode)
            else {
                continue;
            };
            let written = match entry.kind {
                EntryKind::Symlink => {
                    let target = String::from_utf8_lossy(&content).into_owned();
                    std::os::unix::fs::symlink(target, &path)
                }
                EntryKind::Executable => fs::write(&path, &content)
                    .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(0o755))),
                EntryKind::File | EntryKind::Tree => fs::write(&path, &content),
            };
            written.map_err(|source| StateError::io(&path, source))?;
        }
        Ok(())
    }

    /// Returns the tree `id`.
    fn tree(&self, id: ObjectId) -> Result<Tree, StateError> {
        let object = self.objects.get(&id).ok_or(StateError::MissingTree(id))?;
        object
            .decode_as()
            .map_err(|source| StateError::Decode { id, source })
    }
}

/// A directory holding a snapshot's files, for checks to run in. Removed
/// when dropped.
#[derive(Debug)]
pub struct Checkout {
    dir: PathBuf,
    lock: PathBuf,
    platform: Native,
    _held: LockedFile,
}

/// A counter that gives each checkout of this process its own number.
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

impl State {
    /// How many hex digits of the hash of a repository's URL name its
    /// directory.
    const KEY_LEN: usize = 16;

    /// Opens the state of the repository at `url` in the user's state
    /// directory, sweeping away the checkouts of crashed runs.
    ///
    /// # Errors
    ///
    /// Fails if there is no state directory, or like [`State::new`].
    pub fn open(url: &RemoteUrl) -> Result<Self, StateError> {
        let base = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".local/state")))
            .ok_or(StateError::NoDir)?;
        Self::new(&base.join("origin-ci"), url)
    }

    /// Opens the state of the repository at `url` in `base`, sweeping away the
    /// checkouts of crashed runs.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created or swept.
    pub fn new(base: &Path, url: &RemoteUrl) -> Result<Self, StateError> {
        let mut key = Digest::of(url.to_string().as_bytes()).to_hex();
        key.truncate(Self::KEY_LEN);
        let state = Self {
            dir: base.join(key),
            platform: Native::default(),
        };
        let runs = state.runs();
        fs::create_dir_all(&runs).map_err(|source| StateError::io(&runs, source))?;
        state.sweep()?;
        Ok(state)
    }

    /// Returns the directory holding the repository's state.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn runs(&self) -> PathBuf {
        self.dir.join("runs")
    }

    /// Removes the checkouts no run holds, which are those of crashed runs,
    /// and kills what their commands left running.
    fn sweep(&self) -> Result<(), StateError> {
        let runs = self.runs();
        let entries = fs::read_dir(&runs).map_err(|source| StateError::io(&runs, source))?;
        for entry in entries {
            let lock = entry
                .map_err(|source| StateError::io(&runs, source))?
                .path();
            if lock.extension().is_none_or(|extension| extension != "lock") {
                continue;
            }
            if let Some(held) = LockedFile::try_open(&self.platform, &lock, LockMode::Exclusive)? {
                let checkout = Checkout {
                    dir: lock.with_extension(""),
                    lock,
                    platform: self.platform,
                    _held: held,
                };
                if let Some(group) = checkout.group()? {
                    checkout.stop(group)?;
                }
                checkout.remove_all()?;
            }
        }
        Ok(())
    }

    /// Takes the repository's worker lock, so only one worker answers its
    /// events. Returns `None` if another worker holds it.
    ///
    /// # Errors
    ///
    /// Fails if the lock file cannot be opened.
    pub fn lock_worker(&self) -> Result<Option<LockedFile>, StateError> {
        let path = self.dir.join("worker.lock");
        Ok(LockedFile::try_open(
            &self.platform,
            &path,
            LockMode::Exclusive,
        )?)
    }

    /// Checks `snapshot` out into a new directory. Files the snapshot lacks
    /// because the remote withholds them are left out.
    ///
    /// # Errors
    ///
    /// Fails if a tree is missing or malformed, or the files cannot be
    /// written.
    pub fn checkout(&self, snapshot: &Snapshot) -> Result<Checkout, StateError> {
        let name = format!(
            "{}-{}",
            process::id(),
            NEXT_RUN.fetch_add(1, Ordering::Relaxed)
        );
        let dir = self.runs().join(&name);
        let lock = self.runs().join(format!("{name}.lock"));
        let held = LockedFile::open(&self.platform, &lock, LockMode::Exclusive)?;
        Checkout::remove(&dir)?;
        let checkout = Checkout {
            dir,
            lock,
            platform: self.platform,
            _held: held,
        };
        snapshot.write_tree(&checkout.dir, snapshot.root.into())?;
        Ok(checkout)
    }
}

impl Checkout {
    /// How long the processes [`Checkout::stop`] killed may take to die and
    /// release what they held: the lock, and the command's output.
    pub const GRACE: Duration = Duration::from_secs(5);

    /// How often [`Checkout::spawn`] tries the lock again while it waits.
    const RETRY: Duration = Duration::from_millis(10);

    /// Returns the directory holding the files.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Returns the path of the file that stays locked while any process of the
    /// commands run in the checkout lives.
    fn held(&self) -> PathBuf {
        self.lock.with_extension("held")
    }

    /// Returns the path of the file naming the process group of the last
    /// command started.
    fn group_file(&self) -> PathBuf {
        self.lock.with_extension("group")
    }

    /// Returns the process group of the last command started in the checkout,
    /// if the group file names one.
    fn group(&self) -> Result<Option<u32>, StateError> {
        let path = self.group_file();
        match fs::read_to_string(&path) {
            Ok(group) => Ok(group.trim_end().parse().ok()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(StateError::io(&path, source)),
        }
    }

    /// Starts `command` in the checkout, in its own process group. The
    /// command inherits the checkout's locked `held` file, so its processes
    /// hold the lock while any lives. Commands must start one at a time, or
    /// others would inherit it too; it first waits up to [`Checkout::GRACE`]
    /// for the processes of the last command to release it.
    ///
    /// # Errors
    ///
    /// Fails if a process of an earlier command still holds the lock after the
    /// wait, if the lock or the group file cannot be written, or if the command cannot be started.
    pub fn spawn(&self, mut command: Command) -> Result<Child, StateError> {
        let path = self.held();
        let deadline = Instant::now() + Self::GRACE;
        let held = loop {
            if let Some(held) = LockedFile::try_open(&self.platform, &path, LockMode::Exclusive)? {
                break held;
            }
            if Instant::now() >= deadline {
                return Err(StateError::Held(path));
            }
            thread::sleep(Self::RETRY);
        };
        self.platform
            .inherit(&held)
            .map_err(|source| StateError::io(&path, source))?;
        let group = self.group_file();
        Self::remove(&group)?;
        let child = command
            .current_dir(&self.dir)
            .process_group(0)
            .spawn()
            .map_err(|source| StateError::io(&self.dir, source))?;
        drop(held);
        fs::write(&group, format!("{}\n", child.id()))
            .map_err(|source| StateError::io(&group, source))?;
        Ok(child)
    }

    /// Kills what is left of the process group `group` of a command started in
    /// the checkout, but only if one of its processes still holds the lock. So
    /// nothing the command started outlives it, and no other group is killed
    /// (see the module docs).
    ///
    /// # Errors
    ///
    /// Fails if the lock cannot be tried, the processes cannot be listed, or
    /// the group cannot be killed.
    pub fn stop(&self, group: u32) -> Result<(), StateError> {
        let held = self.held();
        if !held.exists()
            || LockedFile::try_open(&self.platform, &held, LockMode::Exclusive)?.is_some()
        {
            return Ok(());
        }
        let io = |source| StateError::io(&held, source);
        if self.platform.group_holds(group, &held).map_err(io)? {
            self.platform.kill_group(group).map_err(io)?;
        }
        Ok(())
    }

    /// Removes the checkout's directory and files, the lock last.
    fn remove_all(&self) -> Result<(), StateError> {
        for path in [&self.dir, &self.held(), &self.group_file(), &self.lock] {
            Self::remove(path)?;
        }
        Ok(())
    }

    /// Removes a checkout's directory, or one of its files, if it exists.
    fn remove(path: &Path) -> Result<(), StateError> {
        let removed = if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        match removed {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                Err(StateError::io(path, error))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
impl Checkout {
    /// Returns a checkout of an empty snapshot, in a new state kept in
    /// `base`.
    pub(crate) fn empty(base: &Path) -> Self {
        use origin_shared::Record;

        let url = RemoteUrl::parse(base.join("repo").to_str().unwrap()).unwrap();
        let state = State::new(base, &url).unwrap();
        let root = Tree::default().encode();
        let snapshot = Snapshot::new(
            root.id().into(),
            Pack {
                objects: vec![root],
            },
        );
        state.checkout(&snapshot).unwrap()
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        let _ = self.remove_all();
    }
}

/// A failure to keep state.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Neither `$XDG_STATE_HOME` nor `$HOME` is set.
    #[error("no state directory: set $XDG_STATE_HOME or $HOME")]
    NoDir,
    /// A file cannot be read or written.
    #[error("cannot access {}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        source: io::Error,
    },
    /// A process an earlier command started escaped being stopped and still
    /// holds the checkout's lock.
    #[error("a process an earlier command started still holds {}", .0.display())]
    Held(PathBuf),
    /// A file holds something unexpected.
    #[error("{} is malformed", .0.display())]
    Malformed(PathBuf),
    /// A snapshot's tree is missing from what the remote sent.
    #[error("the remote sent no tree {0}")]
    MissingTree(ObjectId),
    /// A tree the remote sent is malformed.
    #[error("tree {id} is malformed")]
    Decode {
        /// The tree.
        id: ObjectId,
        /// What is wrong.
        source: DecodeError,
    },
    /// A lock or durable write failed.
    #[error(transparent)]
    Platform(#[from] origin_platform::Error),
}

impl StateError {
    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_owned(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    /// A command's group ended, but a process it started in its own session
    /// still holds the lock, and the group's number now names another group.
    /// Stopping the command must not kill that group.
    #[test]
    fn stopping_a_command_never_kills_a_group_that_took_its_number() {
        let base = tempfile::tempdir().unwrap();
        let checkout = Checkout::empty(base.path());
        let mut escaped = Command::new("sleep");
        escaped.arg("60");
        let mut escaped = checkout.spawn(escaped).unwrap();
        let mut other = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        checkout.stop(other.id()).unwrap();
        // wall-clock: time for a kill to land.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            other.try_wait().unwrap().is_none(),
            "another group was killed"
        );
        for child in [&mut escaped, &mut other] {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    /// A process of a command that escaped being stopped still holds the lock.
    /// The next command fails once the grace period is over, rather than wait
    /// for it.
    #[test]
    fn a_command_never_waits_past_the_grace_for_one_that_escaped() {
        let base = tempfile::tempdir().unwrap();
        let checkout = Checkout::empty(base.path());
        let mut escaped = Command::new("sleep");
        escaped.arg("60");
        let mut escaped = checkout.spawn(escaped).unwrap();
        let next = checkout.spawn(Command::new("true"));
        escaped.kill().unwrap();
        escaped.wait().unwrap();
        assert!(matches!(next, Err(StateError::Held(_))), "{next:?}");
    }
}
