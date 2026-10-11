//! The themes in dbdelve's own [`store::theme_directory`]: read at startup,
//! and added to at runtime by importing one from another app.

use std::path::Path;
use std::sync::{PoisonError, RwLock};

use super::{Theme, zed};
use crate::store;

static USER_THEMES: RwLock<Vec<Theme>> = RwLock::new(Vec::new());

/// An app whose themes can be imported: a variant, its name, and the call
/// that finds its themes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Zed,
}

impl Source {
    pub const ALL: [Self; 1] = [Self::Zed];

    pub fn name(self) -> &'static str {
        match self {
            Self::Zed => "Zed",
        }
    }

    /// Every theme this source has on this machine, unglassed, one per name,
    /// in the order the import list shows them. Scanned once per process.
    fn found(self) -> &'static [Theme] {
        match self {
            Self::Zed => zed::discover(),
        }
    }

    /// The themes as they would paint, for the import list to preview.
    pub fn themes(self) -> impl Iterator<Item = Theme> {
        self.found().iter().copied().map(zed::finished)
    }

    /// What a theme is called once imported: its own name, unless one of
    /// dbdelve's own already has it, as dbdelve's One Dark has Zed's.
    pub fn imported_name(self, name: &str) -> String {
        let taken = Theme::built_in()
            .iter()
            .any(|theme| theme.name.eq_ignore_ascii_case(name));
        if taken {
            format!("{name} ({})", self.name())
        } else {
            name.to_string()
        }
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
    import_into(&store::theme_directory()?, source, flat)
}

fn import_into(directory: &Path, source: Source, flat: Theme) -> Result<Theme, String> {
    let mut themes = USER_THEMES.write().unwrap_or_else(PoisonError::into_inner);
    let name = source.imported_name(flat.name);
    let existing = themes
        .iter()
        .position(|theme| theme.name.eq_ignore_ascii_case(&name));
    let name: &'static str = match existing {
        Some(index) if themes[index].name == name => themes[index].name,
        _ if name == flat.name => flat.name,
        // ponytail: leaked, bounded by the imports made in one run.
        _ => Box::leak(name.into_boxed_str()),
    };
    let named = Theme { name, ..flat };
    let path = directory.join(format!("{}.json", slug(name)));
    store::write_file(&path, &zed::native_text(&named))?;
    let theme = zed::finished(named);
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
        assert_eq!(Source::Zed.imported_name("Ayu Dark"), "Ayu Dark");
        let directory =
            std::env::temp_dir().join(format!("dbdelve-theme-import-{}", std::process::id()));
        // Light, so it lands after every light theme as the listing tests
        // expect of whatever `Theme::all` holds while they run.
        let flat = Theme {
            name: "One Light",
            ..Theme::light()
        };
        let imported = import_into(&directory, Source::Zed, flat);
        let again = import_into(&directory, Source::Zed, flat);
        let written = std::fs::read_to_string(directory.join("one-light-zed.json"));
        let _ = std::fs::remove_dir_all(&directory);

        assert_eq!(imported.map(|theme| theme.name), Ok("One Light (Zed)"));
        assert_eq!(again.map(|theme| theme.name), Ok("One Light (Zed)"));
        let parsed = zed::parse_own(&written.unwrap()).unwrap();
        assert_eq!(parsed[0].name, "One Light (Zed)");
        let listed = Theme::all();
        let named = |name| listed.iter().filter(|theme| theme.name == name).count();
        assert_eq!(named("One Light (Zed)"), 1);
        assert_eq!(named("One Light"), 1);
        assert!(is_imported("One Light (Zed)"));
    }
}
