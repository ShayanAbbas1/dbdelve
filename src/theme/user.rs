//! The themes in dbdelve's own [`store::theme_directory`]: read at startup,
//! and added to at runtime by importing one from another app.

use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock, PoisonError, RwLock};

use super::{Theme, vscode, zed};
use crate::store;

static USER_THEMES: RwLock<Vec<Theme>> = RwLock::new(Vec::new());
static FOUND: [OnceLock<Vec<Theme>>; 2] = [const { OnceLock::new() }; 2];

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

/// Every theme in dbdelve's directory, as of startup plus anything imported.
pub(super) fn themes() -> Vec<Theme> {
    USER_THEMES
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Read every file in dbdelve's directory. A file that does not read or parse
/// is skipped, never fatal, and a name already taken goes to whichever came
/// first: built-in, then the files in name order.
pub(crate) fn load() {
    let mut themes = zed::read(
        store::theme_directory()
            .into_iter()
            .flat_map(|directory| zed::json_files(&directory)),
        zed::parse_own,
    );
    zed::keep_untaken(
        &mut themes,
        &mut Theme::built_in().map(|theme| theme.name).to_vec(),
    );
    *USER_THEMES.write().unwrap_or_else(PoisonError::into_inner) = themes;
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
    let theme = zed::finished(flat);
    let mut themes = USER_THEMES.write().unwrap_or_else(PoisonError::into_inner);
    match themes
        .iter()
        .position(|listed| listed.name.eq_ignore_ascii_case(theme.name))
    {
        Some(index) => themes[index] = theme,
        None => themes.push(theme),
    }
    Ok(theme)
}

/// Into the file that already holds a theme of its name, the one [`load`]
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
}
