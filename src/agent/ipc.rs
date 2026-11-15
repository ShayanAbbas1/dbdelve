//! The socket's directory, its bind, and who is on the other end of it.
//!
//! The directory is the lock and the euid check is the second one: a local
//! process of another user cannot reach a `0700` directory, and if it got
//! through, [`admit`] refuses it before a byte is read. Only a filesystem path
//! is ever bound. The Linux abstract namespace has no permissions, so it is
//! never used.

use std::{
    fs::{File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
};

use super::Declined;

/// The longest path `bind` accepts: `sun_path` less the terminating NUL.
pub(super) fn max_path_len() -> usize {
    // SAFETY: `sockaddr_un` is plain old data, so all zeroes is a valid value.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_path.len() - 1
}

/// Refuse a path the platform cannot bind, by its length, before one is tried.
pub(super) fn check_len(path: &Path) -> Result<(), String> {
    let length = path.as_os_str().len();
    let max = max_path_len();
    if length > max {
        return Err(format!(
            "The agent socket path is {length} bytes long; this system allows at most {max}."
        ));
    }
    Ok(())
}

/// Where this build's socket lives: beside its profiles, so a dev build and a
/// release build never share one. No other directory is tried when this one is
/// too long to bind.
pub(crate) fn socket_path() -> Result<PathBuf, String> {
    let path = crate::store::dbdelve_directory()?.join("agent").join("s");
    check_len(&path)?;
    Ok(path)
}

/// The effective uid this process runs as.
pub(super) fn current_euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
    unsafe { libc::geteuid() }
}

/// The effective uid of the process on the other end of `stream`.
///
/// std's own call for this is unstable, so the platform's is used: the peer's
/// credentials as the kernel recorded them when the connection was made.
pub(super) fn peer_euid(stream: &UnixStream) -> io::Result<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `credentials` and `length` outlive the call, and `length` is
        // the size of the buffer the option is written into.
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast::<libc::c_void>(),
                &mut length,
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(credentials.uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
        // SAFETY: both out-pointers are to live locals.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Whether the peer of `stream` runs as `expected`. A peer whose credentials
/// cannot be read is not admitted. `expected` is a parameter so a test can
/// give the wrong uid and watch the connection refused.
pub(super) fn admit(stream: &UnixStream, expected: u32) -> bool {
    peer_euid(stream).is_ok_and(|uid| uid == expected)
}

/// Make `dir` exist, be a directory of this user's own, and be reachable by
/// nobody else. A symbolic link, a file, and a directory another user owns are
/// refused; one with group or other bits is tightened.
pub(super) fn secure_dir(dir: &Path) -> Result<(), String> {
    let shown = dir.display();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| {
            format!("The agent socket directory {shown} could not be created: {error}.")
        })?;
    let metadata = std::fs::symlink_metadata(dir).map_err(|error| {
        format!("The agent socket directory {shown} could not be read: {error}.")
    })?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "The agent socket directory {shown} is a symbolic link."
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "The agent socket directory {shown} is not a directory."
        ));
    }
    if metadata.uid() != current_euid() {
        return Err(format!(
            "The agent socket directory {shown} is owned by another user."
        ));
    }
    if metadata.mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
            format!("The agent socket directory {shown} could not be made private: {error}.")
        })?;
    }
    Ok(())
}

/// Whether something answers on the socket at `path`: another window serving.
/// A plain connect that sends no frame, which the serving window closes
/// without counting as a call or a presence.
pub(super) fn is_live(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// A bound socket and the lock that makes it this window's. The lock is held
/// for as long as the window serves: while it is, no other window binds or
/// removes the path, and the kernel lets go of it when the process ends, crash
/// or not.
pub(super) struct Bound {
    pub(super) listener: UnixListener,
    pub(super) lock: File,
}

/// Take the serving lock beside `path`, or learn that another window holds it.
///
/// Whether a window serves is asked of this lock and never of a connect: a
/// socket whose backlog is full refuses a connect exactly as a stale one does,
/// so a connect cannot tell a busy window from a crashed one.
fn take_lock(path: &Path) -> Result<File, Declined> {
    let lock_path = path.with_extension("lock");
    let shown = lock_path.display();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| {
            Declined::Failed(format!(
                "The agent socket's lock {shown} could not be opened: {error}."
            ))
        })?;
    // SAFETY: the descriptor is the open file's own and outlives the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Err(Declined::ServedElsewhere);
        }
        return Err(Declined::Failed(format!(
            "The agent socket's lock {shown} could not be taken: {error}."
        )));
    }
    Ok(file)
}

/// Replace what binding `path` found there. Only a window holding the lock
/// gets here, so a socket at the path is one a crash left behind. A socket of
/// this user's is removed, once; a file of any other kind, or a socket of
/// another user's, is never removed.
fn replace_stale(path: &Path) -> Result<UnixListener, Declined> {
    let shown = path.display();
    let ours = std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.uid() == current_euid());
    if !ours {
        return Err(Declined::Failed(format!(
            "Something other than a DBDelve socket of this user is at {shown}."
        )));
    }
    std::fs::remove_file(path).map_err(|error| {
        Declined::Failed(format!(
            "The stale agent socket {shown} could not be removed: {error}."
        ))
    })?;
    UnixListener::bind(path).map_err(|error| {
        Declined::Failed(format!(
            "The agent socket {shown} could not be bound: {error}."
        ))
    })
}

/// Bind the socket at `path`, in a private directory, readable and writable by
/// this user only, under the serving lock.
pub(super) fn bind(path: &Path) -> Result<Bound, Declined> {
    let shown = path.display();
    let Some(parent) = path.parent() else {
        return Err(Declined::Failed(format!(
            "The agent socket path {shown} has no directory."
        )));
    };
    secure_dir(parent).map_err(Declined::Failed)?;
    let lock = take_lock(path)?;
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => replace_stale(path)?,
        Err(error) => {
            return Err(Declined::Failed(format!(
                "The agent socket {shown} could not be bound: {error}."
            )));
        }
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        Declined::Failed(format!(
            "The agent socket {shown} could not be made private: {error}."
        ))
    })?;
    Ok(Bound { listener, lock })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::agent::tests::short_socket_path;

    fn fresh_dir() -> PathBuf {
        short_socket_path()
            .parent()
            .expect("the path has a directory")
            .to_path_buf()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("the path exists")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn the_socket_directory_is_made_private_to_this_user() {
        let dir = fresh_dir();
        secure_dir(&dir).expect("an absent directory is created");
        assert_eq!(mode(&dir), 0o700);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("the mode changes");
        secure_dir(&dir).expect("an open directory is tightened");
        assert_eq!(mode(&dir), 0o700);

        let link = fresh_dir();
        std::os::unix::fs::symlink(&dir, &link).expect("the link is made");
        assert!(secure_dir(&link).is_err());
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("the link is still there")
                .file_type()
                .is_symlink()
        );
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_socket_is_left_alone_and_a_stale_one_is_replaced() {
        let path = short_socket_path();
        let dir = path
            .parent()
            .expect("the path has a directory")
            .to_path_buf();
        let Ok(first) = bind(&path) else {
            panic!("the first bind succeeds");
        };
        assert_eq!(mode(&path), 0o600);

        assert!(matches!(bind(&path), Err(Declined::ServedElsewhere)));
        let client = UnixStream::connect(&path).expect("the live socket still takes clients");
        assert!(first.listener.accept().is_ok());
        drop(client);

        // A crash leaves the file behind with nobody listening.
        drop(first);
        assert!(path.exists());
        let Ok(second) = bind(&path) else {
            panic!("a stale socket is replaced");
        };
        let client = UnixStream::connect(&path).expect("the new socket takes clients");
        assert!(second.listener.accept().is_ok());
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_starting_together_over_a_stale_socket_leave_exactly_one_serving() {
        let path = short_socket_path();
        let dir = path
            .parent()
            .expect("the path has a directory")
            .to_path_buf();
        // A crash leaves the file behind with nobody listening.
        drop(bind(&path).unwrap_or_else(|_| panic!("the first bind succeeds")));
        assert!(path.exists());

        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let windows: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    bind(&path)
                })
            })
            .collect();
        let results: Vec<_> = windows
            .into_iter()
            .map(|window| window.join().expect("the window thread ends"))
            .collect();
        let serving: Vec<&Bound> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(serving.len(), 1);
        assert!(
            results
                .iter()
                .all(|result| matches!(result, Ok(_) | Err(Declined::ServedElsewhere)))
        );
        let client = UnixStream::connect(&path).expect("the serving socket takes clients");
        assert!(serving[0].listener.accept().is_ok());
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_regular_file_at_the_socket_path_is_never_removed() {
        let path = short_socket_path();
        let dir = path
            .parent()
            .expect("the path has a directory")
            .to_path_buf();
        secure_dir(&dir).expect("the directory is made");
        std::fs::write(&path, b"not a socket").expect("the file is written");

        assert!(matches!(bind(&path), Err(Declined::Failed(_))));
        assert_eq!(
            std::fs::read(&path).expect("the file is still there"),
            b"not a socket"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_peer_with_another_uid_is_refused() {
        let (stream, _peer) = UnixStream::pair().expect("a socket pair");
        assert!(admit(&stream, current_euid()));
        assert!(!admit(&stream, current_euid().wrapping_add(1)));
    }

    #[test]
    fn a_socket_path_one_byte_over_the_limit_is_refused_by_name() {
        let dir = fresh_dir();
        std::fs::create_dir_all(&dir).expect("the directory is made");
        let max = max_path_len();
        let room = max - dir.as_os_str().len() - 1;
        let longest = dir.join("a".repeat(room));
        let too_long = dir.join("a".repeat(room + 1));
        assert_eq!(longest.as_os_str().len(), max);

        assert!(check_len(&longest).is_ok());
        assert!(UnixListener::bind(&longest).is_ok());
        assert!(UnixListener::bind(&too_long).is_err());
        assert_eq!(
            check_len(&too_long),
            Err(format!(
                "The agent socket path is {} bytes long; this system allows at most {max}.",
                max + 1
            ))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
