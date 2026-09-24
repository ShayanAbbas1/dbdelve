//! dbdelve's icon set.
//!
//! GPUI renders an SVG by asking the application's [`AssetSource`] for a file,
//! and gpui-component names Lucide files without shipping any. Rather than
//! vendoring another project's artwork into the repository, dbdelve depends on
//! `icondata_lu` — Lucide as Rust data — and serves the documents from memory
//! at the paths GPUI asks for. Nothing is read from disk, so this works the
//! same from `cargo run` and from a bundled `.app`.
//!
//! GPUI paints an SVG as a mask tinted by the element's text colour, so the
//! `currentColor` in the source is never used: an icon is whatever colour the
//! theme token beside it is.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_component::Icon;
use icondata_core::IconData;

/// The icons dbdelve can draw, by the path GPUI asks for.
///
/// The gpui-component widgets ask for their own paths — those are Lucide names
/// too, so they resolve here as well. Add a row when something asks for one;
/// an unlisted path simply draws nothing.
const ICONS: [(&str, &IconData); 40] = [
    ("icons/git-fork.svg", icondata_lu::LuGitFork),
    ("icons/chevron-down.svg", icondata_lu::LuChevronDown),
    ("icons/chevron-right.svg", icondata_lu::LuChevronRight),
    ("icons/chevron-left.svg", icondata_lu::LuChevronLeft),
    ("icons/chevron-up.svg", icondata_lu::LuChevronUp),
    ("icons/chevrons-up-down.svg", icondata_lu::LuChevronsUpDown),
    ("icons/sort-ascending.svg", icondata_lu::LuArrowUpNarrowWide),
    (
        "icons/sort-descending.svg",
        icondata_lu::LuArrowDownWideNarrow,
    ),
    ("icons/file-code.svg", icondata_lu::LuFileCode),
    ("icons/trash.svg", icondata_lu::LuTrash2),
    ("icons/check.svg", icondata_lu::LuCheck),
    ("icons/save.svg", icondata_lu::LuSave),
    ("icons/pencil.svg", icondata_lu::LuPencil),
    ("icons/close.svg", icondata_lu::LuX),
    ("icons/ellipsis.svg", icondata_lu::LuEllipsis),
    ("icons/loader-circle.svg", icondata_lu::LuLoaderCircle),
    ("icons/minus.svg", icondata_lu::LuMinus),
    ("icons/square.svg", icondata_lu::LuSquare),
    ("icons/plus.svg", icondata_lu::LuPlus),
    ("icons/search.svg", icondata_lu::LuSearch),
    ("icons/arrow-up.svg", icondata_lu::LuArrowUp),
    ("icons/arrow-down.svg", icondata_lu::LuArrowDown),
    ("icons/database.svg", icondata_lu::LuDatabase),
    ("icons/table.svg", icondata_lu::LuTable),
    ("icons/layers.svg", icondata_lu::LuLayers),
    ("icons/eye.svg", icondata_lu::LuEye),
    ("icons/hard-drive.svg", icondata_lu::LuHardDrive),
    ("icons/globe.svg", icondata_lu::LuGlobe),
    ("icons/list-tree.svg", icondata_lu::LuListTree),
    ("icons/square-function.svg", icondata_lu::LuSquareFunction),
    ("icons/square-play.svg", icondata_lu::LuSquarePlay),
    ("icons/play.svg", icondata_lu::LuPlay),
    ("icons/square-pen.svg", icondata_lu::LuSquarePen),
    ("icons/history.svg", icondata_lu::LuHistory),
    ("icons/panel-left.svg", icondata_lu::LuPanelLeft),
    ("icons/panel-right.svg", icondata_lu::LuPanelRight),
    ("icons/copy.svg", icondata_lu::LuCopy),
    ("icons/type.svg", icondata_lu::LuType),
    ("icons/arrow-up-right.svg", icondata_lu::LuArrowUpRight),
    ("icons/shield-alert.svg", icondata_lu::LuShieldAlert),
];

/// dbdelve's own names for the icons it draws, so a call site names a thing
/// rather than a file.
pub mod icon {
    pub const CHEVRON_DOWN: &str = "icons/chevron-down.svg";
    /// Windows' caption buttons, which dbdelve draws itself there.
    pub const MINIMIZE: &str = "icons/minus.svg";
    pub const MAXIMIZE: &str = "icons/square.svg";
    pub const CLOSE_WINDOW: &str = "icons/close.svg";
    pub const CHEVRON_RIGHT: &str = "icons/chevron-right.svg";
    pub const CHEVRON_LEFT: &str = "icons/chevron-left.svg";
    pub const SWITCHER: &str = "icons/chevrons-up-down.svg";
    /// A connection's mode, on its pill in the titlebar.
    pub const READ_ONLY: &str = "icons/eye.svg";
    pub const READ_WRITE: &str = "icons/pencil.svg";
    pub const FULL_ACCESS: &str = "icons/shield-alert.svg";
    /// Folds the explorer column away, and brings it back.
    pub const SIDEBAR: &str = "icons/panel-left.svg";
    /// Folds the row panel beside a grid away, and brings it back.
    pub const ROW_PANEL: &str = "icons/panel-right.svg";
    /// A column header's sort state: which way the server ordered the rows, or
    /// that it could be asked to.
    pub const SORT_UP: &str = "icons/sort-ascending.svg";
    pub const SORT_DOWN: &str = "icons/sort-descending.svg";
    pub const SORTABLE: &str = "icons/chevrons-up-down.svg";
    pub const SAVED_QUERY: &str = "icons/file-code.svg";
    pub const DELETE: &str = "icons/trash.svg";
    pub const SEARCH: &str = "icons/search.svg";
    pub const DATABASE: &str = "icons/database.svg";
    pub const TABLE: &str = "icons/table.svg";
    pub const PARTITIONED_TABLE: &str = "icons/layers.svg";
    pub const VIEW: &str = "icons/eye.svg";
    pub const MATERIALIZED_VIEW: &str = "icons/hard-drive.svg";
    pub const FOREIGN_TABLE: &str = "icons/globe.svg";
    pub const FUNCTION: &str = "icons/square-function.svg";
    pub const PROCEDURE: &str = "icons/square-play.svg";
    pub const STRUCTURE: &str = "icons/list-tree.svg";
    pub const PLUS: &str = "icons/plus.svg";
    pub const CHECK: &str = "icons/check.svg";
    pub const CLOSE: &str = "icons/close.svg";
    pub const COPY: &str = "icons/copy.svg";
    /// A floppy disk, which is what "save" looks like everywhere else.
    pub const SAVE: &str = "icons/save.svg";
    pub const RENAME: &str = "icons/pencil.svg";
    pub const RUN: &str = "icons/play.svg";
    /// A query plan: the branching the server said it would do.
    pub const PLAN: &str = "icons/git-fork.svg";
    pub const SCRATCH_QUERY: &str = "icons/square-pen.svg";
    pub const HISTORY: &str = "icons/history.svg";
    /// The connection form's "fill the fields from this URL" action: the URL
    /// flows down into the fields below it.
    pub const FILL_DOWN: &str = "icons/arrow-down.svg";
    pub const FONT: &str = "icons/type.svg";
    /// A cell whose column carries a foreign key: the arrow leaves this row for
    /// the one it references.
    pub const FOLLOW_KEY: &str = "icons/arrow-up-right.svg";
}

pub fn icon(path: &'static str) -> Icon {
    Icon::empty().path(path)
}

pub struct Icons;

impl AssetSource for Icons {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, data)| Cow::Owned(document(data).into_bytes())))
    }

    fn list(&self, _: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().map(|(name, _)| (*name).into()).collect())
    }
}

/// Wrap Lucide's path data in the SVG document GPUI's renderer expects.
fn document(icon: &IconData) -> String {
    let attribute = |name: &str, value: Option<&str>| {
        value
            .map(|value| format!(r#" {name}="{value}""#))
            .unwrap_or_default()
    };

    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg"{}{}{}{}{}{}{}>{}</svg>"#,
        attribute("viewBox", icon.view_box),
        attribute("width", icon.width),
        attribute("height", icon.height),
        attribute("fill", icon.fill),
        attribute("stroke", icon.stroke),
        attribute("stroke-width", icon.stroke_width),
        attribute("stroke-linecap", icon.stroke_linecap),
        icon.data
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_icon_resolves_to_a_document() {
        // A name with no entry in `ICONS` draws nothing at all, and a missing
        // icon is invisible rather than loud -- so the check has to be here.
        for path in [
            icon::CHEVRON_DOWN,
            icon::CHEVRON_RIGHT,
            icon::SWITCHER,
            icon::READ_ONLY,
            icon::READ_WRITE,
            icon::FULL_ACCESS,
            icon::SIDEBAR,
            icon::ROW_PANEL,
            icon::SORT_UP,
            icon::SORT_DOWN,
            icon::SORTABLE,
            icon::SAVED_QUERY,
            icon::DELETE,
            icon::SEARCH,
            icon::DATABASE,
            icon::TABLE,
            icon::PARTITIONED_TABLE,
            icon::VIEW,
            icon::MATERIALIZED_VIEW,
            icon::FOREIGN_TABLE,
            icon::FUNCTION,
            icon::PROCEDURE,
            icon::STRUCTURE,
            icon::PLUS,
            icon::CHECK,
            icon::CLOSE,
            icon::COPY,
            icon::SAVE,
            icon::RENAME,
            icon::RUN,
            icon::PLAN,
            icon::SCRATCH_QUERY,
            icon::FILL_DOWN,
            icon::FONT,
            icon::FOLLOW_KEY,
        ] {
            assert_draws(path);
        }
    }

    #[test]
    fn every_listed_icon_draws_something() {
        // The paths the library's own widgets ask for are never named in
        // dbdelve, so nothing above would notice a row here going bad.
        for (path, _) in ICONS {
            assert_draws(path);
        }
    }

    fn assert_draws(path: &str) {
        let loaded = Icons.load(path).unwrap();
        let document = loaded.unwrap_or_else(|| panic!("{path} has no icon"));
        let document = String::from_utf8(document.to_vec()).unwrap();
        assert!(document.starts_with("<svg"), "{path}: {document}");
        // Any child element counts: Lucide draws with <path>, <polygon>,
        // <circle> and friends, and an icon that names none of them is
        // an invisible icon.
        let content = document.split_once('>').map(|(_, rest)| rest).unwrap_or("");
        assert!(
            content.trim_end().trim_end_matches("</svg>").contains('<'),
            "{path} drew nothing"
        );
    }
}
