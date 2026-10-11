//! User themes, read once at startup: dbdelve's own [`store::theme_directory`],
//! then Zed's themes directory and its installed extensions.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

use super::color::{Rgba, Srgb, contrast_ratio};
use super::{PALETTE_EDITED_ALPHA, PALETTE_SELECTION_ALPHA, Theme};
use crate::store;

struct Loaded {
    own: Vec<Theme>,
    zed: Vec<Theme>,
}

static USER_THEMES: OnceLock<Loaded> = OnceLock::new();

#[derive(Deserialize)]
struct Family {
    #[serde(default)]
    themes: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    #[serde(default)]
    appearance: String,
    // A `Value` rather than a struct: every key is optional, any can be null,
    // and `players` and `syntax` nest, so a JSON pointer reads all of them alike.
    #[serde(default)]
    style: Value,
}

/// The themes [`load_user_themes`] read, dbdelve's own before Zed's, or none
/// if it has not run.
pub(super) fn user_themes() -> impl Iterator<Item = Theme> {
    USER_THEMES
        .get()
        .into_iter()
        .flat_map(|loaded| loaded.own.iter().chain(&loaded.zed))
        .copied()
}

pub(crate) fn is_from_zed(name: &str) -> bool {
    USER_THEMES
        .get()
        .is_some_and(|loaded| loaded.zed.iter().any(|theme| theme.name == name))
}

/// Read every theme file once. A file that does not read or parse is
/// skipped, never fatal, and a name already taken goes to whichever came
/// first: built-in, then dbdelve's directory, then Zed's, then its extensions.
pub(crate) fn load_user_themes() {
    let mut own = read(
        store::theme_directory()
            .into_iter()
            .flat_map(|directory| json_files(&directory)),
        parse_own,
    );
    let zed_themes = store::zed_theme_directory()
        .into_iter()
        .flat_map(|directory| json_files(&directory));
    let zed_extensions = store::zed_extension_directory()
        .into_iter()
        .flat_map(|root| sorted_entries(&root))
        .flat_map(|extension| json_files(&extension.join("themes")));
    let mut zed = read(zed_themes.chain(zed_extensions), parse_zed);
    let mut taken = Theme::built_in().map(|theme| theme.name).to_vec();
    keep_untaken(&mut own, &mut taken);
    keep_untaken(&mut zed, &mut taken);
    let _ = USER_THEMES.set(Loaded { own, zed });
}

fn sorted_entries(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

fn json_files(directory: &Path) -> Vec<PathBuf> {
    let mut paths = sorted_entries(directory);
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    paths
}

fn read(
    files: impl Iterator<Item = PathBuf>,
    parse: fn(&str) -> Result<Vec<Theme>, String>,
) -> Vec<Theme> {
    let mut themes = Vec::new();
    for path in files {
        match std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| parse(&text))
        {
            Ok(parsed) => themes.extend(parsed),
            Err(error) => eprintln!("skipping theme file {}: {error}", path.display()),
        }
    }
    themes
}

/// Drop every theme whose name is taken, case-blind as `restored_theme` is,
/// and take the names of the ones kept.
fn keep_untaken(themes: &mut Vec<Theme>, taken: &mut Vec<&'static str>) {
    themes.retain(|theme| {
        let fresh = !taken
            .iter()
            .any(|name| name.eq_ignore_ascii_case(theme.name));
        if fresh {
            taken.push(theme.name);
        }
        fresh
    });
}

/// A file in dbdelve's own directory: a Zed family, or one native theme.
fn parse_own(text: &str) -> Result<Vec<Theme>, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    if value.get("themes").is_some() {
        return zed_family(value);
    }
    native(&value)
        .map(|theme| vec![theme])
        .ok_or_else(|| "neither a Zed theme family nor a theme with a name".to_string())
}

fn parse_zed(text: &str) -> Result<Vec<Theme>, String> {
    zed_family(serde_json::from_str(text).map_err(|error| error.to_string())?)
}

fn zed_family(value: Value) -> Result<Vec<Theme>, String> {
    let family: Family = serde_json::from_value(value).map_err(|error| error.to_string())?;
    Ok(family.themes.into_iter().map(convert).collect())
}

fn base(appearance: &str) -> Theme {
    if appearance == "light" {
        Theme::light()
    } else {
        Theme::dark()
    }
}

/// `{ "name", "appearance", "glass", "colors": { <Theme field>: hex } }`.
fn native(value: &Value) -> Option<Theme> {
    // ponytail: leaked, but themes load once per process so it is bounded.
    let name: &'static str = Box::leak(value.get("name")?.as_str()?.into());
    let appearance = value.get("appearance").and_then(Value::as_str);
    let mut theme = Theme {
        name,
        ..base(appearance.unwrap_or_default())
    };
    let pick = |field: &str| color(value, &format!("/colors/{field}"));
    macro_rules! fill {
        ($($solid:ident),*; $($wash:ident),*) => {
            $(if let Some(c) = pick(stringify!($solid)) {
                theme.$solid = c.flatten(theme.surface);
            })*
            $(if let Some(c) = pick(stringify!($wash)) {
                theme.$wash = c;
            })*
        };
    }
    // `surface` first: it is what a translucent solid is flattened over.
    fill!(
        surface, bg, panel, overlay, control, text, text_muted, text_faint, accent, on_accent,
        cursor, danger, success, syntax_comment, syntax_keyword, syntax_string, syntax_number,
        syntax_function, syntax_type, syntax_variable, syntax_operator;
        element_hover, element_active, border, border_strong, selection, edited
    );
    Some(if value.get("glass") == Some(&Value::Bool(true)) {
        theme.glassed(name)
    } else {
        theme
    })
}

fn convert(entry: Entry) -> Theme {
    let base = base(&entry.appearance);
    let style = &entry.style;
    let rgba = |pointer: &str| color(style, pointer);
    let glass = matches!(
        style
            .pointer("/background.appearance")
            .and_then(Value::as_str),
        Some("blurred" | "transparent")
    );
    // A glass theme's background is translucent so the desktop shows through;
    // glassing re-tones its hue, which compositing would have washed out.
    let surface = rgba("/background").map_or(base.surface, |c| {
        if glass {
            Srgb::new(c.r, c.g, c.b)
        } else {
            c.flatten(base.surface)
        }
    });
    let solid = |pointer: &str| rgba(pointer).map(|c| c.flatten(surface));
    let editor = solid("/editor.background");
    let text = solid("/text").unwrap_or(base.text);
    let cursor = solid("/players/0/cursor");
    let accent = solid("/text.accent").or(cursor).unwrap_or(base.accent);
    let syntax = |name: &str| solid(&format!("/syntax/{name}/color"));
    let name: &'static str = Box::leak(entry.name.into_boxed_str());
    let theme = Theme {
        name,
        bg: editor.unwrap_or(base.bg),
        panel: editor.unwrap_or(base.panel),
        surface,
        overlay: solid("/elevated_surface.background").unwrap_or(base.overlay),
        control: solid("/element.background").unwrap_or(base.control),
        element_hover: rgba("/element.hover").unwrap_or(base.element_hover),
        element_active: rgba("/element.active").unwrap_or(base.element_active),
        border: rgba("/border.variant").unwrap_or(base.border),
        border_strong: rgba("/border").unwrap_or(base.border_strong),
        text,
        text_muted: solid("/text.muted").unwrap_or(base.text_muted),
        text_faint: solid("/text.placeholder").unwrap_or(base.text_faint),
        accent,
        // Zed has no on-accent colour; take whichever of its two ends reads
        // better on the accent.
        on_accent: if contrast_ratio(text, accent) >= contrast_ratio(surface, accent) {
            text
        } else {
            surface
        },
        // An opaque selection would hide the text it selects.
        selection: rgba("/players/0/selection").map_or(base.selection, |c| {
            if c.a < 1.0 {
                c
            } else {
                c.flatten(surface).alpha(PALETTE_SELECTION_ALPHA)
            }
        }),
        cursor: cursor.unwrap_or(base.cursor),
        edited: solid("/warning").map_or(base.edited, |c| c.alpha(PALETTE_EDITED_ALPHA)),
        danger: solid("/error").unwrap_or(base.danger),
        success: solid("/success")
            .or(solid("/created"))
            .unwrap_or(base.success),
        syntax_comment: syntax("comment").unwrap_or(base.syntax_comment),
        syntax_keyword: syntax("keyword").unwrap_or(base.syntax_keyword),
        syntax_string: syntax("string").unwrap_or(base.syntax_string),
        syntax_number: syntax("number").unwrap_or(base.syntax_number),
        syntax_function: syntax("function").unwrap_or(base.syntax_function),
        syntax_type: syntax("type").unwrap_or(base.syntax_type),
        syntax_variable: syntax("variable").unwrap_or(base.syntax_variable),
        syntax_operator: syntax("operator").unwrap_or(base.syntax_operator),
        ..base
    };
    if glass { theme.glassed(name) } else { theme }
}

/// `#RGB`, `#RRGGBB` or `#RRGGBBAA` at `pointer`; anything else is absent.
fn color(style: &Value, pointer: &str) -> Option<Rgba> {
    let hex = style.pointer(pointer)?.as_str()?.strip_prefix('#')?;
    let hex = match hex.len() {
        3 => hex
            .chars()
            .flat_map(|c| [c, c])
            .chain("ff".chars())
            .collect(),
        6 => format!("{hex}ff"),
        8 => hex.to_string(),
        _ => return None,
    };
    let value = u32::from_str_radix(&hex, 16).ok()?;
    Some(Srgb::from_hex(value >> 8).alpha((value & 0xff) as f32 / 255.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Appearance;

    #[test]
    fn a_zed_theme_maps_onto_its_appearance_base() {
        let mut themes = parse_own(
            r##"{
                "name": "Family",
                "themes": [
                    {
                        "name": "Zed Test",
                        "appearance": "light",
                        "style": {
                            "background": "#eeeeee",
                            "editor.background": "#ffffff",
                            "text": "#123",
                            "text.accent": null,
                            "border": "#00000033",
                            "accents": ["#ff0000"],
                            "players": [{ "cursor": "#0055aa", "selection": "#0055aa40" }],
                            "syntax": { "keyword": { "color": "#aa0055", "font_weight": 700 } }
                        }
                    },
                    { "name": "DBDelve Dark", "style": {} }
                ]
            }"##,
        )
        .unwrap();
        keep_untaken(&mut themes, &mut vec!["dbdelve dark"]);
        let [theme] = themes.as_slice() else {
            panic!("the built-in name should be skipped: {themes:?}");
        };
        let light = Theme::light();
        assert_eq!(theme.name, "Zed Test");
        assert_eq!(theme.appearance, Appearance::Light);
        assert_eq!(theme.bg, Srgb::from_hex(0xffffff));
        assert_eq!(theme.surface, Srgb::from_hex(0xeeeeee));
        assert_eq!(theme.text, Srgb::from_hex(0x112233));
        assert_eq!(
            theme.border_strong,
            Srgb::from_hex(0x000000).alpha(0x33 as f32 / 255.0)
        );
        // A null accent falls back to the cursor.
        assert_eq!(theme.accent, Srgb::from_hex(0x0055aa));
        assert_eq!(theme.selection.a, 0x40 as f32 / 255.0);
        assert_eq!(theme.syntax_keyword, Srgb::from_hex(0xaa0055));
        assert_eq!(theme.syntax_string, light.syntax_string);
        assert_eq!(theme.danger, light.danger);
    }

    #[test]
    fn a_blurred_zed_theme_comes_out_glass() {
        let themes = parse_zed(
            r##"{ "themes": [{ "name": "Blur", "style": {
                "background.appearance": "blurred",
                "background": "#1e1e2ecc"
            } }] }"##,
        )
        .unwrap();
        assert!(themes[0].is_glass);
        assert_eq!(themes[0].name, "Blur");
    }

    #[test]
    fn a_native_theme_fills_its_fields_and_falls_back_to_its_base() {
        let [theme] = parse_own(
            r##"{
                "name": "Native",
                "appearance": "light",
                "unknown": 1,
                "colors": { "bg": "#fafafa", "border": "#00000020", "nonsense": "#fff" }
            }"##,
        )
        .unwrap()[..] else {
            panic!("one theme per native file");
        };
        assert_eq!(theme.name, "Native");
        assert!(!theme.is_glass);
        assert_eq!(theme.bg, Srgb::from_hex(0xfafafa));
        assert_eq!(theme.border, Srgb::from_hex(0).alpha(0x20 as f32 / 255.0));
        assert_eq!(theme.text, Theme::light().text);

        let glass = parse_own(r#"{ "name": "Native Glass", "glass": true }"#).unwrap();
        assert!(glass[0].is_glass);
        assert_eq!(glass[0].appearance, Appearance::Dark);
    }
}
