//! Zed's theme format, and the themes Zed has on this machine for
//! [`super::user::Source::Zed`] to import: its own defaults, which are compiled
//! into Zed rather than installed, its themes directory and its extensions.
//! dbdelve's native format is read and written here too, since a file in
//! dbdelve's own directory may be either.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use super::color::{Oklch, Rgba, Srgb, contrast_ratio};
use super::{Appearance, PALETTE_EDITED_ALPHA, PALETTE_SELECTION_ALPHA, Theme};
use crate::store;

/// Zed's defaults, from `assets/themes` in its repository.
const BUNDLED: [&str; 3] = [
    include_str!("../../assets/themes/zed/ayu.json"),
    include_str!("../../assets/themes/zed/gruvbox.json"),
    include_str!("../../assets/themes/zed/one.json"),
];

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

/// Every Zed theme on this machine, unglassed. A name goes to whichever has it
/// first: Zed's themes directory, its extensions, then its defaults, so a
/// user's own copy of a default wins.
pub(super) fn discover() -> Vec<Theme> {
    let user = store::zed_theme_directory()
        .into_iter()
        .flat_map(|directory| json_files(&directory));
    let extensions = store::zed_extension_directory()
        .into_iter()
        .flat_map(|root| sorted_entries(&root))
        .flat_map(|extension| json_files(&extension.join("themes")));
    let mut themes = read(user.chain(extensions), parse_zed);
    themes.extend(
        BUNDLED
            .iter()
            .flat_map(|text| parse_zed(text).unwrap_or_default()),
    );
    keep_untaken(&mut themes, &mut Vec::new());
    themes.sort_by_key(|theme| theme.name.to_lowercase());
    themes
}

pub(super) fn sorted_entries(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    paths.sort();
    paths
}

pub(super) fn json_files(directory: &Path) -> Vec<PathBuf> {
    let mut paths = sorted_entries(directory);
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    paths
}

pub(super) fn read(
    files: impl Iterator<Item = PathBuf>,
    parse: fn(&str) -> Result<Vec<Theme>, String>,
) -> Vec<Theme> {
    let mut themes = Vec::new();
    for path in files {
        match read_text(&path).and_then(|text| parse(&text)) {
            Ok(parsed) => themes.extend(parsed),
            Err(error) => eprintln!("skipping theme file {}: {error}", path.display()),
        }
    }
    themes
}

pub(super) fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| error.to_string())
}

/// Drop every theme whose name is taken, case-blind as `restored_theme` is,
/// and take the names of the ones kept.
pub(super) fn keep_untaken(themes: &mut Vec<Theme>, taken: &mut Vec<&'static str>) {
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

/// A file in dbdelve's own directory: a Zed family, or one native theme. A
/// glass theme comes out glassed.
pub(super) fn parse_own(text: &str) -> Result<Vec<Theme>, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let flat = if value.get("themes").is_some() {
        zed_family(value)?
    } else {
        vec![native(&value).ok_or("neither a Zed theme family nor a theme with a name")?]
    };
    Ok(flat.into_iter().map(finished).collect())
}

/// A theme as it is painted. An unglassed theme carries `is_glass` as the
/// intent only: its planes are the ones glassing tones, which is also what a
/// native file holds, so loading one glasses it exactly once.
pub(super) fn finished(theme: Theme) -> Theme {
    if theme.is_glass {
        theme.glassed(theme.name)
    } else {
        theme
    }
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

/// Every colour a native file names, solid planes and inks before the
/// translucent washes, for `$then` to fill or write.
macro_rules! native_colors {
    ($then:ident) => {
        // `surface` first: it is what a translucent solid is flattened over.
        $then!(
            surface, bg, panel, overlay, control, text, text_muted, text_faint, accent, on_accent,
            cursor, danger, success, syntax_comment, syntax_keyword, syntax_string, syntax_number,
            syntax_function, syntax_type, syntax_variable, syntax_operator;
            element_hover, element_active, border, border_strong, selection, edited
        )
    };
}

/// `{ "name", "appearance", "glass", "colors": { <Theme field>: hex } }`,
/// unglassed.
fn native(value: &Value) -> Option<Theme> {
    // ponytail: leaked, bounded by the files read at startup and on each import.
    let name: &'static str = Box::leak(value.get("name")?.as_str()?.into());
    let appearance = value.get("appearance").and_then(Value::as_str);
    let mut theme = Theme {
        name,
        is_glass: value.get("glass") == Some(&Value::Bool(true)),
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
    native_colors!(fill);
    Some(theme)
}

/// The inverse of [`native`]: every colour, so the file does not lean on
/// whichever base a later dbdelve falls back to.
pub(super) fn native_text(theme: &Theme) -> String {
    let mut colors = serde_json::Map::new();
    macro_rules! put {
        ($($solid:ident),*; $($wash:ident),*) => {
            $(colors.insert(stringify!($solid).into(), theme.$solid.hex().into());)*
            $(colors.insert(stringify!($wash).into(), rgba_hex(theme.$wash).into());)*
        };
    }
    native_colors!(put);
    let file = json!({
        "name": theme.name,
        "appearance": match theme.appearance {
            Appearance::Dark => "dark",
            Appearance::Light => "light",
        },
        "glass": theme.is_glass,
        "colors": colors,
    });
    serde_json::to_string_pretty(&file).unwrap_or_default()
}

fn rgba_hex(c: Rgba) -> String {
    let alpha = (c.a.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("{}{alpha:02x}", Srgb::new(c.r, c.g, c.b).hex())
}

/// Unglassed, as [`finished`] takes it.
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
    let background = rgba("/background").map_or(base.surface, |c| {
        if glass {
            Srgb::new(c.r, c.g, c.b)
        } else {
            c.flatten(base.surface)
        }
    });
    let editor = rgba("/editor.background").map(|c| c.flatten(background));
    // Zed's `background` is what sits behind its panes, and many themes set it
    // to the editor's own colour; its chrome is the panel and the title bar.
    // Chrome matching the editor would make the active tab, which is painted
    // in the editor's tone, invisible.
    let surface = if glass {
        background
    } else {
        let chrome = rgba("/panel.background")
            .or(rgba("/title_bar.background"))
            .map_or(background, |c| c.flatten(background));
        apart_from(chrome, editor.unwrap_or(base.panel))
    };
    let solid = |pointer: &str| rgba(pointer).map(|c| c.flatten(surface));
    let text = solid("/text").unwrap_or(base.text);
    let cursor = solid("/players/0/cursor");
    let accent = solid("/text.accent").or(cursor).unwrap_or(base.accent);
    let syntax = |name: &str| solid(&format!("/syntax/{name}/color"));
    Theme {
        name: Box::leak(entry.name.into_boxed_str()),
        is_glass: glass,
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
        on_accent: on_accent(text, surface, accent),
        selection: rgba("/players/0/selection").map_or(base.selection, |c| see_through(c, surface)),
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
    }
}

/// `chrome`, unless it sits too close to `editor` to tell the two apart, as
/// in GitHub Dark High Contrast, where they are one colour. Then it is moved
/// a step off the editor in lightness, keeping the side of it the theme put it
/// on, or the side with room on a near-black or near-white editor.
pub(super) fn apart_from(chrome: Srgb, editor: Srgb) -> Srgb {
    const MIN_RATIO: f32 = 1.05;
    if contrast_ratio(chrome, editor) >= MIN_RATIO {
        return chrome;
    }
    let (own, edge) = (Oklch::from_srgb(chrome), Oklch::from_srgb(editor));
    let lighter = match own.l.partial_cmp(&edge.l) {
        _ if edge.l > 0.9 => false,
        _ if edge.l < 0.1 => true,
        Some(std::cmp::Ordering::Equal) | None => edge.l < 0.5,
        Some(order) => order.is_gt(),
    };
    let step = if lighter { 0.01 } else { -0.01 };
    (1..=20)
        .map(|n| Oklch::new(edge.l + step * n as f32, own.c, own.h).to_srgb())
        .find(|candidate| contrast_ratio(*candidate, editor) >= MIN_RATIO)
        .unwrap_or(chrome)
}

/// Neither Zed nor VS Code names a colour to write on the accent; take
/// whichever of the theme's two ends reads better on it.
pub(super) fn on_accent(text: Srgb, surface: Srgb, accent: Srgb) -> Srgb {
    if contrast_ratio(text, accent) >= contrast_ratio(surface, accent) {
        text
    } else {
        surface
    }
}

/// `selection`, made translucent if it is not: an opaque selection would hide
/// the text it selects.
pub(super) fn see_through(selection: Rgba, surface: Srgb) -> Rgba {
    if selection.a < 1.0 {
        selection
    } else {
        selection.flatten(surface).alpha(PALETTE_SELECTION_ALPHA)
    }
}

fn color(style: &Value, pointer: &str) -> Option<Rgba> {
    hex(style.pointer(pointer)?.as_str()?)
}

/// `#RGB`, `#RGBA`, `#RRGGBB` or `#RRGGBBAA`; anything else is absent.
pub(super) fn hex(text: &str) -> Option<Rgba> {
    let hex = text.strip_prefix('#')?;
    // `from_str_radix` would take a leading `+` as a sign.
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let hex = match hex.len() {
        3 => hex.chars().chain(['f']).flat_map(|c| [c, c]).collect(),
        4 => hex.chars().flat_map(|c| [c, c]).collect(),
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
        assert!(hex("#+abcde").is_none());
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
    fn chrome_stays_apart_from_the_editor_so_the_active_tab_shows() {
        let themes = parse_zed(
            r##"{ "themes": [
                { "name": "Panel", "style": {
                    "background": "#0d1117", "editor.background": "#0d1117",
                    "panel.background": "#010409"
                } },
                { "name": "Flat", "style": {
                    "background": "#010409", "editor.background": "#010409",
                    "panel.background": "#010409"
                } }
            ] }"##,
        )
        .unwrap();
        assert_eq!(themes[0].surface, Srgb::from_hex(0x010409));
        assert_ne!(themes[1].surface, themes[1].panel);
    }

    #[test]
    fn a_native_file_round_trips_and_glasses_once() {
        let themes = parse_zed(
            r##"{ "themes": [
                { "name": "Trip Glass", "appearance": "light", "style": {
                    "background.appearance": "blurred", "background": "#d0e0f0cc",
                    "editor.background": "#fafafa", "border": "#00000033", "text": "#123456"
                } },
                { "name": "Trip", "style": {
                    "background": "#1e1e2e", "players": [{ "selection": "#0055aa40" }]
                } }
            ] }"##,
        )
        .unwrap();
        for flat in themes {
            let text = native_text(&flat);
            let read = native(&serde_json::from_str(&text).unwrap()).unwrap();
            assert_eq!(native_text(&read), text, "{}", flat.name);
            let [loaded] = parse_own(&text).unwrap()[..] else {
                panic!("one theme per native file");
            };
            assert_eq!(native_text(&loaded), native_text(&finished(read)));
            assert_eq!(loaded.bg != read.bg, flat.is_glass, "{}", flat.name);
        }
    }

    #[test]
    fn zeds_defaults_are_bundled() {
        let names: Vec<_> = BUNDLED
            .iter()
            .flat_map(|text| parse_zed(text).unwrap())
            .map(|theme| theme.name)
            .collect();
        assert_eq!(names.len(), 11);
        assert!(names.contains(&"Gruvbox Light Soft"));
        assert!(names.contains(&"One Dark"));
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
