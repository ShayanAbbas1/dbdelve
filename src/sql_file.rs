//! A `.sql` file a query tab is bound to: read into the editor, written back
//! on Save, exactly as the buffer holds it.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
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
    pub(crate) stamp: Stamp,
}

/// A file's modification time and length, as a read or write left it. One
/// that differs at the next look means something else wrote the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }
    }
}

#[cfg(test)]
impl Stamp {
    pub(crate) fn of_len(len: u64) -> Self {
        Self {
            modified: None,
            len,
        }
    }
}

/// A digest of a file's text and byte order mark that a relaunch can compare.
/// FNV-1a by hand: std's `DefaultHasher` may change between Rust releases,
/// and a collision costs only a question not asked.
pub(crate) fn text_hash(text: &str, bom: bool) -> String {
    let hash = std::iter::once(u8::from(bom))
        .chain(text.bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// `path`'s stamp now, or `None` when nothing is there: one `stat`.
pub(crate) fn stamp(path: &Path) -> io::Result<Option<Stamp>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(Stamp::of(&metadata))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
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
    let stamp = file
        .metadata()
        .map(|metadata| Stamp::of(&metadata))
        .map_err(|error| format!("Could not read {name}: {error}"))?;
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
    let (text, bom) = decoded(bytes).ok_or_else(|| format!("{name} is not UTF-8 text."))?;
    Ok(Some(SqlText { text, bom, stamp }))
}

/// The text of `bytes`, and whether a byte order mark came off the front.
pub(crate) fn decoded(bytes: Vec<u8>) -> Option<(String, bool)> {
    let text = String::from_utf8(bytes).ok()?;
    Some(match text.strip_prefix(BOM) {
        Some(rest) => (rest.to_string(), true),
        None => (text, false),
    })
}

/// Write `text` to `path` as it is, behind the byte order mark the file had,
/// and the file's stamp once it is there.
///
/// Staged beside the file and renamed over it, so a failed write never leaves
/// half a file. A symlink is followed to its target, or the rename would
/// replace the link with a regular file, and the file's permissions are kept,
/// a read-only one refused: this is the user's file, not one of DBDelve's
/// private ones.
pub(crate) fn write(path: &Path, text: &str, bom: bool) -> Result<Stamp, String> {
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
    let written = (|| -> io::Result<Stamp> {
        if bom {
            file.write_all(BOM.as_bytes())?;
        }
        file.write_all(text.as_bytes())?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        // Off the staged copy, which the rename moves without touching.
        let stamp = Stamp::of(&file.metadata()?);
        drop(file);
        fs::rename(&temporary, &path)?;
        Ok(stamp)
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

/// How copying one file for a move went.
#[derive(Debug, PartialEq)]
pub(crate) enum Move {
    Done,
    /// Something already has the name, and is left as it was.
    Taken,
    Failed(String),
}

/// The first half of moving `from` to `to`, by copying, never by renaming: a
/// rename fails across volumes, and a picked folder is often on another. `to`
/// is made new, so a file already there is never written over, and the copy
/// and its directory entry are on disk before this returns. A copy that
/// fails is removed. [`remove_moved`] is the second half.
///
/// The copy takes the folder's default permissions rather than the support
/// directory's 0600: it is the user's file now, beside the ones their own
/// editor makes there.
pub(crate) fn copy_new(from: &Path, to: &Path) -> Move {
    let mut copy = match fs::OpenOptions::new().write(true).create_new(true).open(to) {
        Ok(copy) => copy,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Move::Taken,
        Err(error) => return Move::Failed(format!("Could not create {}: {error}", to.display())),
    };
    let copied = fs::File::open(from)
        .and_then(|mut source| io::copy(&mut source, &mut copy))
        .and_then(|_| copy.sync_all())
        .and_then(|()| sync_directory(to));
    drop(copy);
    match copied {
        Ok(()) => Move::Done,
        Err(error) => {
            let _ = fs::remove_file(to);
            Move::Failed(format!("Could not copy {}: {error}", from.display()))
        }
    }
}

/// `from`, once [`copy_new`] has put it at `to`. One that will not go takes
/// its copy with it, so one file is left either way. A link goes, never the
/// file it leads to.
pub(crate) fn remove_moved(from: &Path, to: &Path) -> Result<(), String> {
    fs::remove_file(from).map_err(|error| {
        let _ = fs::remove_file(to);
        format!("Could not remove {}: {error}", from.display())
    })
}

/// `directory` and every parent of it missing, each new entry synced into the
/// directory holding it before the next is made: a crash must not keep the
/// removal of a query and lose the folder its copy went into.
pub(crate) fn create_dirs(directory: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut at = directory;
    loop {
        match fs::metadata(at) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(at);
                at = at.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    }
    for made in missing.into_iter().rev() {
        match fs::create_dir(made) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => sync_directory(made)?,
        }
    }
    Ok(())
}

/// The new name's directory entry on disk too: a synced file under an entry
/// a crash forgets is lost along with the source removed after it.
fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
        assert_eq!((read.text.as_str(), read.bom), ("SELECT 1;\r\n", true));
        write(&path, "SELECT 2;\r\n", read.bom).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfSELECT 2;\r\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_files_stamp_is_what_its_read_and_write_left_until_someone_else_writes() {
        let dir = scratch_dir("stamp");
        let path = dir.join("q.sql");
        assert_eq!(stamp(&path).unwrap(), None);
        let written = write(&path, "SELECT 1;", false).unwrap();
        assert_eq!(stamp(&path).unwrap(), Some(written));
        assert_eq!(read(&path).unwrap().unwrap().stamp, written);
        fs::write(&path, "SELECT 12;").unwrap();
        assert_ne!(stamp(&path).unwrap(), Some(written));
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

    #[test]
    fn a_move_copies_the_bytes_exactly_and_then_removes_the_source() {
        let dir = scratch_dir("move");
        let (from, to) = (dir.join("daily.sql"), dir.join("daily-moved.sql"));
        let bytes = b"\xef\xbb\xbfSELECT 1;\r\n\xff";
        fs::write(&from, bytes).unwrap();
        assert_eq!(copy_new(&from, &to), Move::Done);
        assert_eq!(fs::read(&to).unwrap(), bytes);
        assert!(from.exists(), "the copy alone removes nothing");
        remove_moved(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&to).unwrap(), bytes);
    }

    #[test]
    fn the_folders_a_move_needs_are_made_however_deep_and_again_without_harm() {
        let dir = scratch_dir("move-dirs");
        let deep = dir.join("Moved").join("Prod").join("2026");
        create_dirs(&deep).unwrap();
        assert!(deep.is_dir());
        create_dirs(&deep).unwrap();
    }

    #[test]
    fn a_move_onto_a_taken_name_writes_nothing_and_leaves_its_source() {
        let dir = scratch_dir("move-taken");
        let (from, to) = (dir.join("daily.sql"), dir.join("taken.sql"));
        fs::write(&from, "mine").unwrap();
        fs::write(&to, "theirs").unwrap();
        assert_eq!(copy_new(&from, &to), Move::Taken);
        assert_eq!(fs::read_to_string(&to).unwrap(), "theirs");
        assert_eq!(fs::read_to_string(&from).unwrap(), "mine");
    }

    #[test]
    fn a_copy_that_fails_is_removed_and_its_source_kept() {
        let dir = scratch_dir("move-fails");
        // A directory opens or reads as no file does, on every platform.
        let (from, to) = (dir.join("daily.sql"), dir.join("moved.sql"));
        fs::create_dir(&from).unwrap();
        assert!(matches!(copy_new(&from, &to), Move::Failed(_)));
        assert!(!to.exists());
        assert!(from.is_dir());
    }
}
