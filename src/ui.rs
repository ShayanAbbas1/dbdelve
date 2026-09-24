//! The shared widget vocabulary: the buttons, icons, chrome and readouts every
//! surface is assembled from.
//!
//! Every item here was a free function at the crate root that took a `Theme` and
//! returned an element. They moved out whole; nothing changed but their
//! visibility. Shared so two panels asking the same kind of question cannot end
//! up looking like two different applications.

use gpui::{
    AnyElement, FontWeight, InteractiveElement, IntoElement, Keystroke, ParentElement, Styled, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    InteractiveElementExt,
    button::{Button, ButtonVariants},
    kbd::Kbd,
};

use std::collections::HashMap;

use crate::{
    db::{RelationKind, RoutineKind},
    explorer::ObjectKind,
    icons::icon,
    keybindings,
    sql::Mode,
    theme::{self, ConnectionColor, Theme, layout},
};

/// The icon a sidebar row carries: a grid for a table, layers for one split
/// into partitions, an eye for the kinds that are a saved query over a table, a
/// disk for the one that stores its answer, and a globe for the one that lives
/// on another server entirely.
pub(crate) fn object_icon(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Relation(RelationKind::Table) => icon::TABLE,
        ObjectKind::Relation(RelationKind::PartitionedTable) => icon::PARTITIONED_TABLE,
        ObjectKind::Relation(RelationKind::View) => icon::VIEW,
        ObjectKind::Relation(RelationKind::MaterializedView) => icon::MATERIALIZED_VIEW,
        ObjectKind::Relation(RelationKind::ForeignTable) => icon::FOREIGN_TABLE,
        ObjectKind::Routine(RoutineKind::Function) => icon::FUNCTION,
        ObjectKind::Routine(RoutineKind::Procedure) => icon::PROCEDURE,
    }
}

/// One column of icons down the sidebar, so every label starts at the same x
/// whether its row is a folder or an object.
pub(crate) fn row_icon(t: Theme, path: &'static str) -> impl IntoElement {
    row_icon_tinted(t, path, None)
}

/// `row_icon`, in a connection's own colour. Without one it is `row_icon`
/// exactly, so an uncoloured connection is drawn the way it always was.
pub(crate) fn row_icon_tinted(
    t: Theme,
    path: &'static str,
    color: Option<ConnectionColor>,
) -> impl IntoElement {
    icon(path)
        .size(px(layout::ICON_SIZE))
        .text_color(color.map_or(t.text_faint, ConnectionColor::swatch))
}

/// What this connection is allowed to do, drawn beside its name because "which
/// database am I on" and "what can I do to it" are read in one glance or not at
/// all.
///
/// Drawn as the same tinted pill as the connection switcher beside it, in a
/// hue that climbs with what the mode lets through: green reads, yellow
/// writes, red can do anything.
///
/// A `Button` rather than the plain `Div` this drew as before Task 7: the
/// titlebar hangs a `dropdown_menu` off it, and that trait is bounded on
/// `Selectable`, which `Div` does not implement.
pub(crate) fn mode_pill(t: Theme, mode: Mode) -> Button {
    let (color, path) = match mode {
        Mode::ReadOnly => (ConnectionColor::Green, icon::READ_ONLY),
        Mode::ReadWrite => (ConnectionColor::Yellow, icon::READ_WRITE),
        Mode::Full => (ConnectionColor::Red, icon::FULL_ACCESS),
    };
    // The look is set on a plain `Div` inside rather than on the `Button`, so
    // the button's own label sizing cannot drift it from the switcher's.
    control("connection-mode", Tone::Quiet, Control::Compact)
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .bg(color.fill())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_XS))
                .text_size(px(layout::TEXT_SM))
                .text_color(t.text)
                .font_weight(FontWeight::MEDIUM)
                .child(row_icon_tinted(t, path, Some(color)))
                .child(mode.label())
                .child(row_icon(t, icon::CHEVRON_DOWN)),
        )
}

/// macOS draws its window buttons over a transparent titlebar, and Windows
/// draws none at all into one, so both leave the strip to dbdelve. Linux has no
/// compositor that will draw buttons into it, so it keeps the system bar.
pub(crate) const CLIENT_TITLEBAR: bool = !cfg!(target_os = "linux");

/// dbdelve's own titlebar, drawn where the platform's would be.
///
/// On macOS and Windows the system titlebar is transparent (see `main`), so
/// this row is what runs to the top of the window. It is also the drag handle
/// the platform no longer provides — which is why the drag region is a child
/// covering what is left of the row rather than the row itself: a drag region
/// swallows the clicks a button needs, so anything interactive goes in
/// `leading`, outside it. macOS draws its window buttons over the row's
/// leading inset; Windows gets `caption_buttons` at the trailing end.
///
/// On Linux the window wears a real system titlebar instead, so this row keeps
/// only the job the system one cannot do — saying, through the connection
/// switcher in `leading`, which database is in front of you. Nothing is inset
/// for buttons that are drawn above rather than over it, and moving the window
/// belongs to the bar the compositor drew.
pub(crate) fn titlebar(
    t: Theme,
    mode: Option<AnyElement>,
    leading: Vec<AnyElement>,
) -> impl IntoElement {
    div()
        .h(px(layout::TITLEBAR_HEIGHT))
        .w_full()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(layout::SPACE_MD))
        .pl(px(if cfg!(target_os = "macos") {
            layout::TITLEBAR_LEADING_INSET
        } else {
            layout::SPACE_MD
        }))
        .when(!cfg!(target_os = "windows"), |row| {
            row.pr(px(layout::SPACE_MD))
        })
        .children(leading)
        .child(
            div()
                .id("titlebar")
                .when(CLIENT_TITLEBAR, |strip| {
                    strip
                        .window_control_area(gpui::WindowControlArea::Drag)
                        .on_double_click(|_, window, _| window.titlebar_double_click())
                })
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .children(mode),
        )
        .when(cfg!(target_os = "windows"), |row| {
            row.child(caption_buttons(t))
        })
}

/// Minimise, maximise and close, for Windows, which draws none into a
/// transparent titlebar. Each is only marked with its control area: Windows
/// hit-tests it as its own caption button, so the click, the snap-layout flyout
/// on maximise and closing the window stay the system's.
///
/// ponytail: one square for maximise whether or not the window already is;
/// Windows' restore glyph is two, and `window.is_maximized()` picks it if that
/// ever reads wrong.
fn caption_buttons(t: Theme) -> impl IntoElement {
    let button = |id: &'static str, path: &'static str, area, hover: gpui::Hsla| {
        div()
            .id(id)
            .w(px(layout::TITLEBAR_HEIGHT * 1.25))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .window_control_area(area)
            .hover(move |style| style.bg(hover))
            .child(icon(path).size(px(layout::ICON_SIZE)).text_color(t.text))
    };
    div()
        .h_full()
        .flex()
        .flex_shrink_0()
        .child(button(
            "minimize",
            icon::MINIMIZE,
            gpui::WindowControlArea::Min,
            t.element_hover.into(),
        ))
        .child(button(
            "maximize",
            icon::MAXIMIZE,
            gpui::WindowControlArea::Max,
            t.element_hover.into(),
        ))
        .child(button(
            "close",
            icon::CLOSE_WINDOW,
            gpui::WindowControlArea::Close,
            t.danger.into(),
        ))
}

/// The card every modal is drawn on. Shared so two panels asking the same kind
/// of question cannot end up looking like two different applications.
pub(crate) fn dialog(t: Theme) -> gpui::Div {
    // `occlude`, or a click on one of its buttons also lands on whatever is
    // drawn beneath -- a grid header that re-sorts and re-runs, a cell that
    // moves the selection.
    div()
        .occlude()
        .w(px(layout::DIALOG_WIDTH))
        .p(px(layout::SPACE_LG))
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_MD))
        .bg(t.overlay_glass())
        .border_1()
        .border_color(t.border_strong)
        .rounded(px(layout::RADIUS_PANEL))
        .shadow_lg()
}

/// What a button's fill says. Colour is state here as everywhere else: a dbdelve
/// button is the neutral control tone unless it is the one action its surface
/// exists to take, or the one that destroys something.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Tone {
    Primary,
    Quiet,
    Danger,
}

/// Standard for a dialog or the connection form, where a button sits beside a
/// field and is the thing the surface exists to click. Compact for the strips
/// that are themselves only a control tall. Inline for the affordance revealed
/// on a chip or a row it does not own.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Control {
    Standard,
    Compact,
    Inline,
}

impl Tone {
    /// The colour of the label and the icon, pinned rather than inherited — see
    /// [`button`].
    pub(crate) fn ink(self, t: Theme) -> theme::color::Srgb {
        match self {
            Tone::Primary => t.text,
            Tone::Quiet => t.text_muted,
            Tone::Danger => t.on_accent,
        }
    }
}

impl Control {
    pub(crate) fn height(self) -> f32 {
        match self {
            Control::Standard => layout::CONTROL_HEIGHT,
            Control::Compact => layout::CONTROL_HEIGHT_COMPACT,
            Control::Inline => layout::CONTROL_HEIGHT_INLINE,
        }
    }

    pub(crate) fn text_size(self) -> f32 {
        match self {
            Control::Standard => layout::TEXT_MD,
            Control::Compact | Control::Inline => layout::TEXT_SM,
        }
    }
}

/// A dbdelve button.
///
/// gpui-component supplies the mechanism — the tooltip and the keybinding in
/// it, the focus ring, the disabled gate — and none of the appearance survives
/// contact with it. Its size scale bottoms out at a 20px box with 4px of
/// padding, its label comes out at the library's 16px body rather than dbdelve's
/// 13, and 0.5.1 tints button content `red_400` on hover from a hardcoded
/// colour no theme token reaches.
///
/// So the box is measured here off the layout scale, and the content goes in as
/// a child carrying its own colour. That last part is what settles the hover
/// tint: a child that sets a colour does not inherit the container's.
pub(crate) fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    tone: Tone,
    size: Control,
    t: Theme,
) -> Button {
    control(id, tone, size)
        .px(px(layout::SPACE_MD))
        .when(size == Control::Standard, |standard| {
            standard.min_w(px(layout::CONTROL_MIN_WIDTH))
        })
        .child(button_label(label, tone, size, t))
}

/// A button that is only its icon, so it is square and reads as an
/// affordance beside the thing it acts on rather than as a control of its own.
pub(crate) fn icon_button(
    id: impl Into<gpui::ElementId>,
    path: &'static str,
    tone: Tone,
    size: Control,
    t: Theme,
) -> Button {
    control(id, tone, size).w(px(size.height())).p_0().child(
        icon(path)
            .size(px(layout::ICON_SIZE))
            .text_color(tone.ink(t)),
    )
}

/// A button's words. Separate so the two delete buttons, which grow a
/// confirmation beside their icon once armed, can add them without becoming a
/// different control.
pub(crate) fn button_label(
    label: impl Into<gpui::SharedString>,
    tone: Tone,
    size: Control,
    t: Theme,
) -> impl IntoElement {
    div()
        .flex_none()
        // Or the descenders decide where the text sits in the box.
        .line_height(gpui::relative(1.))
        .text_size(px(size.text_size()))
        .font_weight(FontWeight::MEDIUM)
        .text_color(tone.ink(t))
        .child(label.into())
}

/// The box, without its content. Radius comes from the theme, which is already
/// pointed at `RADIUS_CONTROL`.
pub(crate) fn control(id: impl Into<gpui::ElementId>, tone: Tone, size: Control) -> Button {
    Button::new(id)
        .map(|button| match tone {
            Tone::Primary => button.primary(),
            Tone::Quiet => button.ghost(),
            Tone::Danger => button.danger(),
        })
        .h(px(size.height()))
}

/// The quietest thing on screen: small, uppercase, and dim enough that the
/// names under it are what the eye lands on first.
pub(crate) fn section_label(t: Theme, label: &str) -> impl IntoElement {
    div()
        .text_size(px(layout::TEXT_XS))
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.text_faint)
        .child(label.to_uppercase())
}

/// A keycap, drawn the way the platform draws one in a menu. Reads a stroke in
/// GPUI's binding syntax so the hint and the binding cannot drift apart.
pub(crate) fn keycap(stroke: &'static str) -> Kbd {
    Kbd::new(Keystroke::parse(stroke).expect("keycap strokes are compile-time constants"))
}

/// The same keycap for a stroke only known at runtime -- a rebound chord read
/// back off disk, which [`keycap`]'s constant cannot cover. One GPUI cannot
/// parse draws nothing rather than a cap with garbage on it.
pub(crate) fn keycap_for(stroke: &str) -> Option<Kbd> {
    Keystroke::parse(stroke).ok().map(Kbd::new)
}

/// The same cap as plain text, for the readouts that run as a sentence rather
/// than carrying a chip of their own. `Kbd`'s own formatter, so a cap written
/// into a string reads the way the drawn ones do -- `⌘0` on macOS, `Ctrl+0`
/// where there is no Command key.
///
/// Space-separated strokes are a two-stroke chord, which `Kbd` has no notion
/// of: each stroke is formatted on its own and they are joined the way the
/// binding is written.
pub(crate) fn keycap_text(chord: &str) -> String {
    chord
        .split_whitespace()
        .filter_map(|stroke| Keystroke::parse(stroke).ok().map(|key| Kbd::format(&key)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The cap to draw beside an action, read from the keymap rather than written
/// beside the label: a hint spelled out at the call site goes on claiming the
/// shipped chord after the action has been rebound to another one.
pub(crate) fn chord_hint(id: &str, overrides: &HashMap<String, String>) -> String {
    keybindings::REGISTRY
        .iter()
        .find(|spec| spec.id == id)
        .and_then(|spec| keybindings::chords_for(spec, overrides).first().copied())
        .map(keycap_text)
        .unwrap_or_default()
}

/// A shortcut hint and what it does, in the app face rather than the editor's
/// monospace -- these are sentences about the UI, not query output.
pub(crate) fn key_hint(
    t: Theme,
    stroke: &'static str,
    explanation: &'static str,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .text_size(px(layout::TEXT_SM))
        .text_color(t.text_faint)
        .child(keycap(stroke))
        .child(explanation)
}

/// A row count as a chip label: `1K` rather than `1,000`, because four of these
/// sit side by side and the grouped form is twice as wide for no more meaning.
pub(crate) fn compact_count(rows: usize) -> String {
    match rows >= 1_000 && rows.is_multiple_of(1_000) {
        true => format!("{}K", rows / 1_000),
        false => rows.to_string(),
    }
}

/// `1234567` → `1,234,567`. Row counts are read at a glance, and groups are
/// what keeps six digits legible.
pub(crate) fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// The row count for the status bar.
///
/// A restored snapshot keeps at most [`store::GRID_ROW_CAP`] rows of what the
/// result had, so the grid can hold fewer rows than the count it reports --
/// and "20,000 rows" over 5,000 of them is a number nobody can act on. Every
/// other result reads as the plain count it always did.
pub(crate) fn row_readout(showing: usize, total: usize) -> String {
    let unit = if total == 1 { "row" } else { "rows" };
    if showing < total {
        return format!(
            "{} of {} {unit}",
            group_thousands(showing as u64),
            group_thousands(total as u64)
        );
    }
    format!("{} {unit}", group_thousands(total as u64))
}

/// How long ago, at the one unit worth reading in a status bar. A cache's age
/// is read to decide whether to trust it, and no such decision turns on the
/// difference between 121 and 122 minutes.
pub(crate) fn relative_age(seconds: u64) -> String {
    match seconds {
        ..60 => "moments".to_string(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Bytes at the precision a person reads them, not the count the server sent.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explorer;

    #[test]
    fn a_hint_follows_a_rebound_action_rather_than_its_shipped_chord() {
        let mut overrides = HashMap::new();
        let shipped = chord_hint("refresh_relation", &overrides);
        overrides.insert(
            "refresh_relation".to_string(),
            "secondary-shift-r".to_string(),
        );
        let rebound = chord_hint("refresh_relation", &overrides);
        assert!(!shipped.is_empty());
        assert_ne!(shipped, rebound);
        // An action nothing has bound draws no cap at all.
        assert_eq!(chord_hint("cancel_query", &overrides), "");
    }

    #[test]
    fn a_row_limit_reads_as_a_chip_not_as_a_number() {
        assert_eq!(compact_count(100), "100");
        assert_eq!(compact_count(1_000), "1K");
        assert_eq!(compact_count(100_000), "100K");
        // Every offered limit has to be labelled by this, so none can come out
        // as something like `1500`.
        for rows in explorer::ROW_LIMITS {
            assert!(compact_count(rows).len() <= 4, "{rows} is a wide label");
        }
    }

    #[test]
    fn a_snapshots_age_reads_in_one_unit() {
        assert_eq!(relative_age(0), "moments");
        assert_eq!(relative_age(59), "moments");
        assert_eq!(relative_age(60), "1m");
        assert_eq!(relative_age(3_599), "59m");
        assert_eq!(relative_age(7_200), "2h");
        assert_eq!(relative_age(86_399), "23h");
        // A cache left over a long weekend, which is exactly when its age is
        // the thing worth reading.
        assert_eq!(relative_age(3 * 86_400 + 4_000), "3d");
    }

    #[test]
    fn a_capped_snapshots_readout_says_how_much_of_the_result_it_holds() {
        // The status bar over a restored, capped grid. Without the "of" it
        // reads as 20,000 rows on screen, and the export below it as complete.
        assert_eq!(row_readout(5_000, 20_000), "5,000 of 20,000 rows");
        // Nothing was trimmed: the count it always was.
        assert_eq!(row_readout(842, 842), "842 rows");
        assert_eq!(row_readout(1, 1), "1 row");
        assert_eq!(row_readout(0, 0), "0 rows");
        // A snapshot written before `total_rows` was kept reports zero over
        // rows it can still show, and must not read as "5,000 of 0".
        assert_eq!(row_readout(5_000, 0), "0 rows");
    }

    #[test]
    fn row_counts_are_grouped_for_reading() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(5000), "5,000");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }

    #[test]
    fn byte_counts_read_at_human_precision() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(578_923), "578.9 KB");
        assert_eq!(human_bytes(1_500_000), "1.5 MB");
    }
}
