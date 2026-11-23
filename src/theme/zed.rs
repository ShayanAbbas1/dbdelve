//! Zed's theme format, and the themes Zed has on this machine for
//! [`super::user::Source::Zed`] to import: its own defaults, which are compiled
//! into Zed rather than installed, its themes directory and its extensions.
//! dbdelve's native format is read and written here too, since a file in
//! dbdelve's own directory may be either.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use super::color::{Oklch, Rgba, Srgb, contrast_ratio};
use super::{
    Appearance, PALETTE_ADDED_ALPHA, PALETTE_DELETED_ALPHA, PALETTE_EDITED_ALPHA,
    PALETTE_SELECTION_ALPHA, PALETTE_UPDATED_ALPHA, ROW_WASH_FROM_SELECTION, ROW_WASHES_APART,
    Theme,
};
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

/// Every Zed theme on this machine, unglassed. Native themes precede Flatpak
/// themes; within each installation, user themes precede extensions. Bundled
/// defaults come last, so a user's own copy of a default wins.
pub(super) fn discover() -> Vec<Theme> {
    let user = store::zed_theme_directories().unwrap_or_default();
    let extensions = store::zed_extension_directories().unwrap_or_default();
    let files = (0..user.len().max(extensions.len())).flat_map(|installation| {
        let user = user
            .get(installation)
            .into_iter()
            .flat_map(|directory| json_files(directory));
        let extensions = extensions
            .get(installation)
            .into_iter()
            .flat_map(|root| sorted_entries(root))
            .flat_map(|extension| json_files(&extension.join("themes")));
        user.chain(extensions)
    });
    let mut themes = read(files, parse_zed);
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
            Err(error) => skipped(&path, &error),
        }
    }
    themes
}

/// By file name at warn, since the path runs through the user's home; the
/// whole path and the error only when verbose, since a parse error quotes
/// the file's contents.
pub(super) fn skipped(path: &Path, error: &str) {
    let file = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    tracing::warn!(file, "theme file skipped");
    tracing::debug!(path = %path.display(), "theme file skipped: {error}");
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

/// The Zed defaults as [`convert`] reads them, for the tests that hold every
/// theme to one floor.
#[cfg(test)]
pub(super) fn bundled() -> Vec<Theme> {
    BUNDLED
        .iter()
        .flat_map(|text| parse_zed(text).unwrap())
        .collect()
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
            element_hover, element_active, border, border_strong, selection, edited, added, updated, deleted
        )
    };
}

/// `{ "name", "appearance", "glass", "colors": { <Theme field>: hex } }`,
/// unglassed.
fn native(value: &Value) -> Option<Theme> {
    let name = super::user::intern(value.get("name")?.as_str()?);
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
    let selection =
        rgba("/players/0/selection").map_or(base.selection, |c| see_through(c, surface));
    let edited = solid("/warning").map_or(base.edited, |c| c.alpha(PALETTE_EDITED_ALPHA));
    let [added, updated, deleted] = row_washes(
        &base,
        editor.unwrap_or(base.bg),
        selection,
        edited,
        [solid("/created"), solid("/modified"), solid("/deleted")],
    );
    Theme {
        name: super::user::intern(&entry.name),
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
        selection,
        cursor: cursor.unwrap_or(base.cursor),
        edited,
        added,
        updated,
        deleted,
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

/// The added, updated and deleted washes of an import, from the colours it
/// names for them (`None` where it names none, which takes `base`'s). Each is
/// the colour at its palette alpha, kept legible as a state: an updated wash
/// that lands within [`ROW_WASH_FROM_SELECTION`] of the selection, of the
/// selection over it, or of the edited cell wash over it is moved off in
/// lightness the way [`apart_from`] moves chrome (Zed's `modified` is its
/// `warning`, the edited amber, and is sometimes the selection's own hue); and
/// a wash within [`ROW_WASHES_APART`] of an earlier one, of the `base` wash a
/// later unnamed slot keeps, or that no move rescues, is `base`'s instead,
/// since two states drawn alike are worse than a borrowed colour. That default
/// is held to the same: an updated one on the selection is moved off it, and
/// a wash named before it that it would be drawn like goes back to its own
/// default. `bg` is what the washes are flattened over.
pub(super) fn row_washes(
    base: &Theme,
    bg: Srgb,
    selection: Rgba,
    edited: Rgba,
    named: [Option<Srgb>; 3],
) -> [Rgba; 3] {
    let own = [base.added, base.updated, base.deleted];
    let alphas = [
        PALETTE_ADDED_ALPHA,
        PALETTE_UPDATED_ALPHA,
        PALETTE_DELETED_ALPHA,
    ];
    let clear_of_selection = |color: Srgb| {
        let under = color.alpha(PALETTE_UPDATED_ALPHA).flatten(bg);
        [
            under.distance(selection.flatten(bg)),
            selection.flatten(under).distance(under),
            edited.flatten(under).distance(under),
        ]
        .into_iter()
        .all(|step| step >= ROW_WASH_FROM_SELECTION)
    };
    // The updated wash's colour, or the nearest lightness off it clear of
    // the selection.
    let rescued = |color: Srgb| {
        let own = Oklch::from_srgb(color);
        // Away from the selection first: toward the side with room.
        let away = if Oklch::from_srgb(selection.flatten(bg)).l > own.l {
            -0.02
        } else {
            0.02
        };
        // The colour as named first: a round trip through Oklch would
        // nudge one that needs no move.
        std::iter::once(color)
            .chain(
                (1..=20)
                    .flat_map(|n| [away * n as f32, -away * n as f32])
                    .map(|step| Oklch::new((own.l + step).clamp(0.0, 1.0), own.c, own.h).to_srgb()),
            )
            .find(|candidate| clear_of_selection(*candidate))
    };
    let apart = |a: Rgba, b: Rgba| a.flatten(bg).distance(b.flatten(bg)) >= ROW_WASHES_APART;
    let mut washes = own;
    let unnamed = named.map(|color| color.is_none());
    for (i, named) in named.into_iter().enumerate() {
        let Some(color) = named else { continue };
        let color = if i == 1 { rescued(color) } else { Some(color) };
        let stands_apart = |wash: &Rgba, washes: &[Rgba]| {
            let later_defaults = (i + 1..own.len()).filter(|&j| unnamed[j]).map(|j| own[j]);
            washes[..i]
                .iter()
                .copied()
                .chain(later_defaults)
                .all(|other| apart(*wash, other))
        };
        let picked = color
            .map(|color| color.alpha(alphas[i]))
            .filter(|wash| stands_apart(wash, &washes));
        washes[i] = match picked {
            Some(wash) => wash,
            None => {
                // The default stands in, moved off the selection where it
                // too sits on it, and a wash named before it that it would
                // be drawn like goes back to its own default.
                let fallback = (i == 1)
                    .then(|| {
                        rescued(Srgb::new(own[i].r, own[i].g, own[i].b))
                            .map(|color| color.alpha(alphas[i]))
                            .filter(|wash| stands_apart(wash, &washes))
                    })
                    .flatten()
                    .unwrap_or(own[i]);
                for j in 0..i {
                    if !apart(washes[j], fallback) {
                        washes[j] = own[j];
                    }
                }
                fallback
            }
        };
    }
    washes
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
    use crate::theme::{Appearance, ROW_WASH_FROM_SELECTION};

    #[cfg(target_os = "linux")]
    #[test]
    fn flatpak_themes_and_extensions_are_imported_beside_native_themes() {
        struct ConfigHome(Option<std::ffi::OsString>);

        impl Drop for ConfigHome {
            fn drop(&mut self) {
                // SAFETY: with_home holds the environment lock until this drops.
                unsafe {
                    match self.0.take() {
                        Some(config) => std::env::set_var("XDG_CONFIG_HOME", config),
                        None => std::env::remove_var("XDG_CONFIG_HOME"),
                    }
                }
            }
        }

        store::tests::with_home(|| {
            let home = store::home().unwrap();
            let _config_home = ConfigHome(std::env::var_os("XDG_CONFIG_HOME"));
            // SAFETY: with_home serializes the tests that touch these roots.
            unsafe { std::env::set_var("XDG_CONFIG_HOME", home.join("custom-config")) };
            let native = store::zed_theme_directories().unwrap().remove(0);
            let flatpak = home.join(".var/app/dev.zed.Zed");
            for (directory, text) in [
                (
                    native,
                    r##"{ "themes": [{ "name": "Shared Test", "style": { "text": "#123456" } }] }"##,
                ),
                (
                    store::zed_extension_directories()
                        .unwrap()
                        .remove(0)
                        .join("test-theme/themes"),
                    r##"{ "themes": [{ "name": "Shared Extension", "style": { "text": "#654321" } }] }"##,
                ),
                (
                    flatpak.join("config/zed/themes"),
                    r##"{ "themes": [
                        { "name": "Shared Test", "style": { "text": "#abcdef" } },
                        { "name": "Shared Extension", "style": { "text": "#abcdef" } },
                        { "name": "Flatpak Theme", "appearance": "light", "style": {} }
                    ] }"##,
                ),
                (
                    flatpak.join("data/zed/extensions/installed/test-theme/themes"),
                    r#"{ "themes": [{ "name": "Flatpak Extension", "style": {} }] }"#,
                ),
            ] {
                std::fs::create_dir_all(&directory).unwrap();
                std::fs::write(directory.join("theme.json"), text).unwrap();
            }
            let themes = discover();
            let named = |name| themes.iter().find(|theme| theme.name == name).unwrap();
            assert_eq!(named("Shared Test").text, Srgb::from_hex(0x123456));
            assert_eq!(named("Shared Extension").text, Srgb::from_hex(0x654321));
            assert_eq!(named("Flatpak Theme").appearance, Appearance::Light);
            assert_eq!(named("Flatpak Extension").appearance, Appearance::Dark);
            assert_eq!(
                themes
                    .iter()
                    .filter(|theme| theme.name == "Shared Test")
                    .count(),
                1
            );
        });
    }

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

        // The three row washes are a native file's too.
        let own = Theme {
            added: Srgb::from_hex(0x11aa44).alpha(0.2),
            updated: Srgb::from_hex(0x2255dd).alpha(0.3),
            deleted: Srgb::from_hex(0xcc2233).alpha(0.4),
            ..Theme::dark()
        };
        let text = native_text(&own);
        let read = native(&serde_json::from_str(&text).unwrap()).unwrap();
        for (put, got) in [
            (own.added, read.added),
            (own.updated, read.updated),
            (own.deleted, read.deleted),
        ] {
            assert_eq!(rgba_hex(got), rgba_hex(put));
        }
    }

    #[test]
    fn a_zed_theme_maps_created_modified_and_deleted_to_the_three_row_washes() {
        let [mapped, bare, crowded] = parse_zed(
            r##"{ "themes": [
                { "name": "Mapped", "style": {
                    "created": "#2ecc71", "modified": "#e056fd", "deleted": "#ff4d4d"
                } },
                { "name": "Bare", "style": {} },
                { "name": "Crowded", "style": {
                    "created": "#2ecc71", "modified": "#0055aa", "deleted": "#ff4d4d",
                    "players": [{ "selection": "#0055aa2e" }]
                } }
            ] }"##,
        )
        .unwrap()[..] else {
            panic!("three themes");
        };
        let dark = Theme::dark();
        assert_eq!(
            mapped.added,
            Srgb::from_hex(0x2ecc71).alpha(PALETTE_ADDED_ALPHA)
        );
        assert_eq!(
            mapped.updated,
            Srgb::from_hex(0xe056fd).alpha(PALETTE_UPDATED_ALPHA)
        );
        assert_eq!(
            mapped.deleted,
            Srgb::from_hex(0xff4d4d).alpha(PALETTE_DELETED_ALPHA)
        );
        assert_eq!(
            (bare.added, bare.updated, bare.deleted),
            (dark.added, dark.updated, dark.deleted)
        );
        // A modified colour that is the selection's own would be a changed row
        // read as a selected one.
        assert_ne!(
            crowded.updated,
            Srgb::from_hex(0x0055aa).alpha(PALETTE_UPDATED_ALPHA)
        );
        let updated = crowded.updated.flatten(crowded.bg);
        assert!(updated.distance(crowded.selection.flatten(crowded.bg)) >= ROW_WASH_FROM_SELECTION);
        assert!(crowded.selection.flatten(updated).distance(updated) >= ROW_WASH_FROM_SELECTION);
    }

    /// A named wash is kept apart from the default a later, unnamed slot
    /// keeps, not only from the washes named before it.
    #[test]
    fn a_partial_import_keeps_its_named_wash_apart_from_the_defaults() {
        let dark = Theme::dark();
        let updated = Srgb {
            r: dark.updated.r,
            g: dark.updated.g,
            b: dark.updated.b,
        };
        let [added, updated, _] = row_washes(
            &dark,
            dark.bg,
            dark.selection,
            dark.edited,
            [Some(updated), None, None],
        );
        assert!(
            added.flatten(dark.bg).distance(updated.flatten(dark.bg)) >= ROW_WASHES_APART,
            "added and updated rows drawn alike"
        );
        assert_eq!(added, dark.added);
    }

    /// A named wash refused for standing too close to another falls back to
    /// its default, which is held to the same rules: a wash named before it
    /// that it would be drawn like goes back to its own default, and an
    /// updated default that sits on the selection is moved off it.
    #[test]
    fn a_refused_wash_falls_back_to_a_default_that_stands_apart() {
        let dark = Theme::dark();
        let rgb = |wash: Rgba| Srgb::new(wash.r, wash.g, wash.b);
        let added_like_updated = rgb(dark.updated);
        let washes = row_washes(
            &dark,
            dark.bg,
            dark.selection,
            dark.edited,
            [Some(added_like_updated), Some(added_like_updated), None],
        );
        for (a, b) in [(0, 1), (0, 2), (1, 2)] {
            assert!(
                washes[a]
                    .flatten(dark.bg)
                    .distance(washes[b].flatten(dark.bg))
                    >= ROW_WASHES_APART,
                "washes {a} and {b} drawn alike"
            );
        }

        // The selection is the default updated wash's own colour.
        let selection = rgb(dark.updated).alpha(PALETTE_SELECTION_ALPHA);
        let [_, updated, _] = row_washes(
            &dark,
            dark.bg,
            selection,
            dark.edited,
            [None, Some(rgb(dark.added)), None],
        );
        let updated = updated.flatten(dark.bg);
        assert!(updated.distance(selection.flatten(dark.bg)) >= ROW_WASH_FROM_SELECTION);
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
