use std::{fs, io, path::Path};

use super::download::COULD_NOT_REPLACE;

pub(super) const ALREADY_UPDATED: &str =
    "DBDelve was updated since it started. Restart it to run that version.";

/// A file, whatever path it is reached by or renamed to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Identity {
    pub(super) device: u64,
    pub(super) inode: u64,
}

impl Identity {
    #[cfg(unix)]
    pub(super) fn of(path: &Path) -> io::Result<Self> {
        path.symlink_metadata()
            .map(|metadata| Self::from(&metadata))
    }

    /// By volume serial number and file index, which is what Windows has in
    /// place of a device and inode.
    #[cfg(windows)]
    pub(super) fn of(path: &Path) -> io::Result<Self> {
        use std::os::windows::{fs::OpenOptionsExt as _, io::AsRawHandle as _};

        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            GetFileInformationByHandle,
        };

        // No access asked for, which is all reading attributes needs, as std's
        // own metadata calls open it.
        let file = fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: a handle this function owns for the length of the call, and
        // a pointer to a struct of the type the call fills.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            device: u64::from(info.dwVolumeSerialNumber),
            inode: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }
}

#[cfg(unix)]
impl From<&fs::Metadata> for Identity {
    fn from(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;

        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// Another process may have updated this install since it started, perhaps
/// past the release this one offers: putting that over it would go backwards.
/// [`ALREADY_UPDATED`], which offers a restart, only when another regular file
/// has taken its place; anything else there could not be launched, and is a
/// plain failure. The reason comes with the detail for the log.
pub(super) fn unchanged(installed: &Path, running: Identity) -> Result<(), (&'static str, String)> {
    let metadata = installed.symlink_metadata().map_err(|error| {
        (
            COULD_NOT_REPLACE,
            format!("{}: {error}", installed.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err((
            COULD_NOT_REPLACE,
            format!("{} is no longer a file", installed.display()),
        ));
    }
    #[cfg(unix)]
    let identity = Identity::from(&metadata);
    #[cfg(windows)]
    let identity = Identity::of(installed).map_err(|error| {
        (
            COULD_NOT_REPLACE,
            format!("{}: {error}", installed.display()),
        )
    })?;
    if identity != running {
        return Err((
            ALREADY_UPDATED,
            format!(
                "{} is no longer the file this DBDelve started from",
                installed.display()
            ),
        ));
    }
    Ok(())
}
