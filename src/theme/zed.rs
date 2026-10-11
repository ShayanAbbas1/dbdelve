//! Zed theme files, read once at startup from [`store::theme_directory`].

use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::Value;

use super::color::{Rgba, Srgb, contrast_ratio};
use super::{PALETTE_EDITED_ALPHA, PALETTE_SELECTION_ALPHA, Theme};
use crate::store;

static USER_THEMES: OnceLock<Vec<Theme>> = OnceLock::new();

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

/// The themes [`load_user_themes`] read, or none if it has not run.
pub(super) fn user_themes() -> &'static [Theme] {
    USER_THEMES.get().map_or(&[], Vec::as_slice)
}

/// Read every `*.json` in the themes directory as a Zed theme family. A file
/// that does not read or parse is skipped, never fatal.
pub(crate) fn load_user_themes() {
    let mut paths: Vec<_> = store::theme_directory()
        .ok()
        .and_then(|directory| std::fs::read_dir(directory).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    let mut themes = Vec::new();
    for path in paths {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| parse(&text).map_err(|error| error.to_string()));
        match parsed {
            Ok(family) => themes.extend(family),
            Err(error) => eprintln!("skipping theme file {}: {error}", path.display()),
        }
    }
    let _ = USER_THEMES.set(themes);
}

/// A family's themes, minus any named like a built-in one: the built-in wins.
fn parse(text: &str) -> serde_json::Result<Vec<Theme>> {
    let built_in = Theme::built_in();
    Ok(serde_json::from_str::<Family>(text)?
        .themes
        .into_iter()
        .filter(|entry| {
            !built_in
                .iter()
                .any(|theme| theme.name.eq_ignore_ascii_case(&entry.name))
        })
        .map(convert)
        .collect())
}

fn convert(entry: Entry) -> Theme {
    let base = if entry.appearance == "light" {
        Theme::light()
    } else {
        Theme::dark()
    };
    let style = &entry.style;
    let rgba = |pointer: &str| color(style, pointer);
    let surface = rgba("/background").map_or(base.surface, |c| c.flatten(base.surface));
    let solid = |pointer: &str| rgba(pointer).map(|c| c.flatten(surface));
    let editor = solid("/editor.background");
    let text = solid("/text").unwrap_or(base.text);
    let cursor = solid("/players/0/cursor");
    let accent = solid("/text.accent").or(cursor).unwrap_or(base.accent);
    let syntax = |name: &str| solid(&format!("/syntax/{name}/color"));
    Theme {
        // ponytail: leaked, but themes load once per process so it is bounded.
        name: Box::leak(entry.name.into_boxed_str()),
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
    }
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
        let themes = parse(
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
}
