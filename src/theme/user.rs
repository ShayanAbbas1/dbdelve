//! The themes in dbdelve's own [`store::theme_directory`]: read at startup,
//! reread whenever a file there is written, removed or added, and added to at
//! runtime by importing one from another app.

use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once, OnceLock, PoisonError, RwLock};

use super::{Theme, vscode, zed};
use crate::store;

static USER_THEMES: RwLock<Vec<Theme>> = RwLock::new(Vec::new());
/// Moved each time [`USER_THEMES`] changes, so every caller that wears a user
/// theme compares against the last one it applied rather than relying on
/// whichever reload happened to see the change, an import's included.
static REVISION: AtomicU64 = AtomicU64::new(0);
static FOUND: [OnceLock<Vec<Theme>>; 2] = [const { OnceLock::new() }; 2];
/// Each file [`reload`] last saw, in name order; held across the whole reread,
/// so two rereads never interleave.
static READ: Mutex<Vec<Read>> = Mutex::new(Vec::new());
static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// A file as last read: the themes it held the last time it parsed, and a
/// hash of the bytes read then.
struct Read {
    path: PathBuf,
    hash: u64,
    themes: Vec<Theme>,
}

/// Contents rather than metadata: a replace of the same length inside the
/// timestamp granularity, or one Windows reports with the old times, is seen
/// all the same, and theme files are a few kilobytes.
fn hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

/// A theme's name with the lifetime [`Theme`] carries. Each distinct name is
/// leaked once, so rereading a file, however often, allocates nothing new.
// ponytail: a linear scan over every name ever read, which is a few dozen.
pub(super) fn intern(name: &str) -> &'static str {
    let mut names = NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(held) = names.iter().find(|held| **held == name) {
        return held;
    }
    let held: &'static str = Box::leak(name.into());
    names.push(held);
    held
}

/// An app whose themes can be imported: a variant, its name, and the call
/// that finds its themes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Zed,
    VsCode,
}

impl Source {
    pub const ALL: [Self; 2] = [Self::Zed, Self::VsCode];

    pub fn name(self) -> &'static str {
        match self {
            Self::Zed => "Zed",
            Self::VsCode => "VS Code",
        }
    }

    /// Every theme this source has on this machine, unglassed, one per name,
    /// in the order the import list shows them. Each is named `<source>:
    /// <theme>`, as it is once imported, so it clashes with neither a built-in
    /// nor another source's. Scanned once per process, which bounds the names
    /// leaked for them.
    pub(crate) fn found(self) -> &'static [Theme] {
        FOUND[self as usize].get_or_init(|| {
            let themes = match self {
                Self::Zed => zed::discover(),
                Self::VsCode => vscode::discover(),
            };
            themes
                .into_iter()
                .map(|theme| Theme {
                    name: Box::leak(format!("{}: {}", self.name(), theme.name).into()),
                    ..theme
                })
                .collect()
        })
    }

    /// Whether [`Self::found`] would answer without waiting on a scan.
    pub(crate) fn is_ready(self) -> bool {
        FOUND[self as usize].get().is_some()
    }

    /// One of [`Self::found`] as it would paint, for the import list to
    /// preview.
    pub fn theme(self, index: usize) -> Option<Theme> {
        self.found().get(index).copied().map(zed::finished)
    }
}

/// Scan each source on a thread of its own, once, so a list opened on one has
/// it ready rather than reading a large extensions folder on the UI thread.
pub(crate) fn prewarm() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        for source in Source::ALL {
            std::thread::spawn(move || source.found());
        }
    });
}

/// Every theme in dbdelve's directory, as of the last [`reload`].
pub(super) fn themes() -> Vec<Theme> {
    USER_THEMES
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Reread the files in dbdelve's directory written, added or removed since the
/// last call, moving [`revision`] if that changed any theme. A name already taken
/// goes to whichever came first: built-in, then the files in name order.
pub(crate) fn reload() {
    let Ok(directory) = store::theme_directory() else {
        return;
    };
    let mut read = READ.lock().unwrap_or_else(PoisonError::into_inner);
    if !reread(&directory, &mut read) {
        return;
    }
    let mut themes: Vec<Theme> = read
        .iter()
        .flat_map(|file| file.themes.iter().copied())
        .collect();
    zed::keep_untaken(
        &mut themes,
        &mut Theme::built_in().map(|theme| theme.name).to_vec(),
    );
    let mut listed = USER_THEMES.write().unwrap_or_else(PoisonError::into_inner);
    if *listed == themes {
        return;
    }
    tracing::info!(count = themes.len(), "user themes loaded");
    *listed = themes;
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Which [`reload`] the listed themes are from.
pub(crate) fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

/// Bring `read` up to `directory`, parsing only the files whose bytes moved,
/// and say whether anything did. A file that does not parse -- broken, or
/// caught part way through a write -- keeps the themes it last parsed to, so a
/// theme on screen never falls back while it is being saved. A file that does
/// not read is left as it was, to be read again on the next pass.
fn reread(directory: &Path, read: &mut Vec<Read>) -> bool {
    let paths = match json_files(directory) {
        Ok(paths) => paths,
        // Unlisted is not empty: every theme stays until a pass lists again.
        Err(error) => {
            tracing::debug!(path = %directory.display(), "theme directory not listed: {error}");
            return false;
        }
    };
    let mut changed = false;
    let mut next = Vec::new();
    for path in paths {
        let held = read
            .iter()
            .position(|file| file.path == path)
            .map(|index| read.swap_remove(index));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                // Not at warn: a file that stays unreadable is retried, and
                // so would be logged, every pass.
                tracing::debug!(path = %path.display(), "theme file not read: {error}");
                next.extend(held);
                continue;
            }
        };
        let hash = hash(&bytes);
        match held {
            Some(held) if held.hash == hash => next.push(held),
            held => {
                changed = true;
                let parsed = String::from_utf8(bytes)
                    .map_err(|error| error.to_string())
                    .and_then(|text| zed::parse_own(&text));
                let themes = match parsed {
                    Ok(themes) => themes,
                    Err(error) => {
                        zed::skipped(&path, &error);
                        held.map(|held| held.themes).unwrap_or_default()
                    }
                };
                next.push(Read { path, hash, themes });
            }
        }
    }
    // Whatever is left was removed.
    changed |= !read.is_empty();
    *read = next;
    changed
}

/// [`zed::json_files`], failing rather than empty when the listing breaks; a
/// directory that is not there is one with no files.
fn json_files(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    paths.sort();
    Ok(paths)
}

/// `theme` as the registry holds it now, found case-blind as
/// `restored_theme` finds a saved name; `theme` itself once no theme answers
/// to its name, so a deleted or renamed file leaves its theme on.
pub(crate) fn current(theme: Theme) -> Theme {
    Theme::all()
        .into_iter()
        .find(|listed| listed.name.eq_ignore_ascii_case(theme.name))
        .unwrap_or(theme)
}

pub(crate) fn is_imported(name: &str) -> bool {
    USER_THEMES
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|theme| theme.name.eq_ignore_ascii_case(name))
}

/// Write a source's theme into dbdelve's directory as a native file and list
/// it, replacing any theme of the same name, and hand back the theme to
/// install.
pub(crate) fn import(source: Source, index: usize) -> Result<Theme, String> {
    let flat = *source
        .found()
        .get(index)
        .ok_or("That theme is no longer listed.")?;
    write_native(&store::theme_directory()?, flat)?;
    // Listed the way any other written file is, so the theme handed back is
    // the one a later reread finds, and no reread can list it twice or not at all.
    reload();
    Ok(themes()
        .into_iter()
        .find(|listed| listed.name.eq_ignore_ascii_case(flat.name))
        .unwrap_or_else(|| zed::finished(flat)))
}

/// Into the file that already holds a theme of its name, the one [`reload`]
/// takes it from, else the first of `<slug>.json`, `<slug>-2.json`… that is
/// free. Never over a file holding another theme, nor one that does not parse,
/// which may be a theme someone is part way through writing.
fn write_native(directory: &Path, theme: Theme) -> Result<PathBuf, String> {
    let slug = slug(theme.name);
    let holder = zed::json_files(directory).into_iter().find_map(|path| {
        let held = zed::read(std::iter::once(path.clone()), zed::parse_own);
        held.iter()
            .any(|held| held.name.eq_ignore_ascii_case(theme.name))
            .then_some((path, held.len()))
    });
    let path = match holder {
        // Overwriting a family would drop its other themes, and a file beside
        // it would hold a name that only one of the two loads under.
        Some((path, held)) if held > 1 => {
            return Err(format!(
                "{} already has a theme named \"{}\"; rename it there to import this one.",
                path.file_name().unwrap_or_default().to_string_lossy(),
                theme.name
            ));
        }
        Some((path, _)) => path,
        None => (1..)
            .map(|n| match n {
                1 => directory.join(format!("{slug}.json")),
                n => directory.join(format!("{slug}-{n}.json")),
            })
            .find(|path| !path.exists())
            .ok_or("No free file name for the theme.")?,
    };
    store::write_file(&path, &zed::native_text(&theme))?;
    Ok(path)
}

fn slug(name: &str) -> String {
    let words: Vec<_> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        "theme".to_string()
    } else {
        words.join("-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_import_overwrites_only_the_file_holding_its_own_name() {
        let found = Source::Zed.found();
        assert!(found.iter().any(|theme| theme.name == "Zed: One Dark"));

        let directory =
            std::env::temp_dir().join(format!("dbdelve-theme-import-{}", std::process::id()));
        let named = |name| Theme {
            name,
            ..Theme::dark()
        };
        let seed = |file: &str, name| {
            store::write_file(&directory.join(file), &zed::native_text(&named(name)))
        };
        let import = |name| {
            write_native(&directory, named(name))
                .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        };
        let held = |file: &str| {
            zed::read(std::iter::once(directory.join(file)), zed::parse_own)
                .first()
                .map(|theme| theme.name)
        };
        seed("zed-ayu-dark.json", "Mine").unwrap();
        seed("custom.json", "zed: gruvbox dark").unwrap();
        store::write_file(
            &directory.join("family.json"),
            r#"{ "name": "Family", "themes": [
                { "name": "Zed: Rosé Pine", "style": {} },
                { "name": "Rosé Pine Moon", "style": {} }
            ] }"#,
        )
        .unwrap();
        let first = import("Zed: One Dark");
        let again = import("Zed: One Dark");
        let beside = import("Zed: Ayu Dark");
        let over = import("Zed: Gruvbox Dark");
        let refused = import("Zed: Rosé Pine");
        let family = zed::read(
            std::iter::once(directory.join("family.json")),
            zed::parse_own,
        )
        .len();
        let held = ["zed-ayu-dark.json", "zed-ayu-dark-2.json", "custom.json"].map(held);
        let files = zed::json_files(&directory).len();
        let _ = std::fs::remove_dir_all(&directory);

        assert_eq!(first.as_deref(), Ok("zed-one-dark.json"));
        assert_eq!(again, first);
        assert_eq!(beside.as_deref(), Ok("zed-ayu-dark-2.json"));
        assert_eq!(over.as_deref(), Ok("custom.json"));
        assert_eq!(
            refused,
            Err(
                "family.json already has a theme named \"Zed: Rosé Pine\"; rename it there to import this one."
                    .into()
            )
        );
        assert_eq!(family, 2);
        assert_eq!(
            held,
            [
                Some("Mine"),
                Some("Zed: Ayu Dark"),
                Some("Zed: Gruvbox Dark")
            ]
        );
        assert_eq!(files, 5);
    }

    #[test]
    fn a_reread_follows_the_directory_and_keeps_the_last_theme_that_parsed() {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-theme-reread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let file = directory.join("omarchy.json");
        let text = |name: &str, surface: &str| {
            format!(
                r##"{{ "name": "{name}", "appearance": "dark", "colors": {{ "surface": "{surface}" }} }}"##
            )
        };
        let write = |surface: &str| {
            store::write_file(&file, &text("Reread", surface)).expect("the theme must be writable");
        };
        let surface = |read: &[Read]| read.first().map(|file| file.themes[0].surface.hex());
        let mut read = Vec::new();

        write("#102030");
        let first = reread(&directory, &mut read);
        let unchanged = reread(&directory, &mut read);
        let before = surface(&read);
        // In place, the same length and the same time, as a filesystem that
        // keeps coarse or stale times reports it: only the bytes differ.
        let modified = std::fs::metadata(&file).and_then(|metadata| metadata.modified());
        std::fs::write(&file, text("Reread", "#405060")).expect("the theme must be writable");
        std::fs::File::options()
            .write(true)
            .open(&file)
            .and_then(|handle| handle.set_modified(modified?))
            .expect("the time must be settable");
        let replaced = reread(&directory, &mut read);
        let after = surface(&read);
        // Unreadable for a pass -- here a directory in its place -- the file
        // keeps its theme and is read again on the next.
        std::fs::remove_file(&file).expect("the theme must be removable");
        std::fs::create_dir(&file).expect("the directory must be creatable");
        let unreadable = reread(&directory, &mut read);
        let held = surface(&read);
        std::fs::remove_dir(&file).expect("the directory must be removable");
        write("#a0b0c0");
        let retried = reread(&directory, &mut read);
        let recovered = surface(&read);
        std::fs::write(&file, r#"{ "name": "Rer"#).expect("the theme must be writable");
        let broken = reread(&directory, &mut read);
        let kept = surface(&read);
        let name = read[0].themes[0].name;
        write("#708090");
        reread(&directory, &mut read);
        let renamed_once = std::ptr::eq(name, read[0].themes[0].name);
        std::fs::remove_file(&file).expect("the theme must be removable");
        let removed = reread(&directory, &mut read);
        let _ = std::fs::remove_dir_all(&directory);

        assert!(first && !unchanged && replaced && broken && removed);
        assert_eq!(before.as_deref(), Some("#102030"));
        assert_eq!(after.as_deref(), Some("#405060"));
        assert!(!unreadable, "a file that does not read has not changed");
        assert_eq!(held, after);
        assert!(retried);
        assert_eq!(recovered.as_deref(), Some("#a0b0c0"));
        assert_eq!(kept, recovered);
        assert!(renamed_once, "a reread leaks no second copy of a name");
        assert!(read.is_empty());
    }

    #[test]
    fn a_directory_that_does_not_list_keeps_its_themes_and_one_removed_drops_them() {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-theme-unlisted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::remove_file(&directory);
        store::write_file(
            &directory.join("mine.json"),
            r##"{ "name": "Unlisted", "appearance": "dark", "colors": { "surface": "#102030" } }"##,
        )
        .expect("the theme must be writable");
        let mut read = Vec::new();
        reread(&directory, &mut read);

        // A file where the directory was fails to list, unlike one not there.
        std::fs::remove_dir_all(&directory).expect("the directory must be removable");
        std::fs::write(&directory, "").expect("the file must be writable");
        let unlisted = reread(&directory, &mut read);
        let held = read.len();
        std::fs::remove_file(&directory).expect("the file must be removable");
        let removed = reread(&directory, &mut read);

        assert!(!unlisted);
        assert_eq!(held, 1, "a listing that fails keeps every theme");
        assert!(
            removed && read.is_empty(),
            "a directory gone is an empty one"
        );
    }
}
