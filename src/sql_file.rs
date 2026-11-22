//! A `.sql` file a query tab is bound to: read into the editor, written back
//! on Save, exactly as the buffer holds it.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

// ponytail: the editor holds the whole file, so a dump past this is refused
// rather than opened into a buffer that crawls; running a file that size
// without loading it is the upgrade.
pub(crate) const MAX_BYTES: u64 = 4 << 20;

const BOM: &str = "\u{feff}";

/// A file's text and whether it began with a UTF-8 byte order mark, which is
/// the file's encoding and not the user's SQL: the editor never holds it, and
/// Save writes it back.
#[derive(Debug, PartialEq)]
pub(crate) struct SqlText {
    pub(crate) text: String,
    pub(crate) bom: bool,
}

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// `path`'s text, or `None` when nothing is there. Text that is not UTF-8 is
/// refused rather than read lossily, which would change statements.
pub(crate) fn read(path: &Path) -> Result<Option<SqlText>, String> {
    let name = file_name(path);
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not open {name}: {error}")),
    };
    // Bounded while reading, not by the size up front, which a file still
    // being written or a link to a device does not keep to.
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read {name}: {error}"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(format!(
            "{name} is more than the {} MB the editor opens.",
            MAX_BYTES >> 20
        ));
    }
    decoded(bytes)
        .map(Some)
        .ok_or_else(|| format!("{name} is not UTF-8 text."))
}

pub(crate) fn decoded(bytes: Vec<u8>) -> Option<SqlText> {
    let text = String::from_utf8(bytes).ok()?;
    Some(match text.strip_prefix(BOM) {
        Some(rest) => SqlText {
            text: rest.to_string(),
            bom: true,
        },
        None => SqlText { text, bom: false },
    })
}

/// Write `text` to `path` as it is, behind the byte order mark the file had.
///
/// Staged beside the file and renamed over it, so a failed write never leaves
/// half a file. A symlink is followed to its target, or the rename would
/// replace the link with a regular file, and the file's permissions are kept,
/// a read-only one refused: this is the user's file, not one of DBDelve's
/// private ones.
pub(crate) fn write(path: &Path, text: &str, bom: bool) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);
    let path = canonical(path);
    let name = file_name(&path);
    let directory = path.parent().unwrap_or(Path::new("."));
    // A link `canonical` could not resolve leads nowhere, and the rename
    // would put a regular file where the link was.
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!("{name} is a link to a file that does not exist."));
    }
    let permissions = match fs::metadata(&path) {
        Ok(metadata) => {
            // The rename only needs the directory writable, so it would
            // replace a file its owner made read-only.
            if metadata.is_file()
                && let Err(error) = fs::OpenOptions::new().write(true).open(&path)
            {
                return Err(match error.kind() {
                    io::ErrorKind::PermissionDenied => format!("{name} is read-only."),
                    _ => format!("Could not save {name}: {error}"),
                });
            }
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Could not save {name}: {error}")),
    };
    let temporary = directory.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("Could not save {name}: {error}"))?;
    let written = (|| {
        if bom {
            file.write_all(BOM.as_bytes())?;
        }
        file.write_all(text.as_bytes())?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)
    })();
    written.map_err(|error| match discard(&temporary) {
        Ok(()) => format!("Could not save {name}: {error}"),
        Err(left) => format!(
            "Could not save {name}: {error}. {} was left beside it: {left}",
            file_name(&temporary)
        ),
    })
}

/// Remove a staged copy that never replaced its file.
fn discard(temporary: &Path) -> io::Result<()> {
    // Windows will not delete a read-only file, and the copy may have been
    // given the attribute along with the rest of the file's permissions.
    #[cfg(windows)]
    if let Ok(metadata) = fs::metadata(temporary) {
        let mut permissions = metadata.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(temporary, permissions)?;
    }
    fs::remove_file(temporary)
}

/// `path` with every link resolved, for a path that may not exist yet too: the
/// deepest ancestor that does is resolved and the rest put back on it. Two
/// tabs are on one file when their canonical paths are equal.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        if let Ok(found) = fs::canonicalize(at) {
            return rest.iter().rev().fold(found, |path, part| path.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(part)) => {
                rest.push(part.to_owned());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// What Save As writes to: the path picked, with `.sql` added when the user
/// typed no extension.
pub(crate) fn with_sql_extension(mut path: PathBuf) -> PathBuf {
    if path.extension().is_none() {
        path.set_extension("sql");
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dbdelve-sql-file-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        canonical(&dir)
    }

    #[test]
    fn a_byte_order_mark_is_kept_out_of_the_text_and_written_back() {
        let dir = scratch_dir("bom");
        let path = dir.join("q.sql");
        fs::write(&path, b"\xef\xbb\xbfSELECT 1;\r\n").unwrap();
        let read = read(&path).unwrap().unwrap();
        assert_eq!(
            read,
            SqlText {
                text: "SELECT 1;\r\n".into(),
                bom: true
            }
        );
        write(&path, "SELECT 2;\r\n", read.bom).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfSELECT 2;\r\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_file_reads_as_nothing_and_a_file_past_the_cap_is_refused() {
        let dir = scratch_dir("cap");
        assert_eq!(read(&dir.join("gone.sql")), Ok(None));
        let (at, past) = (dir.join("at.sql"), dir.join("past.sql"));
        fs::write(&at, vec![b' '; MAX_BYTES as usize]).unwrap();
        fs::write(&past, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(read(&at).unwrap().is_some());
        assert_eq!(
            read(&past),
            Err("past.sql is more than the 4 MB the editor opens.".to_string())
        );
        assert_eq!(decoded(b"SELECT '\xe9';".to_vec()), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_keeps_the_files_permissions_and_writes_through_a_link() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = scratch_dir("save");
        let target = dir.join("target.sql");
        let link = dir.join("link.sql");
        fs::write(&target, "SELECT 1;").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &link).unwrap();

        write(&link, "SELECT 2;", false).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "SELECT 2;");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(canonical(&link), target);
        // Nothing staged is left beside it.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_a_link_that_leads_nowhere_and_leaves_the_link() {
        let dir = scratch_dir("dangling");
        let link = dir.join("link.sql");
        std::os::unix::fs::symlink(dir.join("gone.sql"), &link).unwrap();

        assert_eq!(
            write(&link, "SELECT 1;", false),
            Err("link.sql is a link to a file that does not exist.".to_string())
        );
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!dir.join("gone.sql").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_a_read_only_file_and_leaves_it_as_it_was() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir("read-only");
        let path = dir.join("locked.sql");
        fs::write(&path, "SELECT 1;").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        // Root writes it regardless, and so may Save.
        if fs::OpenOptions::new().write(true).open(&path).is_err() {
            assert_eq!(
                write(&path, "SELECT 2;", false),
                Err("locked.sql is read-only.".to_string())
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "SELECT 1;");
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_that_does_not_exist_yet_resolves_through_its_parent() {
        let dir = scratch_dir("canonical");
        let raw = std::env::temp_dir()
            .join(format!("dbdelve-sql-file-canonical-{}", std::process::id()))
            .join("new")
            .join("q.sql");
        assert_eq!(canonical(&raw), dir.join("new").join("q.sql"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_as_adds_sql_only_where_no_extension_was_typed() {
        assert_eq!(
            with_sql_extension(PathBuf::from("/q/orders")),
            PathBuf::from("/q/orders.sql")
        );
        assert_eq!(
            with_sql_extension(PathBuf::from("/q/orders.txt")),
            PathBuf::from("/q/orders.txt")
        );
    }
}
