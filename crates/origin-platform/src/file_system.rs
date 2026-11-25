use std::io;
use std::path::{Path, PathBuf};

use crate::{Error, Op};

/// Tells local file systems from network ones. Origin refuses to write to
/// network file systems: they can lose locks and reorder or drop syncs, so
/// a crash or a concurrent writer can corrupt a repository on one.
///
/// Folders of sync services such as Dropbox and iCloud Drive are on a local
/// file system, so a repository there is accepted, even though the service
/// may replace files behind origin's back.
pub trait FileSystem {
    /// Returns the kind of file system holding `path`, which must exist.
    ///
    /// # Errors
    ///
    /// Fails if `path` cannot be inspected.
    fn file_system(&self, path: &Path) -> io::Result<FsKind>;

    /// Checks that the repository at `path` may be opened for writing. Refuses
    /// network file systems, and accepts the ones the platform cannot identify.
    ///
    /// # Errors
    ///
    /// Fails with [`WritableError::Network`] on a network file system, and
    /// [`WritableError::Io`] if `path` cannot be inspected.
    fn check_writable(&self, path: &Path) -> Result<(), WritableError> {
        let kind = self
            .file_system(path)
            .map_err(|source| Error::new(Op::Inspect, path, source))?;
        match kind {
            FsKind::Network(name) => Err(WritableError::Network {
                path: path.to_owned(),
                name,
            }),
            FsKind::Local | FsKind::Unknown(_) => Ok(()),
        }
    }
}

/// What a file system is, as far as the platform can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    /// A disk attached to this machine.
    Local,
    /// A file system served by another machine, with its name, such as
    /// `NFS`.
    Network(&'static str),
    /// A file system that may be local or networked, with its name. FUSE is
    /// one: it serves sshfs as well as local disks.
    Unknown(&'static str),
}

/// Why a repository cannot be opened for writing.
#[derive(Debug, thiserror::Error)]
pub enum WritableError {
    /// The repository is on a network file system.
    #[error(
        "{} is on {name}, a network file system; origin writes only to local \
         disks, so clone the repository to one and use this one as a remote",
        path.display()
    )]
    Network {
        /// The repository's path.
        path: PathBuf,
        /// The file system's name.
        name: &'static str,
    },
    /// The file system holding the repository cannot be inspected.
    #[error(transparent)]
    Io(#[from] Error),
}
