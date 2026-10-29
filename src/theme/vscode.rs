//! VS Code's colour themes, its own and its installed extensions', converted
//! onto dbdelve's [`Theme`].

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::color::{Rgba, contrast_ratio};
use super::zed::{
    apart_from, hex, keep_untaken, on_accent, read_text, see_through, sorted_entries,
};
use super::{PALETTE_EDITED_ALPHA, Theme};
use crate::store;

/// A theme file with its includes folded in.
#[derive(Default)]
struct Source {
    colors: Map<String, Value>,
    token_colors: Vec<Value>,
}

/// VS Code's own themes, then its extensions', each by name. A theme that does
/// not read or parse is skipped, and a name already taken goes to whichever
/// came first.
pub(super) fn discover() -> Vec<Theme> {
    let mut themes = Vec::new();
    for roots in [built_in_directories(), extension_directories()] {
        let mut group: Vec<Theme> = roots
            .iter()
            .flat_map(|root| sorted_entries(root))
            .flat_map(|extension| extension_themes(&extension))
            .collect();
        group.sort_by_key(|theme| theme.name.to_lowercase());
        themes.extend(group);
    }
    // ponytail: an extension VS Code has updated but not yet deleted (its
    // `.obsolete` list) is read twice, and either copy may win the name.
    keep_untaken(&mut themes, &mut Vec::new());
    themes
}

/// Where VS Code keeps the extensions it ships, its own themes among them.
fn built_in_directories() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    let installs = [
        Some(PathBuf::from("/Applications")),
        store::home().ok().map(|home| home.join("Applications")),
    ]
    .into_iter()
    .flatten()
    .map(|applications| applications.join("Visual Studio Code.app/Contents/Resources/app"));
    // The user installer, then the system-wide one.
    #[cfg(target_os = "windows")]
    let installs = [
        ("LOCALAPPDATA", "Programs\\Microsoft VS Code"),
        ("ProgramFiles", "Microsoft VS Code"),
    ]
    .into_iter()
    .filter_map(|(variable, install)| {
        let root = std::env::var_os(variable).filter(|value| !value.is_empty())?;
        Some(PathBuf::from(root).join(install).join("resources\\app"))
    });
    // The .deb and .rpm, the AUR's visual-studio-code-bin, then the snap.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let installs = [
        "/usr/share/code",
        "/opt/visual-studio-code",
        "/snap/code/current/usr/share/code",
    ]
    .map(|install| PathBuf::from(install).join("resources/app"));
    installs
        .into_iter()
        .map(|app| app.join("extensions"))
        .collect()
}

fn extension_directories() -> Vec<PathBuf> {
    store::home()
        .map(|home| home.join(".vscode").join("extensions"))
        .into_iter()
        .collect()
}

/// The colour themes an extension's `package.json` contributes.
fn extension_themes(extension: &Path) -> Vec<Theme> {
    let Ok(manifest) = read_text(&extension.join("package.json")).and_then(|text| parse(&text))
    else {
        return Vec::new();
    };
    let strings = read_text(&extension.join("package.nls.json"))
        .and_then(|text| parse(&text))
        .unwrap_or_default();
    let contributions = manifest
        .pointer("/contributes/themes")
        .and_then(Value::as_array);
    contributions
        .into_iter()
        .flatten()
        .filter_map(|contribution| {
            let file = contribution.get("path")?.as_str()?;
            // A TextMate plist, not JSON.
            if file.to_ascii_lowercase().ends_with(".tmtheme") {
                return None;
            }
            let label = contribution.get("label").or(contribution.get("id"))?;
            let light = matches!(
                contribution.get("uiTheme").and_then(Value::as_str),
                Some("vs" | "hc-light")
            );
            let path = extension.join(file);
            match load(&path, &read_text, 0) {
                Ok(source) => Some(convert(
                    localized(label.as_str()?, &strings),
                    light,
                    &source,
                )),
                Err(error) => {
                    eprintln!("skipping theme file {}: {error}", path.display());
                    None
                }
            }
        })
        .collect()
}

/// A `%key%` label names an entry in the extension's `package.nls.json`,
/// either the string itself or `{ "message": ... }`.
fn localized(label: &str, strings: &Value) -> String {
    label
        .strip_prefix('%')
        .and_then(|label| label.strip_suffix('%'))
        .and_then(|key| {
            let entry = strings.get(key)?;
            entry.as_str().or_else(|| entry.get("message")?.as_str())
        })
        .unwrap_or(label)
        .to_string()
}

fn parse(text: &str) -> Result<Value, String> {
    jsonc_parser::parse_to_serde_value(text, &Default::default()).map_err(|error| error.to_string())
}

/// The theme at `path` over whatever it `include`s, relative to itself: the
/// includer's `colors` override the included ones, and its `tokenColors`
/// follow them, so they win TextMate's ties.
fn load(
    path: &Path,
    read: &impl Fn(&Path) -> Result<String, String>,
    depth: usize,
) -> Result<Source, String> {
    // Deep enough for any real chain (Dark 2026 is four deep), and it ends a loop.
    const MAX_DEPTH: usize = 8;
    if depth > MAX_DEPTH {
        return Err("includes nest too deep, or loop".to_string());
    }
    let mut value = parse(&read(path)?)?;
    let mut source = match value.get("include").and_then(Value::as_str) {
        Some(include) => load(
            &path.parent().unwrap_or(path).join(include),
            read,
            depth + 1,
        )?,
        None => Source::default(),
    };
    if let Some(Value::Object(colors)) = value.get_mut("colors").map(Value::take) {
        source.colors.extend(colors);
    }
    // A string here names a `.tmTheme`, which is not read.
    if let Some(Value::Array(rules)) = value.get_mut("tokenColors").map(Value::take) {
        source.token_colors.extend(rules);
    }
    Ok(source)
}

fn convert(name: String, light: bool, source: &Source) -> Theme {
    let base = if light { Theme::light() } else { Theme::dark() };
    // A fully transparent colour is how a theme says "none", as Dark Modern
    // does for its secondary button.
    let first = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            let color = hex(source.colors.get(*key)?.as_str()?)?;
            (color.a > 0.0).then_some(color)
        })
    };
    let editor = first(&["editor.background"]).map(|c| c.flatten(base.bg));
    // Chrome matching the editor would make the active tab, which is painted
    // in the editor's tone, invisible.
    let surface = apart_from(
        first(&[
            "sideBar.background",
            "activityBar.background",
            "titleBar.activeBackground",
        ])
        .map_or(editor.unwrap_or(base.surface), |c| {
            c.flatten(editor.unwrap_or(base.surface))
        }),
        editor.unwrap_or(base.panel),
    );
    let solid = |keys: &[&str]| first(keys).map(|c| c.flatten(surface));
    let text = solid(&["editor.foreground", "foreground"]).unwrap_or(base.text);
    let backdrop = editor.unwrap_or(base.bg);
    // A link reads as text on the editor, which a button or a focus ring need
    // not: Gruvbox's focus border is a gray a step off its background.
    let accents: Vec<_> = ["textLink.foreground", "button.background", "focusBorder"]
        .into_iter()
        .filter_map(|key| solid(&[key]))
        .collect();
    let accent = accents
        .iter()
        .find(|accent| contrast_ratio(**accent, backdrop) >= 3.0)
        .or(accents.first())
        .copied()
        .unwrap_or(base.accent);
    let rules = &source.token_colors;
    let syntax = |scopes: &[&str]| {
        scopes
            .iter()
            .find_map(|scope| foreground(rules, scope))
            .map(|c| c.flatten(backdrop))
    };
    // A rule with no scope is the editor's default foreground.
    let plain = rules
        .iter()
        .rev()
        .filter(|rule| rule.get("scope").is_none())
        .find_map(|rule| hex(rule.pointer("/settings/foreground")?.as_str()?))
        .map_or(text, |c| c.flatten(backdrop));
    // ponytail: leaked, but themes load once per process so it is bounded.
    let name: &'static str = Box::leak(name.into_boxed_str());
    Theme {
        name,
        bg: editor.unwrap_or(base.bg),
        panel: editor.unwrap_or(base.panel),
        surface,
        overlay: solid(&[
            "editorWidget.background",
            "dropdown.background",
            "quickInput.background",
        ])
        .unwrap_or(base.overlay),
        // dbdelve writes `text` on a control, where VS Code writes the
        // button's own foreground: Poimandres's secondary button is its text
        // colour. And Gruvbox's input is its editor, which no button stands off.
        control: [
            "button.secondaryBackground",
            "input.background",
            "dropdown.background",
        ]
        .into_iter()
        .filter_map(|key| solid(&[key]))
        .find(|control| contrast_ratio(text, *control) >= 4.5)
        .map_or(base.control, |control| apart_from(control, backdrop)),
        element_hover: first(&["list.hoverBackground"]).unwrap_or(base.element_hover),
        element_active: first(&[
            "list.activeSelectionBackground",
            "list.inactiveSelectionBackground",
        ])
        .unwrap_or(base.element_active),
        border: first(&[
            "editorGroup.border",
            "panel.border",
            "sideBar.border",
            "contrastBorder",
        ])
        .unwrap_or(base.border),
        // The edge of a dialog or an input. Not `focusBorder`, which is
        // usually the accent, nor `dropdown.border`, which Catppuccin makes it.
        border_strong: first(&[
            "input.border",
            "widget.border",
            "editorWidget.border",
            "contrastBorder",
        ])
        .unwrap_or(base.border_strong),
        text,
        text_muted: solid(&["descriptionForeground"]).unwrap_or(base.text_muted),
        text_faint: solid(&["editorLineNumber.foreground"]).unwrap_or(base.text_faint),
        accent,
        on_accent: on_accent(text, surface, accent),
        selection: first(&["editor.selectionBackground"])
            .map_or(base.selection, |c| see_through(c, surface)),
        cursor: solid(&["editorCursor.foreground"]).unwrap_or(text),
        edited: solid(&["editorWarning.foreground"])
            .map_or(base.edited, |c| c.alpha(PALETTE_EDITED_ALPHA)),
        danger: solid(&["errorForeground", "editorError.foreground"]).unwrap_or(base.danger),
        // Gruvbox's added-file colour is its plain foreground.
        success: solid(&[
            "terminal.ansiGreen",
            "editorGutter.addedBackground",
            "gitDecoration.addedResourceForeground",
        ])
        .unwrap_or(base.success),
        syntax_comment: syntax(&["comment"]).unwrap_or(base.syntax_comment),
        syntax_keyword: syntax(&["keyword.control", "keyword", "storage"])
            .unwrap_or(base.syntax_keyword),
        syntax_string: syntax(&["string"]).unwrap_or(base.syntax_string),
        syntax_number: syntax(&["constant.numeric", "constant"]).unwrap_or(base.syntax_number),
        syntax_function: syntax(&["entity.name.function", "support.function"])
            .unwrap_or(base.syntax_function),
        syntax_type: syntax(&["entity.name.type", "support.type", "storage.type"])
            .unwrap_or(base.syntax_type),
        syntax_variable: syntax(&["variable", "variable.other"]).unwrap_or(plain),
        syntax_operator: syntax(&["keyword.operator"]).unwrap_or(plain),
        ..base
    }
}

/// The foreground TextMate gives `scope`: the rule whose selector is the
/// longest dot-wise prefix of it, a later rule winning a tie. A selector with a
/// descendant or an exclusion (`source.sql keyword`, `string -comment`) has a
/// space in it, so it is never a prefix and never matches.
fn foreground(rules: &[Value], scope: &str) -> Option<Rgba> {
    let mut best: Option<(usize, Rgba)> = None;
    for rule in rules {
        let Some(color) = rule
            .pointer("/settings/foreground")
            .and_then(Value::as_str)
            .and_then(hex)
        else {
            continue;
        };
        let selectors = match rule.get("scope") {
            Some(Value::String(selectors)) => vec![selectors.as_str()],
            Some(Value::Array(selectors)) => selectors.iter().filter_map(Value::as_str).collect(),
            _ => continue,
        };
        for selector in selectors.iter().flat_map(|s| s.split(',')).map(str::trim) {
            let matches = scope
                .strip_prefix(selector)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'));
            if matches && best.is_none_or(|(length, _)| selector.len() >= length) {
                best = Some((selector.len(), color));
            }
        }
    }
    best.map(|(_, color)| color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::color::Srgb;

    #[test]
    fn a_vscode_theme_folds_in_its_include_and_matches_scopes_textmate_style() {
        let base = r##"{
            "colors": { "editor.background": "#101010", "editor.foreground": "#c0c0c0" },
            "tokenColors": [{ "scope": "keyword", "settings": { "foreground": "#aa0000" } }]
        }"##;
        let theme = r##"{
            // The includer's colours win, and its rules follow the included ones.
            "include": "./base.json",
            "colors": { "editor.background": "#202020", "sideBar.background": "#0000", },
            "tokenColors": [
                { "settings": { "foreground": "#dddddd" } },
                { "scope": ["comment", "string.quoted"], "settings": { "foreground": "#00aa00" } },
                { "scope": "keyword.control, constant", "settings": { "foreground": "#0000aa" } },
                { "scope": "keyword", "settings": { "foreground": "#aaaa00" } },
                { "scope": "source.sql keyword.control.x", "settings": { "foreground": "#fff" } },
            ],
        }"##;
        let files = |path: &Path| match path.file_name().and_then(|name| name.to_str()) {
            Some("base.json") => Ok(base.to_string()),
            Some("theme.json") => Ok(theme.to_string()),
            _ => Err("missing".to_string()),
        };
        let source = load(Path::new("themes/theme.json"), &files, 0).unwrap();
        let theme = convert("Test".to_string(), false, &source);
        assert_eq!(theme.bg, Srgb::from_hex(0x202020));
        assert_eq!(theme.text, Srgb::from_hex(0xc0c0c0));
        // A transparent sidebar is no sidebar: chrome comes off the editor.
        assert_ne!(theme.surface, theme.bg);
        assert_eq!(theme.syntax_comment, Srgb::from_hex(0x00aa00));
        assert_eq!(theme.syntax_keyword, Srgb::from_hex(0x0000aa));
        assert_eq!(theme.syntax_number, Srgb::from_hex(0x0000aa));
        assert_eq!(theme.syntax_variable, Srgb::from_hex(0xdddddd));
        assert_eq!(theme.syntax_string, Theme::dark().syntax_string);

        let looping = |_: &Path| Ok(r#"{ "include": "./loop.json" }"#.to_string());
        assert!(load(Path::new("loop.json"), &looping, 0).is_err());
    }
}
