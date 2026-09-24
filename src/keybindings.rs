//! The keybinding registry: every action a user can put a chord on, its
//! shipped default (if any), and the scope it fires in.
//!
//! `profiles.toml` stores only the overrides a user actually set, keyed by
//! [`KeybindingSpec::id`] -- a default that changes in a later build still
//! reaches anyone who never touched that row.
//!
//! Defaults are spelled `secondary-`, which GPUI reads as Cmd on macOS and
//! Ctrl everywhere else. `cmd-` would parse too, but it sets the platform
//! modifier, which on Linux is Super -- a key the window manager takes before
//! the app ever sees it. An override already on disk in the older `cmd-`
//! spelling still parses and still wins; it just carries that macOS meaning
//! with it.
//!
//! Rebinding takes effect on the next launch. GPUI's keymap can only be
//! replaced wholesale (`clear_key_bindings` then `bind_keys`), and clearing
//! it at runtime would also wipe the bindings `gpui_component::init` wires up
//! for its own widgets -- a risk this app has no reason to take for a
//! settings change nobody is in a hurry over.

use std::collections::HashMap;

use gpui::{KeyBinding, Keystroke};

use crate::{
    actions::{
        AcceptCompletion, AddFilter, ApplyEdits, CancelQuery, ClearFilter, CloseTab,
        CommandPalette, CopyCell, CycleTheme, DeleteRow, DiscardEdits, EditCell, ExplainQuery,
        FollowForeignKey, FormatQuery, FuzzyOpen, NewConnection, NewQuery, NewRow, NextPage,
        NextProfile, NextTab, OpenSettings, PaletteNext, PalettePrevious, PreviousPage,
        PreviousProfile, PreviousTab, Quit, RefreshConnection, RefreshRelation, ResetEditorZoom,
        RunQuery, SaveQuery, SetDefault, SetEmpty, SetNull, ShowEditor, ToggleNextJoin,
        ToggleRowPanel, ToggleSidebar, ZoomEditorIn, ZoomEditorOut,
    },
    db::ExplainMode,
};

/// One entry in the registry.
pub(crate) struct KeybindingSpec {
    /// Stable storage key. Never shown; changing it would orphan anyone's
    /// override, so treat it as append-only.
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    /// `None` means global. Two bindings in different, specific contexts
    /// never conflict -- see [`contexts_overlap`].
    pub(crate) context: Option<&'static str>,
    /// More than one only for `zoom_editor_in`, which ships both the shifted
    /// and unshifted key so `+` reads the same on every layout. An override
    /// replaces the whole list with a single chord.
    pub(crate) defaults: &'static [&'static str],
}

/// Whether `key` is a modifier held on its own. GPUI reports one as a
/// keystroke of its own when it is pressed and released alone, and "⌘" is not
/// a chord anyone means to bind.
pub(crate) fn is_modifier(key: &str) -> bool {
    matches!(key, "shift" | "control" | "alt" | "platform" | "function")
}

/// Whether GPUI can read `chord` back. `KeyBinding::new` panics on one it
/// cannot, and the overrides come off disk -- a hand-edited `profiles.toml`
/// gets its default back rather than a launch that dies on the keymap.
pub(crate) fn is_parseable(chord: &str) -> bool {
    !chord.is_empty()
        && chord
            .split_whitespace()
            .all(|stroke| Keystroke::parse(stroke).is_ok())
}

// The action is matched as a token sequence rather than an `ident` so that one
// carrying a field can be bound too: `expr` cannot be followed by the closing
// paren of the row, and every action is spelled here as a literal value.
macro_rules! registry {
    ($(($id:literal, $label:literal, $context:expr, [$($default:literal),*], $($action:tt)+)),* $(,)?) => {
        pub(crate) const REGISTRY: &[KeybindingSpec] = &[
            $(
                KeybindingSpec {
                    id: $id,
                    label: $label,
                    context: $context,
                    defaults: &[$($default),*],
                }
            ),*
        ];

        /// Every `KeyBinding` GPUI should register at startup: a user's
        /// override where one was set, the shipped defaults otherwise.
        pub(crate) fn build_bindings(overrides: &HashMap<String, String>) -> Vec<KeyBinding> {
            let mut bindings = Vec::new();
            $(
                match overrides.get($id) {
                    Some(chord) if is_parseable(chord) => {
                        bindings.push(KeyBinding::new(chord, $($action)+, $context))
                    }
                    _ => {
                        for default in [$($default),*] {
                            bindings.push(KeyBinding::new(default, $($action)+, $context));
                        }
                    }
                }
            )*
            bindings
        }
    };
}

registry! {
    ("run_query", "Run Query", None, ["secondary-enter"], RunQuery),
    // Only the planning mode gets a chord. The other one runs the statement,
    // and a keystroke away from `secondary-enter` is too close to reach for by
    // accident when reaching for it is a write.
    ("explain_query", "Explain Query", None, ["secondary-shift-enter"], ExplainQuery { mode: ExplainMode::Plan }),
    ("format_query", "Format Query", None, ["secondary-shift-f"], FormatQuery),
    ("apply_edits", "Apply Edits", None, ["secondary-s"], ApplyEdits),
    ("rename_query_tab", "Rename Query Tab", None, ["secondary-k s"], SaveQuery),
    ("new_query", "New Query Tab", None, ["secondary-t"], NewQuery),
    ("new_connection", "New Connection", None, ["secondary-shift-n"], NewConnection),
    ("close_tab", "Close Tab", None, ["secondary-w"], CloseTab),
    ("refresh_relation", "Refresh Rows", None, ["secondary-r"], RefreshRelation),
    ("refresh_connection", "Refresh Connection", None, ["secondary-shift-r"], RefreshConnection),
    ("next_tab", "Next Tab", None, ["ctrl-tab"], NextTab),
    ("previous_tab", "Previous Tab", None, ["ctrl-shift-tab"], PreviousTab),
    ("next_profile", "Next Connection", None, ["ctrl-`"], NextProfile),
    ("previous_profile", "Previous Connection", None, ["ctrl-shift-`"], PreviousProfile),
    ("show_editor", "Back / Dismiss", None, ["escape"], ShowEditor),
    ("cycle_theme", "Cycle Theme", None, ["secondary-k t"], CycleTheme),
    ("open_settings", "Open Settings", None, ["secondary-,"], OpenSettings),
    ("fuzzy_open", "Fuzzy Schema Search", None, ["secondary-p"], FuzzyOpen),
    ("command_palette", "Command Palette", None, ["secondary-shift-p"], CommandPalette),
    ("palette_previous", "Palette: Previous Row", Some("Palette > Input"), ["up"], PalettePrevious),
    ("palette_next", "Palette: Next Row", Some("Palette > Input"), ["down"], PaletteNext),
    ("zoom_editor_in", "Zoom Editor In", None, ["secondary-+", "secondary-="], ZoomEditorIn),
    ("zoom_editor_out", "Zoom Editor Out", None, ["secondary--"], ZoomEditorOut),
    ("reset_editor_zoom", "Reset Editor Zoom", None, ["secondary-0"], ResetEditorZoom),
    ("edit_cell", "Edit Cell", Some("Table"), ["enter"], EditCell),
    ("copy_cell", "Copy Cell", Some("Table"), ["secondary-c"], CopyCell),
    // Not the spreadsheet's `ctrl-shift-n`: off macOS that is New Connection's
    // keys, and New Connection wins even with a cell focused -- which left
    // this unreachable there. Clearing a cell with the delete key is the
    // gesture anyway, and it collides with nothing on any platform.
    ("set_null", "Set Cell to NULL", Some("Table"), ["secondary-backspace"], SetNull),
    ("accept_completion", "Accept Completion", Some("Editor > Input"), ["tab"], AcceptCompletion),
    ("toggle_sidebar", "Toggle Sidebar", None, ["secondary-shift-s"], ToggleSidebar),
    ("toggle_row_panel", "Toggle Row Panel", None, ["secondary-shift-i"], ToggleRowPanel),
    ("quit", "Quit dbdelve", None, ["secondary-q"], Quit),
    // Click or command-palette only today; listed so they can be given a
    // chord for the first time.
    ("cancel_query", "Cancel Query", None, [], CancelQuery),
    ("next_page", "Next Page", None, [], NextPage),
    ("previous_page", "Previous Page", None, [], PreviousPage),
    ("clear_filter", "Clear Filter", None, [], ClearFilter),
    ("add_filter", "Add Filter", None, [], AddFilter),
    ("toggle_next_join", "Toggle Join Type", None, [], ToggleNextJoin),
    ("new_row", "New Row", None, [], NewRow),
    // Scoped like `set_null` rather than left global: they act on the grid's
    // active cell, so a chord put on one later must not fire from the editor.
    ("set_empty", "Set Cell to Empty", Some("Table"), [], SetEmpty),
    ("set_default", "Set Cell to Default", Some("Table"), [], SetDefault),
    ("follow_foreign_key", "Follow Foreign Key", None, [], FollowForeignKey),
    ("delete_row", "Delete Row", None, [], DeleteRow),
    ("discard_edits", "Discard Edits", None, [], DiscardEdits),
}

/// Whether a binding scoped to `a` and one scoped to `b` could both be live
/// at once. Two different, specific contexts never overlap; anything global
/// (`None`) does, because a global binding fires regardless of focus.
fn contexts_overlap(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(a), Some(b)) => a == b,
    }
}

/// The chords currently in force for `spec`: its override if it has one,
/// its shipped defaults otherwise.
pub(crate) fn chords_for<'a>(
    spec: &'a KeybindingSpec,
    overrides: &'a HashMap<String, String>,
) -> Vec<&'a str> {
    match overrides.get(spec.id) {
        Some(chord) => vec![chord.as_str()],
        None => spec.defaults.to_vec(),
    }
}

/// The label of whichever action already holds `chord` in a context that
/// would collide with `context`, if any. `exclude_id` is left out of the
/// search so re-saving an action's own binding unchanged never flags
/// against itself.
pub(crate) fn conflict(
    chord: &str,
    context: Option<&str>,
    exclude_id: &str,
    overrides: &HashMap<String, String>,
) -> Option<&'static str> {
    REGISTRY
        .iter()
        .find(|spec| {
            spec.id != exclude_id
                && contexts_overlap(spec.context, context)
                && chords_for(spec, overrides).contains(&chord)
        })
        .map(|spec| spec.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_context_collision_is_a_conflict() {
        let overrides = HashMap::new();
        assert_eq!(
            conflict("secondary-enter", None, "quit", &overrides),
            Some("Run Query")
        );
    }

    #[test]
    fn disjoint_specific_contexts_do_not_conflict() {
        let overrides = HashMap::new();
        // `enter` belongs to Edit Cell, but only inside "Table".
        assert_eq!(
            conflict(
                "enter",
                Some("Editor > Input"),
                "accept_completion",
                &overrides
            ),
            None
        );
    }

    #[test]
    fn override_is_what_gets_checked_against() {
        let mut overrides = HashMap::new();
        overrides.insert("quit".to_string(), "cmd-enter".to_string());
        assert_eq!(
            conflict("cmd-enter", None, "run_query", &overrides),
            Some("Quit dbdelve")
        );
    }

    #[test]
    fn a_captured_keystroke_is_stored_in_a_form_that_reads_back() {
        // What the capture stores is `Keystroke::unparse`. `to_string` is the
        // glyphs a menu draws -- `⌘⇧P` -- and nothing parses those back.
        let captured = Keystroke::parse("cmd-shift-p").unwrap();
        assert!(is_parseable(&captured.unparse()));
    }

    #[test]
    fn an_unreadable_override_falls_back_to_the_default() {
        let mut overrides = HashMap::new();
        overrides.insert("quit".to_string(), "⌘⏎".to_string());
        // `KeyBinding::new` panics on a stroke it cannot parse, so a
        // `profiles.toml` holding one used to take the launch with it.
        assert!(!build_bindings(&overrides).is_empty());
    }

    #[test]
    fn moving_an_action_off_a_chord_stops_it_holding_that_chord() {
        let mut overrides = HashMap::new();
        // Quit no longer sits on `cmd-enter` once this override is set, so
        // only `run_query` (excluded here, since it's the one asking) should
        // be found holding it.
        overrides.insert("quit".to_string(), "cmd-k q".to_string());
        assert_eq!(
            conflict("secondary-enter", None, "run_query", &overrides),
            None
        );
    }
}
