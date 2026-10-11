//! The themes in dbdelve's own [`store::theme_directory`]: read at startup,
//! and added to at runtime by importing one from another app.

use std::path::Path;
use std::sync::{OnceLock, PoisonError, RwLock};

use super::{Theme, vscode, zed};
use crate::store;

static USER_THEMES: RwLock<Vec<Theme>> = RwLock::new(Vec::new());

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
        static FOUND: [OnceLock<Vec<Theme>>; 2] = [const { OnceLock::new() }; 2];
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

    /// The themes as they would paint, for the import list to preview.
    pub fn themes(self) -> impl Iterator<Item = Theme> {
        self.found().iter().copied().map(zed::finished)
    }
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
    import_into(&store::theme_directory()?, flat)
}

fn import_into(directory: &Path, flat: Theme) -> Result<Theme, String> {
    let mut themes = USER_THEMES.write().unwrap_or_else(PoisonError::into_inner);
    let existing = themes
        .iter()
        .position(|theme| theme.name.eq_ignore_ascii_case(flat.name));
    let path = directory.join(format!("{}.json", slug(flat.name)));
    store::write_file(&path, &zed::native_text(&flat))?;
    let theme = zed::finished(flat);
    match existing {
        Some(index) => themes[index] = theme,
        None => themes.push(theme),
    }
    Ok(theme)
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
    fn an_import_is_written_natively_and_listed_under_a_free_name() {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-theme-import-{}", std::process::id()));
        // Light, so it lands after every light theme as the listing tests
        // expect of whatever `Theme::all` holds while they run.
        let flat = Theme {
            name: "Zed: One Light",
            ..Theme::light()
        };
        let imported = import_into(&directory, flat);
        let again = import_into(&directory, flat);
        let written = std::fs::read_to_string(directory.join("zed-one-light.json"));
        let _ = std::fs::remove_dir_all(&directory);

        assert_eq!(imported.map(|theme| theme.name), Ok("Zed: One Light"));
        assert_eq!(again.map(|theme| theme.name), Ok("Zed: One Light"));
        let parsed = zed::parse_own(&written.unwrap()).unwrap();
        assert_eq!(parsed[0].name, "Zed: One Light");
        let listed = Theme::all();
        let named = |name| listed.iter().filter(|theme| theme.name == name).count();
        assert_eq!(named("Zed: One Light"), 1);
        assert_eq!(named("One Light"), 1);
        assert!(is_imported("Zed: One Light"));
    }
}
