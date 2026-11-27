use std::fs::File;
use std::io;
use std::path::Path;

use rustix::io::FdFlags;
use rustix::process::{Pid, Signal};

/// Identifies and stops a command's processes.
///
/// A command started in its own process group
/// (`CommandExt::process_group(0)`) can be stopped together with whatever it
/// started and left running, because those processes join its group. Files
/// the processes inherit open keep a lock held while any of them lives.
///
/// The default methods use POSIX's `kill` and `fcntl`, which every supported
/// platform has.
pub trait Processes {
    /// Whether [`Processes::group_holds`] can see the files a group holds.
    /// Where it can't, it always returns `false`, so what a command leaves
    /// running is never killed.
    const SEES_HELD_FILES: bool = true;

    /// Returns whether a process of the group `id` holds the file at `path`
    /// open. This tells a group that still runs from a group that ended and
    /// whose number another group took since. Returns `false` where there is no
    /// reliable way to tell, so the group is left running rather than risk
    /// killing another one.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be inspected, or the processes cannot be listed.
    fn group_holds(&self, id: u32, path: &Path) -> io::Result<bool>;

    /// Kills every process of the group `id`, if any is left.
    ///
    /// # Errors
    ///
    /// Fails if `id` doesn't name a group of its own, as 0 and 1 don't, or if
    /// the group's processes may not be signalled.
    fn kill_group(&self, id: u32) -> io::Result<()> {
        let pid = i32::try_from(id)
            .ok()
            .filter(|&id| id > 1)
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a process group"))?;
        match rustix::process::kill_process_group(pid, Signal::KILL) {
            Err(rustix::io::Errno::SRCH) => Ok(()),
            killed => Ok(killed?),
        }
    }

    /// Makes commands started from now on inherit `file` open, and pass it on to
    /// the processes they start, unless they close it. A lock on the file then
    /// stays held while any of them lives. Commands that other threads start
    /// meanwhile inherit it too.
    ///
    /// # Errors
    ///
    /// Fails if the file's descriptor cannot be changed.
    fn inherit(&self, file: &File) -> io::Result<()> {
        let flags = rustix::io::fcntl_getfd(file)?;
        Ok(rustix::io::fcntl_setfd(file, flags - FdFlags::CLOEXEC)?)
    }
}
