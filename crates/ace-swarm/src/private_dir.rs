//! Owner-private state files in an operator-chosen directory, hardened against hostile files.
//!
//! The directory walk never follows a symlink and creates missing components `0700`. The final
//! directory must belong to this user and must not be group- or world-writable. Every file must
//! be a regular `0600` file owned by this user with one link. An exclusive `flock` on a per-store
//! lock file keeps a second writer, including one reaching the directory through an alias, from
//! interleaving with this one. Writes go to a private temporary file, are synced, and replace the
//! target with an atomic rename. Reads are bounded. File names are constants chosen by the
//! caller, never derived from request input. Other platforms report the store as unsupported, so
//! callers fall back to memory.
use std::io;
use std::path::{Path, PathBuf};

/// A failed filesystem step: which stage failed, the path it acted on when that helps the
/// operator, and the OS error. Built on the error path only.
#[derive(Debug)]
pub struct Failure {
    pub stage: &'static str,
    pub path: Option<PathBuf>,
    pub error: io::Error,
}

impl From<Failure> for io::Error {
    fn from(failure: Failure) -> Self {
        failure.error
    }
}

/// What makes two failures "the same" for deduplicated reporting.
pub type FailureKey = (&'static str, io::ErrorKind, Option<i32>);

impl Failure {
    pub fn key(&self) -> FailureKey {
        (self.stage, self.error.kind(), self.error.raw_os_error())
    }
}

/// Attach a stage name to an I/O result.
pub trait Stage<T> {
    fn stage(self, stage: &'static str) -> Result<T, Failure>;
}

impl<T> Stage<T> for io::Result<T> {
    fn stage(self, stage: &'static str) -> Result<T, Failure> {
        self.map_err(|error| Failure {
            stage,
            path: None,
            error,
        })
    }
}

pub use imp::{resolve_ancestors, PrivateDir};

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    /// An opened, validated and locked private directory. The lock is held until drop.
    pub struct PrivateDir {
        dir: File,
        _lock: File,
    }

    /// Resolve symlinks in the ancestors of the configured directory, never in the directory
    /// itself, so the strict [`PrivateDir::open`] walk can run on the result.
    ///
    /// Ancestors are operator-chosen and were never ownership-checked: anyone able to place a
    /// symlink there could place a real directory instead. Android app storage depends on this,
    /// because in an app's mount namespace `/data/user/0` is a symlink to `/data/data`. Links
    /// are resolved once, before the walk; the walk itself still refuses every symlink, so one
    /// swapped in afterwards fails closed, and the final directory and its files keep their
    /// no-follow, ownership and mode checks. A missing suffix is appended unresolved and created
    /// by the walk. Should resolution fail outright (for example, no `/proc` for `realpath`), the
    /// path is returned unchanged and the walk decides.
    pub fn resolve_ancestors(path: &Path) -> PathBuf {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return path.to_owned();
        };
        let mut existing = parent;
        let mut missing = Vec::new();
        loop {
            match std::fs::canonicalize(existing) {
                Ok(mut resolved) => {
                    resolved.extend(missing.iter().rev());
                    resolved.push(name);
                    return resolved;
                }
                // A dangling link also lands here; the walk then rejects it.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let (Some(up), Some(component)) = (existing.parent(), existing.file_name())
                    else {
                        return path.to_owned();
                    };
                    missing.push(component);
                    existing = up;
                }
                Err(_) => return path.to_owned(),
            }
        }
    }

    fn openat(dir: i32, name: &std::ffi::OsStr, flags: i32, mode: u32) -> io::Result<File> {
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL path"))?;
        // SAFETY: the CString outlives the call; successful fd ownership transfers once.
        let fd = unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn validate(file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.mode() & 0o7777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe cache inode",
            ));
        }
        Ok(())
    }

    impl PrivateDir {
        /// Walk to `path` without following links, creating missing components, validate the
        /// final directory and take the exclusive `lock_name` lock inside it.
        pub fn open(path: &Path, lock_name: &str) -> Result<Self, Failure> {
            // Ancestors need search permission only (notably Android's /data).
            // The final directory stays readable for sync_all after atomic replacement.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            const TRAVERSE: i32 = libc::O_PATH | libc::O_DIRECTORY;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            const TRAVERSE: i32 = libc::O_RDONLY | libc::O_DIRECTORY;
            let mut components = path
                .components()
                .filter_map(|component| match component {
                    Component::Normal(name) => Some(name),
                    _ => None,
                })
                .peekable();
            // The walk names the prefix it failed on, so a symlinked or unreadable ancestor shows.
            let at = |stage, prefix: &Path, error| Failure {
                stage,
                path: Some(prefix.to_owned()),
                error,
            };
            let mut prefix = PathBuf::from("/");
            let mut dir = openat(
                libc::AT_FDCWD,
                prefix.as_os_str(),
                if components.peek().is_some() {
                    TRAVERSE
                } else {
                    libc::O_RDONLY | libc::O_DIRECTORY
                },
                0,
            )
            .map_err(|error| at("open cache path", &prefix, error))?;
            while let Some(name) = components.next() {
                prefix.push(name);
                let flags = if components.peek().is_some() {
                    TRAVERSE
                } else {
                    libc::O_RDONLY | libc::O_DIRECTORY
                };
                dir = match openat(dir.as_raw_fd(), name, flags, 0) {
                    Ok(next) => next,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        let name_c = CString::new(name.as_bytes()).map_err(|_| {
                            let error = io::Error::new(io::ErrorKind::InvalidInput, "NUL path");
                            at("open cache path", &prefix, error)
                        })?;
                        if unsafe { libc::mkdirat(dir.as_raw_fd(), name_c.as_ptr(), 0o700) } < 0 {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::AlreadyExists {
                                return Err(at("create cache directory", &prefix, error));
                            }
                        }
                        openat(dir.as_raw_fd(), name, flags, 0)
                            .map_err(|error| at("open cache path", &prefix, error))?
                    }
                    Err(error) => return Err(at("open cache path", &prefix, error)),
                };
            }
            let metadata = dir.metadata().stage("check cache directory")?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe cache directory",
                ))
                .stage("check cache directory");
            }
            let lock = openat(
                dir.as_raw_fd(),
                std::ffi::OsStr::new(lock_name),
                libc::O_RDWR | libc::O_CREAT,
                0o600,
            )
            .stage("open lock file")?;
            validate(&lock).stage("check lock file")?;
            // This inode lock also prevents directory aliases from creating concurrent writers.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
                return Err(io::Error::last_os_error()).stage("lock cache");
            }
            Ok(Self { dir, _lock: lock })
        }

        /// Read the private file `name`, refusing anything over `max_bytes`.
        pub fn read(&self, name: &str, max_bytes: usize) -> io::Result<Vec<u8>> {
            let file = openat(
                self.dir.as_raw_fd(),
                std::ffi::OsStr::new(name),
                libc::O_RDONLY,
                0,
            )?;
            validate(&file)?;
            if file.metadata()?.len() > max_bytes as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized cache",
                ));
            }
            let mut bytes = Vec::new();
            file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > max_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized cache",
                ));
            }
            Ok(bytes)
        }

        /// Atomically replace the private file `name` with `bytes` (at most `max_bytes`).
        pub fn write(&self, name: &str, bytes: &[u8], max_bytes: usize) -> Result<(), Failure> {
            if bytes.len() > max_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized snapshot",
                ))
                .stage("encode snapshot");
            }
            // Reject a poisoned existing target rather than replacing its untrusted inode.
            match openat(
                self.dir.as_raw_fd(),
                std::ffi::OsStr::new(name),
                libc::O_RDONLY,
                0,
            ) {
                Ok(file) => validate(&file).stage("check cache file")?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).stage("check cache file"),
            }
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let temp = format!(
                ".{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let temp_c = CString::new(temp.as_bytes()).map_err(|_| Failure {
                stage: "create temp file",
                path: None,
                error: io::Error::new(io::ErrorKind::InvalidInput, "NUL path"),
            })?;
            let target = CString::new(name).map_err(|_| Failure {
                stage: "replace cache file",
                path: None,
                error: io::Error::new(io::ErrorKind::InvalidInput, "NUL path"),
            })?;
            let mut file = openat(
                self.dir.as_raw_fd(),
                std::ffi::OsStr::new(&temp),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
            .stage("create temp file")?;
            let result = (|| {
                validate(&file).stage("check temp file")?;
                file.write_all(bytes).stage("write temp file")?;
                file.sync_all().stage("sync temp file")?;
                if unsafe {
                    libc::renameat(
                        self.dir.as_raw_fd(),
                        temp_c.as_ptr(),
                        self.dir.as_raw_fd(),
                        target.as_ptr(),
                    )
                } < 0
                {
                    return Err(io::Error::last_os_error()).stage("replace cache file");
                }
                self.dir.sync_all().stage("sync cache directory")
            })();
            if result.is_err() {
                unsafe { libc::unlinkat(self.dir.as_raw_fd(), temp_c.as_ptr(), 0) };
            }
            result
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    /// Unsupported off Unix: inode validation is required, so callers keep state in memory.
    pub struct PrivateDir;

    pub fn resolve_ancestors(path: &Path) -> PathBuf {
        path.to_owned()
    }

    impl PrivateDir {
        pub fn open(_: &Path, _: &str) -> Result<Self, Failure> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private cache requires Unix inode validation",
            ))
            .stage("open cache path")
        }
        pub fn read(&self, _: &str, _: usize) -> io::Result<Vec<u8>> {
            unreachable!()
        }
        pub fn write(&self, _: &str, _: &[u8], _: usize) -> Result<(), Failure> {
            unreachable!()
        }
    }
}
