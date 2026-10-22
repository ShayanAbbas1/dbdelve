//! The settings panel: a card docked at the window's right edge, beside the
//! workspace rather than over it, so the change a setting makes can be watched
//! as it is made.
//!
//! Every row is listed once, as data, in [`setting_rows`]: the categories the
//! tabs show and the search box narrows are both read off that one list. A
//! control calls the same `Workspace` method a keystroke or the palette does,
//! which applies the change and writes it to disk at once; there is no Save.

use std::{collections::HashMap, ops::Range};

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, InteractiveElement, IntoElement,
    ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled, div,
    prelude::FluentBuilder, px, relative,
};
use gpui_component::{Disableable, Sizable, input::Input, tooltip::Tooltip};

use crate::{
    Workspace,
    actions::LimitTarget,
    fuzzy::{Fuzzy, Query},
    icons::icon,
    keybindings::{self, KeybindingSpec},
    palette::Mode as PaletteMode,
    scroller::{SmoothScrollable, smooth},
    theme::{FontSlot, OPACITY_MAX, OPACITY_MIN, OPACITY_STEP, Theme, fonts, layout, popup_theme},
    ui::{
        Control, Tone, button, chord_hint, highlighted, icon_button, keycap_text, keycaps,
        raised_card, segment,
    },
    views::row_limit_menu,
    workspace::{FONT_SIZE_STEP, font_size_range, opacity_percent},
};

/// The categories the panel's tabs show, in the order they are numbered.
/// Which one is in front is not remembered across launches: it is where the
/// panel was left a minute ago, not a preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    #[default]
    General,
    Appearance,
    Fonts,
    Keyboard,
}

impl SettingsTab {
    const ALL: [Self; 4] = [Self::General, Self::Appearance, Self::Fonts, Self::Keyboard];

    fn name(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Fonts => "Fonts",
            Self::Keyboard => "Keyboard",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::General => icon::SLIDERS,
            Self::Appearance => icon::THEME,
            Self::Fonts => icon::FONT,
            Self::Keyboard => icon::KEYBOARD,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::General => "Results, sorting and updates",
            Self::Appearance => "Theme, glass and titlebar",
            Self::Fonts => "Faces and sizes",
            Self::Keyboard => "Shortcuts",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::General => {
                "How much a query brings back, how a header click sorts, and \
                 whether DBDelve looks for a new release."
            }
            Self::Appearance => {
                "What DBDelve looks like, and how much of the desktop shows \
                 through it."
            }
            Self::Fonts => "The chrome, the editor and the grid each have their own face and size.",
            Self::Keyboard => {
                "Every action you can bind. A change takes effect the next \
                 time DBDelve starts."
            }
        }
    }

    /// The eyebrow over the category's heading: `01 · GENERAL`.
    fn eyebrow(self) -> String {
        let number = Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0) + 1;
        format!("{number:02} · {}", self.name().to_uppercase())
    }
}

/// One setting, by what it sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingId {
    RowLimit,
    DefaultSorting,
    CheckForUpdates,
    Theme,
    Opacity,
    ColorTitlebar,
    Font(FontSlot),
    /// A [`KeybindingSpec::id`].
    Keybinding(&'static str),
}

/// A row the search found, and the characters of its title it matched.
pub(crate) struct SettingMatch {
    id: SettingId,
    title: Vec<Range<usize>>,
}

struct SettingRow {
    id: SettingId,
    tab: SettingsTab,
    group: &'static str,
    title: &'static str,
    description: Option<&'static str>,
    /// Words someone looking for this row might type that its title does not
    /// hold.
    keywords: &'static [&'static str],
}

const FONT_KEYWORDS: &[&str] = &["font", "family", "face", "typeface", "size"];

/// Every row the panel can show, in the order it shows them: by category,
/// then by group, then as listed.
fn setting_rows() -> Vec<SettingRow> {
    let row = |id, tab, group, title, description, keywords| SettingRow {
        id,
        tab,
        group,
        title,
        description: Some(description),
        keywords,
    };
    let mut rows = vec![
        row(
            SettingId::RowLimit,
            SettingsTab::General,
            "Results",
            "Row limit",
            "The most rows a new tab keeps from one statement. Tabs already \
             open keep theirs.",
            &["rows", "limit", "default", "page"],
        ),
        row(
            SettingId::DefaultSorting,
            SettingsTab::General,
            "Results",
            "Default sorting",
            "How a new tab sorts on a header click. Server runs the query \
             again, sorted by the database; Client reorders the rows already \
             loaded, so a table sorts only the page on screen. Tabs already \
             open keep theirs; each can be switched from its status bar.",
            &["sort", "order", "server", "client"],
        ),
        row(
            SettingId::CheckForUpdates,
            SettingsTab::General,
            "Updates",
            "Check for updates",
            "One request to GitHub at launch to see whether a newer release \
             exists. Nothing is sent about you or your databases.",
            &["update", "release", "version", "github"],
        ),
        row(
            SettingId::Theme,
            SettingsTab::Appearance,
            "Theme",
            "Theme",
            "Moving through the list previews each theme.",
            &["color", "colour", "dark", "light"],
        ),
        row(
            SettingId::Opacity,
            SettingsTab::Appearance,
            "Theme",
            "Opacity",
            "Everything else is relative to this. The chrome, the editor and \
             the results grid are tints over the frost set here, so they move \
             with it rather than being set apiece.",
            &["transparency", "translucent", "glass", "frost", "blur"],
        ),
        row(
            SettingId::ColorTitlebar,
            SettingsTab::Appearance,
            "Window",
            "Color the titlebar",
            "Paint the titlebar in the connection's color.",
            &["colour", "connection", "band", "tint"],
        ),
        row(
            SettingId::Font(FontSlot::Chrome),
            SettingsTab::Fonts,
            "Faces",
            "Chrome",
            "Sidebar, tabs, titlebar and status bar.",
            FONT_KEYWORDS,
        ),
        row(
            SettingId::Font(FontSlot::Editor),
            SettingsTab::Fonts,
            "Faces",
            "Editor",
            "The query buffer and the other views that show code.",
            FONT_KEYWORDS,
        ),
        row(
            SettingId::Font(FontSlot::Grid),
            SettingsTab::Fonts,
            "Faces",
            "Grid",
            "Result cells, their headers and the row inspector.",
            FONT_KEYWORDS,
        ),
    ];

    // Grouped by where a binding fires, each group where its first binding
    // stands in the registry, and the registry's order inside it.
    let mut groups: Vec<&'static str> = Vec::new();
    for spec in keybindings::REGISTRY {
        let group = keyboard_group(spec.context);
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    for group in groups {
        rows.extend(
            keybindings::REGISTRY
                .iter()
                .filter(|spec| keyboard_group(spec.context) == group)
                .map(|spec| SettingRow {
                    id: SettingId::Keybinding(spec.id),
                    tab: SettingsTab::Keyboard,
                    group,
                    title: spec.label,
                    description: None,
                    keywords: &[],
                }),
        );
    }
    rows
}

/// A binding's context as the place it works, in words.
fn keyboard_group(context: Option<&'static str>) -> &'static str {
    match context {
        None => "Everywhere",
        Some("Palette > Input") => "Palette",
        Some("DataTable") => "Results grid",
        Some("Editor > Input") => "Editor",
        Some(other) => other,
    }
}

/// How a chord's `secondary-` is spelled for someone typing it: the glyphs
/// `keycap_text` draws cannot be typed, so `cmd-shift-p` and `escape` have to
/// find their rows too.
const SECONDARY_KEY: &str = if cfg!(target_os = "macos") {
    "cmd"
} else {
    "ctrl"
};

/// The rows `query` finds, in the panel's order, across every category.
///
/// A row is matched on its title, its group, its category and its keywords,
/// and a binding on its chord too, so `⌘P` or `cmd-p` finds whatever holds
/// it. Never on
/// the description: a paragraph holds nearly any run of letters in order.
///
/// Each word typed has to match within one of those, not across them: run
/// together, "sort" is found in "Close Tab Everywhere Keyboard" (an s, then o,
/// r and t further on), and nearly every row matches any short word.
pub(crate) fn filter_settings(
    query: &str,
    overrides: &HashMap<String, String>,
) -> Vec<SettingMatch> {
    let words: Vec<(Query, bool)> = query
        .split_whitespace()
        .map(|word| (Query::new(word), word.starts_with('!')))
        .collect();
    let mut fuzzy = Fuzzy::new();
    let mut found = Vec::new();
    for row in setting_rows() {
        let mut fields: Vec<String> = [row.title, row.group, row.tab.name()]
            .into_iter()
            .chain(row.keywords.iter().copied())
            .map(str::to_string)
            .collect();
        if let SettingId::Keybinding(id) = row.id
            && let Some(spec) = keybinding(id)
        {
            for chord in keybindings::chords_for(spec, overrides) {
                fields.push(keycap_text(chord));
                fields.push(chord.replace("secondary", SECONDARY_KEY));
            }
        }
        // fzf's `!word` excludes: it holds only when no field has the word.
        let matches = words.iter().all(|(word, excludes)| {
            let mut matched = fields.iter().map(|field| fuzzy.matches(word, field));
            if *excludes {
                matched.all(|kept| kept)
            } else {
                matched.any(|found| found)
            }
        });
        if !matches {
            continue;
        }
        let title = words
            .iter()
            .filter(|(_, excludes)| !excludes)
            .flat_map(|(word, _)| fuzzy.highlights(word, row.title))
            .collect();
        found.push(SettingMatch {
            id: row.id,
            title: merged(title),
        });
    }
    found
}

/// Ranges sorted and joined where they overlap or touch, which is how
/// `ui::highlighted` takes them: each word typed highlights on its own, and
/// two can land on the same letters.
fn merged(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

fn keybinding(id: &str) -> Option<&'static KeybindingSpec> {
    keybindings::REGISTRY.iter().find(|spec| spec.id == id)
}

/// A row on screen, with the characters of its title the search matched.
struct Shown<'a> {
    row: &'a SettingRow,
    matched: &'a [Range<usize>],
}

pub(crate) fn render_settings_panel(
    workspace: &Workspace,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = popup_theme(cx);
    let searching = workspace.settings_matches.is_some();
    let rows = setting_rows();
    let shown: Vec<Shown> = match &workspace.settings_matches {
        Some(found) => rows
            .iter()
            .filter_map(|row| {
                let found = found.iter().find(|found| found.id == row.id)?;
                Some(Shown {
                    row,
                    matched: &found.title,
                })
            })
            .collect(),
        None => rows
            .iter()
            .filter(|row| row.tab == workspace.settings_tab)
            .map(|row| Shown { row, matched: &[] })
            .collect(),
    };

    let body = if shown.is_empty() {
        render_no_match(t).into_any_element()
    } else {
        let sections: Vec<AnyElement> = shown
            .chunk_by(|a, b| a.row.tab == b.row.tab)
            .map(|section| render_section(workspace, section, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_LG * 2.))
            .children(sections)
            .into_any_element()
    };

    // Inset from the window's edges rather than flush with them, so it reads
    // as a sheet set down beside the workspace rather than a column of it.
    div()
        .flex_shrink_0()
        .w(px(layout::SETTINGS_PANEL_WIDTH + layout::SPACE_SM))
        .max_w(relative(layout::SETTINGS_PANEL_MAX_SHARE))
        .h_full()
        .pt(px(layout::SPACE_SM))
        .pr(px(layout::SPACE_SM))
        .pb(px(layout::SPACE_SM))
        .child(
            div()
                .id("settings-panel")
                .track_focus(&workspace.settings_focus)
                .size_full()
                .flex()
                .flex_col()
                .overflow_hidden()
                .text_size(px(layout::chrome(layout::TEXT_MD)))
                .text_color(t.text)
                .border_1()
                .border_color(t.border_strong)
                .rounded(px(layout::RADIUS_CARD))
                .map(|card| raised_card(card, t))
                .child(render_header(workspace, t, cx))
                .child(
                    div()
                        .flex_shrink_0()
                        .px(px(layout::SPACE_LG))
                        .pb(px(layout::SPACE_MD))
                        .child(
                            Input::new(&workspace.settings_search).prefix(
                                icon(icon::SEARCH)
                                    .size(px(layout::chrome(layout::ICON_SIZE)))
                                    .text_color(t.text_muted),
                            ),
                        ),
                )
                .child(render_tabs(workspace, searching, t, cx))
                .child(
                    div()
                        .id("settings-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .smooth_scroll(&smooth("settings-body", cx))
                        .px(px(layout::SPACE_LG))
                        // On the one child rather than here, so scrolling it
                        // to the top (`scroll_to`) lands at offset zero.
                        .child(
                            div()
                                .pt(px(layout::SPACE_LG))
                                .pb(px(layout::SPACE_LG * 2.))
                                .child(body),
                        ),
                ),
        )
        .into_any_element()
}

fn render_header(workspace: &Workspace, t: Theme, cx: &mut Context<Workspace>) -> impl IntoElement {
    let tile = layout::chrome(layout::PALETTE_TILE_SIZE);
    let close = match chord_hint("show_editor", &workspace.settings.custom_keybindings) {
        hint if hint.is_empty() => "Close".to_string(),
        hint => format!("Close ({hint})"),
    };
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_MD))
        .min_h(px(layout::chrome(layout::SETTINGS_HEADER_HEIGHT)))
        .px(px(layout::SPACE_LG))
        .child(
            div()
                .flex_shrink_0()
                .size(px(tile))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(layout::RADIUS_CONTROL))
                .bg(t.accent)
                .child(
                    icon(icon::SLIDERS)
                        .size(px(layout::chrome(layout::ICON_SIZE)))
                        .text_color(t.on_accent),
                ),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(layout::chrome(layout::TEXT_LG)))
                .font_weight(FontWeight::MEDIUM)
                .child("Settings"),
        )
        .child(
            icon_button(
                "settings-close",
                icon::CLOSE,
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .tooltip(close)
            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                workspace.close_settings(window, cx);
            })),
        )
}

/// The categories as icons, the one in front underlined in the accent. None
/// is while searching: the results come from all of them.
fn render_tabs(
    workspace: &Workspace,
    searching: bool,
    t: Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let size = layout::chrome(layout::SETTINGS_TAB_SIZE);
    let underline = layout::SETTINGS_TAB_UNDERLINE_WIDTH;
    let tabs = SettingsTab::ALL.into_iter().map(|tab| {
        let active = !searching && tab == workspace.settings_tab;
        div()
            .id(("settings-tab", tab as usize))
            .relative()
            .size(px(size))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(layout::RADIUS_CONTROL))
            .cursor_pointer()
            .when(!active, |tab| tab.hover(|style| style.bg(t.element_hover)))
            .child(
                icon(tab.icon())
                    .size(px(layout::chrome(layout::ICON_SIZE)))
                    .text_color(if active { t.accent } else { t.text_muted }),
            )
            .when(active, |tab| {
                tab.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left(px((size - underline) / 2.))
                        .w(px(underline))
                        .h(px(layout::SETTINGS_TAB_UNDERLINE_HEIGHT))
                        .rounded_full()
                        .bg(t.accent),
                )
            })
            .tooltip(move |window, cx| Tooltip::new(tab.name()).build(window, cx))
            .on_click(cx.listener(move |workspace, _: &ClickEvent, window, cx| {
                workspace.set_settings_tab(tab, window, cx);
            }))
    });
    div()
        .flex_shrink_0()
        .flex()
        .flex_wrap()
        .gap(px(layout::SPACE_XS))
        .px(px(layout::SPACE_LG))
        .border_b_1()
        .border_color(t.border)
        .children(tabs)
}

fn render_no_match(t: Theme) -> impl IntoElement {
    div()
        .py(px(layout::SPACE_LG * 2.))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .child("No settings match")
        .child(
            div()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_muted)
                .child("Try another word."),
        )
}

/// One category's heading and its groups of rows, every row of one category.
fn render_section(
    workspace: &Workspace,
    rows: &[Shown],
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = popup_theme(cx);
    let tab = rows[0].row.tab;
    let mono = fonts(cx).family(FontSlot::Editor).clone();
    let groups: Vec<AnyElement> = rows
        .chunk_by(|a, b| a.row.group == b.row.group)
        .map(|group| render_group(workspace, group, cx))
        .collect();
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .font_family(mono)
                .text_size(px(layout::chrome(layout::TEXT_XS)))
                .text_color(t.accent)
                .child(tab.eyebrow()),
        )
        .child(
            div()
                .mt(px(layout::SPACE_XS))
                .text_size(px(layout::chrome(layout::TEXT_LG)))
                .font_weight(FontWeight::MEDIUM)
                .child(tab.title()),
        )
        .child(
            div()
                .mt(px(layout::SPACE_XS))
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_muted)
                .child(tab.description()),
        )
        .child(
            div()
                .mt(px(layout::SPACE_LG))
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_LG))
                .children(groups),
        )
        .into_any_element()
}

/// A group's label over a card of its rows, a hairline between each two.
fn render_group(workspace: &Workspace, rows: &[Shown], cx: &mut Context<Workspace>) -> AnyElement {
    let t = popup_theme(cx);
    let mut card = div()
        .flex()
        .flex_col()
        .rounded(px(layout::RADIUS_SETTINGS_CARD))
        .border_1()
        .border_color(t.border)
        .bg(t.element_hover);
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            card = card.child(div().mx(px(layout::SPACE_LG)).h(px(1.)).bg(t.border));
        }
        card = card.child(render_row(workspace, row.row, row.matched, cx));
    }
    div()
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_SM))
        .child(
            div()
                .text_size(px(layout::chrome(layout::TEXT_XS)))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_muted)
                .child(rows[0].row.group),
        )
        .child(card)
        .into_any_element()
}

/// A row's title and description, its control beside them or, when the two
/// do not fit across the panel, under them.
fn render_row(
    workspace: &Workspace,
    row: &SettingRow,
    matched: &[Range<usize>],
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = popup_theme(cx);
    let glass_only = row.id == SettingId::Opacity && !t.is_glass;
    let description = row.description.map(|description| {
        if glass_only {
            SharedString::from(format!("{description} Glass themes only."))
        } else {
            SharedString::from(description)
        }
    });
    let height = if description.is_some() {
        layout::SETTINGS_ROW_HEIGHT
    } else {
        layout::SETTINGS_ROW_COMPACT_HEIGHT
    };
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(layout::SPACE_LG))
        .gap_y(px(layout::SPACE_SM))
        .min_h(px(layout::chrome(height)))
        .px(px(layout::SPACE_LG))
        .py(px(layout::SPACE_MD))
        .child(
            div()
                .flex_1()
                .min_w(px(layout::SETTINGS_TEXT_MIN_WIDTH))
                .flex()
                .flex_col()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(highlighted(row.title, matched, t)),
                )
                .children(description.map(|description| {
                    div()
                        .mt(px(layout::SETTINGS_DESCRIPTION_GAP))
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(t.text_muted)
                        .child(description)
                })),
        )
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .child(render_control(workspace, row.id, t, cx)),
        )
        .into_any_element()
}

fn render_control(
    workspace: &Workspace,
    id: SettingId,
    t: Theme,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let settings = &workspace.settings;
    match id {
        SettingId::RowLimit => row_limit_menu(
            LimitTarget::Default,
            settings.row_limit,
            workspace
                .custom_limit
                .as_ref()
                .filter(|(target, _)| *target == LimitTarget::Default)
                .map(|(_, input)| input),
            t,
        ),
        SettingId::DefaultSorting => {
            let client_sort = settings.client_sort;
            div()
                .flex()
                .gap(px(layout::SPACE_XS))
                .children(
                    [(false, "Server"), (true, "Client")].map(|(client, label)| {
                        segment(
                            ("default-sorting", client as usize),
                            label,
                            client == client_sort,
                            t,
                        )
                        .on_click(cx.listener(
                            move |workspace, _: &ClickEvent, _, cx| {
                                workspace.set_client_sort(client, cx);
                            },
                        ))
                    }),
                )
                .into_any_element()
        }
        SettingId::CheckForUpdates => switch("check-for-updates", settings.check_for_updates, t)
            .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
                let check = !workspace.settings.check_for_updates;
                workspace.set_check_for_updates(check, cx);
            }))
            .into_any_element(),
        SettingId::ColorTitlebar => switch("color-titlebar", settings.color_titlebar, t)
            .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
                let color = !workspace.settings.color_titlebar;
                workspace.set_color_titlebar(color, cx);
            }))
            .into_any_element(),
        // The palette lists the themes, and moving through it previews each
        // one on the window this panel sits in.
        SettingId::Theme => picker("theme", t.name, t)
            .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                workspace.open_palette(PaletteMode::Theme, window, cx);
            }))
            .into_any_element(),
        SettingId::Opacity => render_opacity(workspace, t, cx),
        SettingId::Font(slot) => render_font(workspace, slot, t, cx),
        SettingId::Keybinding(id) => match keybinding(id) {
            Some(spec) => render_keybinding(workspace, spec, t, cx),
            None => div().into_any_element(),
        },
    }
}

/// A choice made from a list somewhere else -- the palette, a menu -- dressed
/// as the row limit's menu button, so every picker in the panel reads alike.
/// The one control in a row that gives way when the row is too narrow, its
/// label cut short: a long family name at a large chrome scale.
fn picker(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    t: Theme,
) -> gpui_component::button::Button {
    button(id, label, Tone::Quiet, Control::Compact, t)
        .min_w_0()
        .flex_initial()
        .child(
            icon(icon::CHEVRON_DOWN)
                .flex_none()
                .size(px(layout::chrome(layout::ICON_SIZE)))
                .text_color(t.text_faint),
        )
}

/// An on/off setting. Drawn here rather than taken from gpui-component, whose
/// switch springs its thumb across: nothing in DBDelve moves on a click (see
/// AGENTS.md, "Animation").
fn switch(id: &'static str, on: bool, t: Theme) -> Stateful<Div> {
    let width = layout::chrome(layout::SWITCH_WIDTH);
    let height = layout::chrome(layout::SWITCH_HEIGHT);
    let thumb = layout::chrome(layout::SWITCH_THUMB_SIZE);
    let inset = (height - thumb) / 2.;
    div()
        .id(id)
        .flex_shrink_0()
        .relative()
        .w(px(width))
        .h(px(height))
        .rounded_full()
        .cursor_pointer()
        .map(|track| {
            if on {
                track.bg(t.accent)
            } else {
                track.bg(t.border_strong)
            }
        })
        .child(
            div()
                .absolute()
                .top(px(inset))
                .left(px(if on { width - thumb - inset } else { inset }))
                .size(px(thumb))
                .rounded_full()
                .map(|knob| {
                    if on {
                        knob.bg(t.on_accent)
                    } else {
                        knob.bg(t.text_muted)
                    }
                }),
        )
}

fn render_opacity(workspace: &Workspace, t: Theme, cx: &mut Context<Workspace>) -> AnyElement {
    // Whole percents, as the field reads: a step can land on 0.77000004, and
    // comparing that f32 to a bound would leave a button live that does nothing.
    let opacity = opacity_percent(t.opacity);
    let default_opacity = t.default_opacity();
    // Disabled wholesale on an opaque theme rather than hidden: the setting is
    // still remembered, it is just that a theme which paints its own chrome
    // has no desktop behind it for this to let through.
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .child(
            button("opacity-down", "−", Tone::Quiet, Control::Compact, t)
                .disabled(!t.is_glass || opacity <= opacity_percent(OPACITY_MIN))
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.step_opacity(-OPACITY_STEP, window, cx);
                })),
        )
        .child(
            // The sign sits beside the field rather than in it. `suffix` lays
            // out inside the width declared here and pads again on its own, so
            // a small input carrying one leaves under twenty points for the
            // text: at 56 the committed 95 rendered as a clipped 9 and a 5.
            // This is the readout's own width instead -- three typed digits
            // and the caret, since 100 is reachable on the way to a value that
            // clamps, inside the small size's 8-point padding.
            Input::new(&workspace.opacity_input)
                .small()
                .w(px(52.))
                .disabled(!t.is_glass),
        )
        .child(
            div()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_faint)
                .child("%"),
        )
        .child(
            button("opacity-up", "+", Tone::Quiet, Control::Compact, t)
                .disabled(!t.is_glass || opacity >= opacity_percent(OPACITY_MAX))
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.step_opacity(OPACITY_STEP, window, cx);
                })),
        )
        .child(
            button("opacity-reset", "Reset", Tone::Quiet, Control::Compact, t)
                .disabled(!t.is_glass)
                .on_click(cx.listener(move |workspace, _: &ClickEvent, window, cx| {
                    workspace.set_opacity(default_opacity, window, cx);
                })),
        )
        .into_any_element()
}

/// The family, picked from the palette, which lists every family the text
/// system resolved and marks the one in use; then the size.
fn render_font(
    workspace: &Workspace,
    slot: FontSlot,
    t: Theme,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let index = slot as usize;
    let family = fonts(cx).family(slot).clone();
    let size = workspace.settings.font_size(slot);
    let (min, default, max) = font_size_range(slot);
    // Disabled at the ends rather than clamped again here: `step_font_size`
    // already refuses to go past them, and a button that looks live and does
    // nothing is worse than one that says it cannot.
    let stepper = div()
        .flex()
        .items_center()
        .child(
            button(
                ("font-smaller", index),
                "−",
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .disabled(size <= min)
            .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                workspace.step_font_size(slot, -FONT_SIZE_STEP, cx);
            })),
        )
        // The readout is the reset: clicking a size that is not the default
        // puts it back.
        .child(
            button(
                ("font-size", index),
                format!("{size}"),
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .min_w(px(layout::chrome(layout::CONTROL_HEIGHT)))
            .disabled(size == default)
            .tooltip(format!("Reset to {default}"))
            .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                workspace.set_font_size(slot, default, cx);
            })),
        )
        .child(
            button(
                ("font-larger", index),
                "+",
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .disabled(size >= max)
            .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                workspace.step_font_size(slot, FONT_SIZE_STEP, cx);
            })),
        );
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .child(picker(("font", index), family, t).on_click(cx.listener(
            move |workspace, _: &ClickEvent, window, cx| {
                workspace.open_palette(PaletteMode::Font(slot), window, cx);
            },
        )))
        .child(stepper)
        .into_any_element()
}

/// What is bound to the action today and the buttons that change it, or --
/// while it is the row [`Workspace::rebinding`] names -- the prompt for the
/// next keystroke. The keystroke itself is taken by the interceptor
/// `Workspace::new` installs; nothing here listens for keys.
fn render_keybinding(
    workspace: &Workspace,
    spec: &'static KeybindingSpec,
    t: Theme,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let id = spec.id;
    if workspace.rebinding == Some(id) {
        return div()
            .text_size(px(layout::chrome(layout::TEXT_SM)))
            .text_color(t.accent)
            .child("Press any key… (Esc to cancel)")
            .into_any_element();
    }
    let overrides = &workspace.settings.custom_keybindings;
    let chords = keybindings::chords_for(spec, overrides);
    let bound = if chords.is_empty() {
        div()
            .text_size(px(layout::chrome(layout::TEXT_SM)))
            .text_color(t.text_faint)
            .child("Unbound")
    } else {
        div()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_SM))
            .children(chords.into_iter().map(|chord| keycaps(chord, t)))
    };
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .child(bound)
        .child(
            button(
                SharedString::from(format!("keybind-edit-{id}")),
                "Edit",
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .on_click(cx.listener(move |workspace, _: &ClickEvent, window, cx| {
                workspace.start_rebind(id, window, cx);
            })),
        )
        .children(overrides.contains_key(id).then(|| {
            button(
                SharedString::from(format!("keybind-reset-{id}")),
                "Reset",
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                workspace.reset_keybinding(id, cx);
            }))
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(found: Vec<SettingMatch>) -> Vec<SettingId> {
        found.into_iter().map(|found| found.id).collect()
    }

    fn found(query: &str) -> Vec<SettingId> {
        ids(filter_settings(query, &HashMap::new()))
    }

    #[test]
    fn font_finds_the_three_font_rows() {
        let found = found("font");
        for slot in [FontSlot::Chrome, FontSlot::Editor, FontSlot::Grid] {
            assert!(found.contains(&SettingId::Font(slot)), "{found:?}");
        }
    }

    #[test]
    fn a_setting_is_found_by_the_word_the_user_would_type() {
        assert!(found("sort").contains(&SettingId::DefaultSorting));
        assert!(found("titlebar").contains(&SettingId::ColorTitlebar));
        assert!(found("transparency").contains(&SettingId::Opacity));
        assert!(found("updates").contains(&SettingId::CheckForUpdates));
    }

    #[test]
    fn a_binding_is_found_by_its_label_and_by_its_chord() {
        assert!(found("palette next row").contains(&SettingId::Keybinding("palette_next")));
        let chord = keycap_text("secondary-shift-p");
        assert!(found(&chord).contains(&SettingId::Keybinding("command_palette")));
    }

    #[test]
    fn a_binding_is_found_by_its_keys_spelled_out() {
        assert!(found("escape").contains(&SettingId::Keybinding("show_editor")));
        let spelled = format!("{SECONDARY_KEY}-shift-p");
        assert!(found(&spelled).contains(&SettingId::Keybinding("command_palette")));
    }

    #[test]
    fn a_rebound_chord_is_searched_as_rebound() {
        let overrides = HashMap::from([("run_query".to_string(), "secondary-shift-j".to_string())]);
        let chord = keycap_text("secondary-shift-j");
        assert!(
            ids(filter_settings(&chord, &overrides)).contains(&SettingId::Keybinding("run_query"))
        );
        assert!(!found(&chord).contains(&SettingId::Keybinding("run_query")));
    }

    #[test]
    fn a_rebound_action_gives_up_its_old_chord() {
        let shipped = keycap_text(keybinding("run_query").unwrap().defaults[0]);
        let run_query = SettingId::Keybinding("run_query");
        assert!(found(&shipped).contains(&run_query));
        let overrides = HashMap::from([("run_query".to_string(), "secondary-shift-j".to_string())]);
        assert!(!ids(filter_settings(&shipped, &overrides)).contains(&run_query));
    }

    #[test]
    fn a_word_matches_within_one_field_not_across_them() {
        assert_eq!(found("sort"), [SettingId::DefaultSorting]);
        assert_eq!(found("!keyboard").len(), 9);
    }

    #[test]
    fn the_title_highlights_every_word_that_landed_in_it() {
        let found = filter_settings("color bar", &HashMap::new());
        let titlebar = found
            .iter()
            .find(|found| found.id == SettingId::ColorTitlebar)
            .unwrap();
        assert_eq!(titlebar.title, [0..5, 15..18]);
        assert_eq!(merged(vec![4..6, 0..2, 1..3, 6..7]), [0..3, 4..7]);
    }

    #[test]
    fn results_span_categories() {
        let found = found("editor");
        assert!(found.contains(&SettingId::Font(FontSlot::Editor)));
        assert!(found.contains(&SettingId::Keybinding("toggle_comment")));
    }

    #[test]
    fn nothing_typed_lists_every_row_in_category_order() {
        let found = found("  ");
        let rows = setting_rows();
        assert_eq!(found, rows.iter().map(|row| row.id).collect::<Vec<_>>());
        let tabs: Vec<SettingsTab> = rows.iter().map(|row| row.tab).collect();
        assert!(tabs.is_sorted_by_key(|tab| *tab as usize));
    }

    #[test]
    fn nonsense_finds_nothing() {
        assert!(found("qzxv").is_empty());
    }

    #[test]
    fn every_binding_has_one_row_and_its_group_is_together() {
        let rows = setting_rows();
        let keyboard: Vec<&SettingRow> = rows
            .iter()
            .filter(|row| row.tab == SettingsTab::Keyboard)
            .collect();
        assert_eq!(keyboard.len(), keybindings::REGISTRY.len());
        let groups: Vec<&str> = keyboard
            .chunk_by(|a, b| a.group == b.group)
            .map(|group| group[0].group)
            .collect();
        assert_eq!(groups, ["Everywhere", "Palette", "Results grid", "Editor"]);
    }

    #[test]
    fn the_eyebrow_numbers_the_category() {
        assert_eq!(SettingsTab::General.eyebrow(), "01 · GENERAL");
        assert_eq!(SettingsTab::Keyboard.eyebrow(), "04 · KEYBOARD");
    }
}
