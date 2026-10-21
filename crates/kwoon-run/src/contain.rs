//! Host-side reads of files a run produced, under the descriptor-relative containment
//! rule of plan D4: every directory component is opened one at a time with
//! `O_DIRECTORY | O_NOFOLLOW`, every file relative to a verified descriptor with
//! `O_NOFOLLOW`, and nothing is opened until `fstatat` without following links has shown
//! it is a directory or a singly linked regular file.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// Why a path was refused. No content is kept, and the path names only components
/// kwoon fixed: a name the run chose shows as `*`, since run output can hold secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub path: String,
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Symlink,
    SpecialFile,
    MultiplyLinked,
    NotADirectory,
    TooLarge,
    /// The object changed between the check and the open.
    Replaced,
    Io(i32),
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.reason {
            Reason::Symlink => "symbolic link".to_string(),
            Reason::SpecialFile => "not a regular file or directory".to_string(),
            Reason::MultiplyLinked => "regular file with more than one link".to_string(),
            Reason::NotADirectory => "not a directory".to_string(),
            Reason::TooLarge => "larger than the read limit".to_string(),
            Reason::Replaced => "replaced while being opened".to_string(),
            Reason::Io(errno) => io::Error::from_raw_os_error(errno).to_string(),
        };
        write!(f, "{}: {reason}", self.path)
    }
}

/// A directory opened under the containment rule.
pub struct Dir {
    fd: OwnedFd,
    /// The run-relative path as diagnostics show it.
    path: String,
}

enum Kind {
    Dir,
    File,
}

impl Dir {
    /// Opens `name` inside `parent`. The parent is trusted (kwoon created it); `name`
    /// itself is held to the rule, so a run-home root that was swapped for a link is
    /// refused.
    pub fn open_root(parent: &Path, name: &str) -> Result<Dir, Rejected> {
        let reject = |reason| Rejected {
            path: name.to_string(),
            reason,
        };
        let parent_c = CString::new(parent.as_os_str().as_bytes())
            .map_err(|_| reject(Reason::Io(libc::EINVAL)))?;
        // SAFETY: parent_c is a valid NUL-terminated string.
        let raw = unsafe {
            libc::open(
                parent_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(reject(Reason::Io(errno())));
        }
        // SAFETY: raw is a descriptor we just opened and own.
        let parent = Dir {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            path: String::new(),
        };
        parent.open_dir(OsStr::new(name), name)
    }

    /// Opens the directory `name`, which diagnostics show as `shown`.
    pub fn open_dir(&self, name: &OsStr, shown: &str) -> Result<Dir, Rejected> {
        let fd = self.open_checked(name, shown, Kind::Dir)?;
        Ok(Dir {
            fd,
            path: self.child_path(shown),
        })
    }

    /// Reads a singly linked regular file of at most `limit` bytes; diagnostics show it
    /// as `shown`.
    pub fn read_file(&self, name: &OsStr, shown: &str, limit: u64) -> Result<Vec<u8>, Rejected> {
        let path = self.child_path(shown);
        let fd = self.open_checked(name, shown, Kind::File)?;
        let mut bytes = Vec::new();
        File::from(fd)
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| Rejected {
                path: path.clone(),
                reason: Reason::Io(err.raw_os_error().unwrap_or(libc::EIO)),
            })?;
        if bytes.len() as u64 > limit {
            return Err(Rejected {
                path,
                reason: Reason::TooLarge,
            });
        }
        Ok(bytes)
    }

    /// The names in this directory, without `.` and `..`, sorted.
    pub fn entries(&self) -> Result<Vec<OsString>, Rejected> {
        let reject = |reason| Rejected {
            path: self.path.clone(),
            reason,
        };
        // SAFETY: dup of a descriptor we own; fdopendir takes ownership of the copy.
        let copy = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            return Err(reject(Reason::Io(errno())));
        }
        // SAFETY: copy is a valid directory descriptor.
        let stream = unsafe { libc::fdopendir(copy) };
        if stream.is_null() {
            let err = errno();
            // SAFETY: fdopendir failed, so copy is still ours to close.
            unsafe { libc::close(copy) };
            return Err(reject(Reason::Io(err)));
        }
        // SAFETY: rewinding a freshly opened stream.
        unsafe { libc::rewinddir(stream) };
        let mut names = Vec::new();
        loop {
            // SAFETY: stream is a valid DIR*; readdir returns null at the end.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            // SAFETY: d_name is NUL-terminated within the entry readdir returned.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name.to_vec()));
            }
        }
        // SAFETY: closes the stream and its descriptor copy.
        unsafe { libc::closedir(stream) };
        names.sort();
        Ok(names)
    }

    fn child_path(&self, shown: &str) -> String {
        if self.path.is_empty() {
            shown.to_string()
        } else {
            format!("{}/{shown}", self.path)
        }
    }

    fn open_checked(&self, name: &OsStr, shown: &str, kind: Kind) -> Result<OwnedFd, Rejected> {
        let path = self.child_path(shown);
        let reject = |reason| Rejected {
            path: path.clone(),
            reason,
        };
        if name.is_empty() || name.as_bytes().contains(&b'/') || name == "." || name == ".." {
            return Err(reject(Reason::Io(libc::EINVAL)));
        }
        let name_c = CString::new(name.as_bytes()).map_err(|_| reject(Reason::Io(libc::EINVAL)))?;

        // Look before opening, so a FIFO, socket, or device is never opened.
        let before = stat_at(self.fd.as_raw_fd(), &name_c).map_err(|e| reject(Reason::Io(e)))?;
        check_type(&before, &kind).map_err(reject)?;

        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | match kind {
                Kind::Dir => libc::O_DIRECTORY,
                Kind::File => 0,
            };
        // SAFETY: valid directory descriptor and NUL-terminated name.
        let raw = unsafe { libc::openat(self.fd.as_raw_fd(), name_c.as_ptr(), flags) };
        if raw < 0 {
            let err = errno();
            return Err(reject(if err == libc::ELOOP {
                Reason::Symlink
            } else {
                Reason::Io(err)
            }));
        }
        // SAFETY: raw is a descriptor we just opened and own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let after = fstat(&fd).map_err(|e| reject(Reason::Io(e)))?;
        if (after.st_dev, after.st_ino) != (before.st_dev, before.st_ino) {
            return Err(reject(Reason::Replaced));
        }
        check_type(&after, &kind).map_err(reject)?;
        Ok(fd)
    }
}

fn check_type(stat: &libc::stat, kind: &Kind) -> Result<(), Reason> {
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFLNK => Err(Reason::Symlink),
        libc::S_IFDIR => match kind {
            Kind::Dir => Ok(()),
            Kind::File => Err(Reason::SpecialFile),
        },
        libc::S_IFREG => match kind {
            Kind::Dir => Err(Reason::NotADirectory),
            Kind::File if stat.st_nlink != 1 => Err(Reason::MultiplyLinked),
            Kind::File => Ok(()),
        },
        _ => Err(Reason::SpecialFile),
    }
}

fn stat_at(dir: libc::c_int, name: &CStr) -> Result<libc::stat, i32> {
    // SAFETY: zeroed stat is a valid out-parameter; name is NUL-terminated.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatat(dir, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) };
    if rc == 0 { Ok(stat) } else { Err(errno()) }
}

fn fstat(fd: &OwnedFd) -> Result<libc::stat, i32> {
    // SAFETY: zeroed stat is a valid out-parameter for fstat on a descriptor we own.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) };
    if rc == 0 { Ok(stat) } else { Err(errno()) }
}

fn errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// One component of a log location: a fixed name, or every entry of the directory
/// whose name has the given prefix and suffix.
#[derive(Debug, Clone, Copy)]
pub enum Component {
    Name(&'static str),
    Any {
        prefix: &'static str,
        suffix: &'static str,
    },
}

impl Component {
    fn matches(&self, name: &OsStr) -> bool {
        match self {
            Component::Name(fixed) => name == *fixed,
            Component::Any { prefix, suffix } => {
                let bytes = name.as_bytes();
                bytes.len() >= prefix.len() + suffix.len()
                    && bytes.starts_with(prefix.as_bytes())
                    && bytes.ends_with(suffix.as_bytes())
            }
        }
    }
}

/// A log file read out of a run home.
#[derive(Debug)]
pub struct LogFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Reads every file under `parent/home_name` that matches `pattern` (the last component
/// names files, the others directories). A missing fixed directory means no logs. Any
/// refused entry on the way refuses the whole extraction, so a run cannot hide a
/// substitution among good files.
pub fn extract(
    parent: &Path,
    home_name: &str,
    pattern: &[Component],
    limit: u64,
) -> Result<Vec<LogFile>, Rejected> {
    let root = Dir::open_root(parent, home_name)?;
    let mut found = Vec::new();
    walk(&root, pattern, limit, &mut found)?;
    Ok(found)
}

fn walk(
    dir: &Dir,
    pattern: &[Component],
    limit: u64,
    found: &mut Vec<LogFile>,
) -> Result<(), Rejected> {
    let Some((first, rest)) = pattern.split_first() else {
        return Ok(());
    };
    // A fixed name is kwoon's and may be shown; a matched one is the run's.
    let (names, shown): (Vec<OsString>, &str) = match first {
        Component::Name(name) => (vec![OsString::from(name)], name),
        Component::Any { .. } => (
            dir.entries()?
                .into_iter()
                .filter(|n| first.matches(n))
                .collect(),
            "*",
        ),
    };
    for name in names {
        if rest.is_empty() {
            match dir.read_file(&name, shown, limit) {
                Ok(bytes) => found.push(LogFile {
                    path: dir.child_path(shown),
                    bytes,
                }),
                Err(Rejected {
                    reason: Reason::Io(libc::ENOENT),
                    ..
                }) => {}
                Err(rejected) => return Err(rejected),
            }
        } else {
            match dir.open_dir(&name, shown) {
                Ok(child) => walk(&child, rest, limit, found)?,
                Err(Rejected {
                    reason: Reason::Io(libc::ENOENT),
                    ..
                }) => {}
                Err(rejected) => return Err(rejected),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::symlink;

    const PATTERN: &[Component] = &[
        Component::Name("logs"),
        Component::Any {
            prefix: "",
            suffix: ".jsonl",
        },
    ];

    /// Reports whether anything opened or read the watched paths.
    fn watch(paths: &[&Path]) -> OwnedFd {
        // SAFETY: plain syscalls on a descriptor owned below.
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(raw >= 0);
        for path in paths {
            let c = CString::new(path.as_os_str().as_bytes()).unwrap();
            let mask = libc::IN_OPEN | libc::IN_ACCESS;
            assert!(unsafe { libc::inotify_add_watch(raw, c.as_ptr(), mask) } >= 0);
        }
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    fn saw_any(fd: &OwnedFd) -> bool {
        let mut buf = [0u8; 4096];
        // SAFETY: reading into a local buffer.
        unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) > 0 }
    }

    #[test]
    fn reads_matching_logs_and_ignores_other_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("home/logs")).unwrap();
        fs::write(dir.path().join("home/logs/a.jsonl"), "a").unwrap();
        fs::write(dir.path().join("home/logs/notes.txt"), "x").unwrap();
        let logs = extract(dir.path(), "home", PATTERN, 10).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].path, "home/logs/*");
        assert_eq!(logs[0].bytes, b"a");
        // A missing log directory means no logs, not an error.
        fs::remove_dir_all(dir.path().join("home/logs")).unwrap();
        assert!(extract(dir.path(), "home", PATTERN, 10).unwrap().is_empty());
    }

    #[test]
    fn a_symlinked_run_home_root_is_refused_without_opening_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let canary = dir.path().join("canary");
        fs::create_dir_all(canary.join("logs")).unwrap();
        fs::write(canary.join("logs/secret.jsonl"), "canary-bytes").unwrap();
        symlink(&canary, dir.path().join("home")).unwrap();
        let watcher = watch(&[
            &canary,
            &canary.join("logs"),
            &canary.join("logs/secret.jsonl"),
        ]);
        let rejected = extract(dir.path(), "home", PATTERN, 1 << 20).unwrap_err();
        assert_eq!(rejected.path, "home");
        assert_eq!(rejected.reason, Reason::Symlink);
        assert!(!rejected.to_string().contains("canary-bytes"));
        assert!(!saw_any(&watcher), "the canary was opened");
    }

    #[test]
    fn an_oversized_log_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("home/logs")).unwrap();
        fs::write(dir.path().join("home/logs/a.jsonl"), "0123456789").unwrap();
        let rejected = extract(dir.path(), "home", PATTERN, 4).unwrap_err();
        assert_eq!(rejected.reason, Reason::TooLarge);
    }
}
