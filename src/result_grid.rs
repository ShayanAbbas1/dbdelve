use std::cmp::Ordering;
use std::rc::Rc;

use gpui::{
    App, AppContext, Context, Div, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Point, SharedString, Stateful, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    InteractiveElementExt,
    input::{Input, InputState},
    menu::{PopupMenu, PopupMenuItem},
    table::TableEvent,
    table::{Column, TableDelegate, TableState},
};

use crate::{
    ShowReferences, Workspace,
    db::{self, EditTarget, QueryResult},
    export::{self, RowsAs},
    icons::icon,
    sql::{Mode, SortKey},
    store::{GRID_ROW_CAP, StoredGrid, captured_at},
    theme::{ConnectionColor, color::Srgb, layout, theme},
    ui::{Control, Tone, icon_button},
};

/// ponytail: a column is a few hundred pixels wide, so shaping more than this is
/// work nobody can see -- and `db` deliberately keeps whole values, which run to
/// megabytes for JSONB and PostGIS. The row inspector is where a whole value
/// gets read; this is the visual-only clip the spec's §4.4 allows.
const CELL_DISPLAY_LIMIT: usize = 300;
/// How far `clip` reads into a value with line breaks before giving up on
/// finding the rest of what it would show. Generous next to the limit, so an
/// indented document still fills its cell.
const CLIP_SCAN_LIMIT: usize = 16 * CELL_DISPLAY_LIMIT;

/// ponytail: the inspector shows the value, not a column's worth of it -- but
/// "the value" has to stop somewhere, because a multi-megabyte document laid
/// out as wrapped text stalls the frame it is laid out in. `cmd+c` on the cell
/// still copies all of it. Raise this if a real value gets cut.
const FIELD_DISPLAY_LIMIT: usize = 4_000;

/// What an absent value is called wherever one is shown.
pub const NULL_LABEL: SharedString = SharedString::new_static("NULL");

/// What a staged `DEFAULT` is called in the cell holding it. Its own word
/// rather than NULL's: the server resolves the two to different values, and a
/// cell that painted one of them for the other would be lying about the write.
const DEFAULT_LABEL: SharedString = SharedString::new_static("DEFAULT");

/// Where every column starts and the narrowest a drag can leave it.
const MIN_COLUMN_WIDTH: f32 = 180.0;

/// The table's column 0 is the row-number gutter, which is not part of the
/// result: every index the library hands the delegate is one past the result's
/// own, and every index the delegate hands back is put one past it.
const GUTTER: usize = 1;
/// One digit's advance in the grid's monospaced face, near enough to size the
/// row-number column to its widest number.
const DIGIT_WIDTH: f32 = 8.0;

/// A gutter cell: the row number's box, evenly padded. It draws no divider of
/// its own -- the library closes a fixed column with one.
fn gutter_cell() -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .px(px(layout::SPACE_SM))
}

/// The relations pointing at a column: an index into the structure's
/// `referenced_by` and the label to show for it.
type ReferenceMenu = Rc<Vec<(usize, SharedString)>>;

/// A row's schema and table, and its primary key as column/value pairs — what a
/// one-row `DELETE` needs and nothing else.
pub type RowKey = (String, String, Vec<(String, String)>);

/// A grid's selection, apart from the grid it was made in.
pub struct GridSelection {
    active: Option<(usize, usize)>,
    rows: std::collections::BTreeSet<usize>,
    anchor: Option<usize>,
}

pub struct ResultGrid {
    columns: Vec<Column>,
    result: QueryResult,
    /// Per column, read for every visible cell on every frame, and
    /// `db::is_numeric_type` lowercases the type name to answer.
    numeric: Vec<bool>,
    /// The keys the rows are in order by, as column indices: a readout of the
    /// statement's `ORDER BY` when the server sorted them, or the view's own
    /// keys when `sort_in_memory` did.
    sort: Vec<(usize, bool)>,
    /// Whether a header click can sort this result at all. A control that does
    /// nothing is worse than no control.
    sortable: bool,
    /// Where each row held sat in the result the server sent, for a sort in
    /// memory to break ties by and to return to when its last key is clicked
    /// away. Empty until the first such sort, which is every row where it was.
    fetched: Vec<usize>,
    /// The cell a keystroke acts on. dbdelve's, not the library's: gpui-component
    /// tracks a selected row *or* a selected column as mutually exclusive
    /// modes and never a cell, so a coordinate has to be assembled here or
    /// `Enter` has no target. A click sets it outright; the library's arrow
    /// keys reach it through `select_row` and `select_col`. Both paths end in
    /// `set_active`, so there is one answer to where the user is.
    active: Option<(usize, usize)>,
    /// How wide the scrolling columns were laid out last frame, for
    /// `keep_active_in_view`.
    laid_out_width: Pixels,
    /// What the user has changed and not yet applied. No value in
    /// `result.rows` is ever written -- a sort in memory moves whole rows, and
    /// these with them -- so the grid can always show pending against
    /// as-fetched and discarding is dropping this.
    ///
    /// ponytail: a linear scan per visible cell per frame, over the handful of
    /// cells one person edits between applies. A map keyed by `(row, col)` is
    /// the upgrade path if that handful ever becomes thousands.
    pending: Vec<PendingEdit>,
    /// The one cell showing an input, if any. At most one: an input is a far
    /// heavier thing to draw than the text every other cell paints.
    editing: Option<Editing>,
    /// When these rows were snapshotted, for a grid that came off disk.
    ///
    /// Held here rather than on the tab because a completed run replaces the
    /// whole delegate: there is no field anyone has to remember to clear, and
    /// so no way for a live result to keep claiming it is a snapshot.
    captured: Option<u64>,
    /// Which of a run's result sets these rows are, while the run is still
    /// appending them (`db::Fed::set`). Dropped with the delegate, like
    /// `captured`, when the finished result replaces it.
    streamed: Option<usize>,
    /// When a streamed run's kept rows reached the limit, for its clock to
    /// stop at.
    filled: Option<std::time::Instant>,
    /// Whether a restored grid's edits wait on the user accepting that its rows
    /// may be stale. Session-only, and dropped with the delegate like
    /// `captured` is, so a run's own rows never ask.
    unconfirmed: bool,
    /// Which result columns carry a foreign key, as indices into `columns`.
    /// Empty until a relation's structure says otherwise, and empty forever on
    /// a query result: a statement can join as many relations as it likes, so
    /// there is no one relation whose keys these columns could be.
    foreign_keys: Vec<usize>,
    /// Which of this result's columns the server declared `NOT NULL`, and
    /// which it gave a default. Indices into `columns`, filled from a
    /// relation's structure the way `foreign_keys` is — so a column absent from
    /// both lists is one nothing has said anything about, which is every column
    /// of a query result.
    not_nullable: Vec<usize>,
    has_default: Vec<usize>,
    /// The hover group each key column's cells share, one per key column and
    /// built where the keys are marked. `render_td` runs for every visible cell
    /// every frame, and a group named there would be a `format!` per cell per
    /// frame.
    follow_groups: Vec<SharedString>,
    /// For each column another relation points at, the menu of those relations:
    /// an index into the structure's `referenced_by` and the label to show.
    /// Built where the references are marked, for the reason `follow_groups`
    /// is, and shared into each cell's menu.
    reference_menus: Vec<(usize, ReferenceMenu)>,
    /// The result columns that make up the relation's primary key, marked
    /// where the structure arrives, by name for the reason `foreign_keys` is.
    primary_key: Vec<usize>,
    /// The rows picked out by clicks in the gutter or cells, for a multi-row copy.
    /// Separate from `active`, which is the single cell the keyboard and the
    /// single-cell gestures act on.
    selected_rows: std::collections::BTreeSet<usize>,
    /// The row a plain click lands on, which a shift-click
    /// extends a range from.
    row_selection_anchor: Option<usize>,
    /// A row the pointer has just selected itself. Telling the library about
    /// it echoes back as `TableEvent::SelectRow`, which must not collapse the
    /// range or toggle the click just made the way a keyboard move does.
    pointer_row: Option<usize>,
    /// Where the pointer last went down on a reference arrow, for the popup it
    /// opens to hang from.
    reference_anchor: Option<Point<Pixels>>,
    /// The connection's mode, cached because `editable` is asked by the grid's
    /// own double-click handler, which has no route back to the profile.
    /// `Workspace::set_mode` is the only thing that writes it after construction.
    mode: Mode,
    /// Asked, never matched on, which of the column types is bytes.
    engine: db::Engine,
    /// The table's own focus handle, recorded by the right click that opens the
    /// row menu. The menu dispatches its action into whatever holds focus, and
    /// `context_menu` is handed the delegate alone -- the table is mid-update
    /// there, so its handle cannot be read back out of the entity.
    focus: Option<FocusHandle>,
}

/// What a pending edit will write. Three states rather than two, because both
/// keywords are writes dbdelve cannot spell as a value: `'NULL'` and
/// `'DEFAULT'` are the words, and the column's own default is a thing only the
/// server knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewValue {
    /// Quoted as the characters it is. A user who types `DEFAULT` into a cell
    /// means the seven-character string; the keyword has its own arm.
    Value(SharedString),
    Null,
    Default,
}

/// What a non-NULL cell sorts as. Numbers rank ahead of text, so a document
/// field holding both, or a number column holding a value that does not read
/// as one, still has one order to be put in.
enum SortValue<'a> {
    Number(f64),
    Text(&'a str),
}

impl SortValue<'_> {
    /// Total, as `sort_by` needs: `total_cmp` places NaN rather than refusing
    /// to compare it.
    fn order(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => a.total_cmp(b),
            // Case folded, so a column of names does not put every capital
            // ahead of every lowercase letter.
            (Self::Text(a), Self::Text(b)) => a
                .chars()
                .flat_map(char::to_lowercase)
                .cmp(b.chars().flat_map(char::to_lowercase)),
            (Self::Number(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Number(_)) => Ordering::Greater,
        }
    }
}

/// One changed cell, held beside the fetched value rather than over it.
struct PendingEdit {
    row: usize,
    col: usize,
    /// What will be written. Whole, because this is what the `UPDATE` carries.
    value: NewValue,
    /// What the column paints, clipped the way a fetched value is, and absent
    /// for either keyword for the same reason a fetched NULL is: the cell
    /// already paints the keyword in italics, and a second spelling of it on
    /// screen is one too many.
    shown: Option<SharedString>,
}

struct Editing {
    row: usize,
    col: usize,
    /// Built on the first render of the cell, because an input needs a window
    /// and opening an edit deliberately does not.
    input: Option<Entity<InputState>>,
}

/// One row's worth of pending edits, resolved to real column names and ready
/// for `sql::update_row`. Alias resolution happens here so the caller does none.
pub struct PendingRow {
    pub schema: String,
    pub table: String,
    /// Real column name and its new value, one per changed column.
    pub sets: Vec<(String, NewValue)>,
    /// The key columns' real names against their **as-fetched** values: the row
    /// is identified by what the server holds, not by what the user has typed.
    pub keys: Vec<(String, String)>,
    /// Real column name against its type, where the result says it.
    pub types: Vec<(String, String)>,
}

impl ResultGrid {
    /// An empty grid never has an edit target, so `editable` refuses on that
    /// alone -- the mode it starts with cannot matter, and callers that build
    /// one before a profile is known (`new_grid`) have no mode to give it.
    pub fn empty() -> Self {
        Self::new(QueryResult::default(), Mode::default())
    }

    pub fn new(result: QueryResult, mode: Mode) -> Self {
        let numeric = result
            .columns
            .iter()
            .map(|column| column.data_type.as_deref().is_some_and(db::is_numeric_type))
            .collect();
        let columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                Column::new(index.to_string(), column.name.clone())
                    .width(px(MIN_COLUMN_WIDTH))
                    .min_width(px(MIN_COLUMN_WIDTH))
                    .resizable(true)
                    .movable(false)
                    // The cell padding is dbdelve's, applied in `render_td` and
                    // `render_th`. Taking the library's as well indents every
                    // value twice and leaves a column's width unknowable here.
                    .p_0()
            })
            .collect();

        Self {
            columns,
            sort: Vec::new(),
            sortable: false,
            fetched: Vec::new(),
            result,
            numeric,
            active: None,
            laid_out_width: Pixels::ZERO,
            pending: Vec::new(),
            editing: None,
            captured: None,
            streamed: None,
            filled: None,
            unconfirmed: false,
            foreign_keys: Vec::new(),
            not_nullable: Vec::new(),
            has_default: Vec::new(),
            follow_groups: Vec::new(),
            reference_menus: Vec::new(),
            primary_key: Vec::new(),
            selected_rows: std::collections::BTreeSet::new(),
            row_selection_anchor: None,
            pointer_row: None,
            reference_anchor: None,
            mode,
            engine: db::Engine::default(),
            focus: None,
        }
    }

    pub fn with_engine(mut self, engine: db::Engine) -> Self {
        self.engine = engine;
        self
    }

    /// The sort the statement asked the server for, so the headers can say
    /// which columns the rows are ordered by and in which direction, and
    /// whether asking for another one is possible at all.
    pub fn with_sort(mut self, sort: Vec<(usize, bool)>, sortable: bool) -> Self {
        self.sort = sort;
        self.sortable = sortable;
        self
    }

    /// A view's sort in memory, applied to rows that have just arrived. The
    /// keys name columns as `filter::sort_expression` writes them, so each
    /// finds its column in this result by name, and a key whose column is gone
    /// orders nothing. `None` is a view the server sorts, which keeps the
    /// readout `with_sort` gave it.
    pub fn with_client_sort(mut self, keys: Option<&[SortKey]>) -> Self {
        if let Some(keys) = keys {
            let order = crate::filter::sort_columns(self.engine, keys, &self.result.columns);
            self.sort_in_memory(order);
        }
        self
    }

    pub fn sort(&self) -> &[(usize, bool)] {
        &self.sort
    }

    /// A grid read back from a snapshot.
    ///
    /// The edit target and column types come back with it, so it can be
    /// edited -- but only once the user has said the rows are fresh enough to
    /// edit against (see [`ResultGrid::confirm_stale`]). A snapshot written
    /// before those were kept has neither, and stays read-only until a run.
    pub fn restored(stored: &StoredGrid, mode: Mode) -> Self {
        let mut grid = Self::new(
            QueryResult {
                columns: stored
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(index, name)| db::Column {
                        name: name.clone(),
                        data_type: stored.data_types.get(index).cloned().flatten(),
                    })
                    .collect(),
                rows: stored.rows.clone(),
                capped_from: (stored.total_rows > stored.rows.len()).then_some(stored.total_rows),
                stopped: stored.stopped,
                // All or none: a snapshot naming a type this build does not
                // know is read as having none, never as rows out of step.
                cell_types: stored
                    .cell_types
                    .iter()
                    .map(|types| types.iter().map(|name| db::cell_type(name)).collect())
                    .collect::<Option<_>>()
                    .unwrap_or_default(),
                edit: stored.edit.clone(),
                ..QueryResult::default()
            },
            mode,
        );

        // The headers say what the rows are ordered by. `sortable` stays false:
        // whether a header click can do anything is a fact about the statement,
        // and a snapshot is not a statement -- the next run settles it.
        for (column, width) in grid.columns.iter_mut().zip(&stored.widths) {
            column.width = px(width.max(MIN_COLUMN_WIDTH));
        }
        grid.sort = stored.sort.clone();
        let (rows, columns) = (grid.result.rows.len(), grid.columns.len());
        // A snapshot is capped, so the cell that was active may be past the
        // rows that came back with it -- and that is not a cell.
        grid.active = stored
            .active
            .filter(|(row, col)| *row < rows && *col < columns);
        grid.captured = Some(stored.captured);
        grid.unconfirmed = stored.edit.is_some();
        grid
    }

    /// When a restored grid's rows were snapshotted, or `None` for rows a run
    /// put here.
    pub fn captured(&self) -> Option<u64> {
        self.captured
    }

    /// A grid for result set `set` of a run's rows to arrive in.
    pub fn streaming(mut self, set: usize) -> Self {
        self.streamed = Some(set);
        self
    }

    pub fn streamed_set(&self) -> Option<usize> {
        self.streamed
    }

    /// The columns' types, learned while their rows are still arriving. The
    /// rows are kept; what is worked out from the types is worked out again.
    /// Their values are rendered apart, off the frame thread
    /// ([`db::unrendered`]).
    pub fn set_column_types(&mut self, columns: Vec<db::Column>) {
        self.numeric = columns
            .iter()
            .map(|column| column.data_type.as_deref().is_some_and(db::is_numeric_type))
            .collect();
        self.result.columns = columns;
    }

    /// Put values rendered off the frame thread in place, each only where the
    /// cell still holds what was rendered: a sort or a new run in between has
    /// moved or replaced it, and a value written there would be another
    /// row's.
    pub fn apply_rendered(&mut self, rendered: Vec<(usize, usize, String, String)>) {
        for (row, column, was, now) in rendered {
            if let Some(Some(cell)) = self
                .result
                .rows
                .get_mut(row)
                .and_then(|cells| cells.get_mut(column))
                && *cell == was
            {
                *cell = now;
            }
        }
    }

    pub fn fill(&mut self, at: Option<std::time::Instant>) {
        self.filled = at;
    }

    pub fn filled(&self) -> Option<std::time::Instant> {
        self.filled
    }

    pub fn append_rows(&mut self, rows: Vec<Vec<db::Cell>>) {
        self.result.rows.extend(rows);
    }

    /// The rows streamed in, for the finished result to be built around.
    pub fn take_rows(&mut self) -> Vec<Vec<db::Cell>> {
        std::mem::take(&mut self.result.rows)
    }

    /// Whether an edit here has to be confirmed against stale rows first.
    pub fn unconfirmed(&self) -> bool {
        self.unconfirmed
    }

    pub fn confirm_stale(&mut self) {
        self.unconfirmed = false;
    }

    /// How many rows the result behind this grid had. More than the grid holds
    /// when the row limit or a snapshot's cap trimmed it.
    pub fn total_rows(&self) -> usize {
        self.result.total_rows()
    }

    /// What a snapshot of this grid keeps. The tab's own fields -- the
    /// statement, the row limit -- are the caller's to fill in: the grid does
    /// not know which kind of tab it is in.
    ///
    /// `pending` and `editing` are deliberately absent. An unapplied edit is
    /// against rows this session fetched, and a restored grid is not those rows.
    pub fn stored(&self) -> StoredGrid {
        StoredGrid {
            columns: self
                .result
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            // Capped here rather than in `write_grid`, which would have to
            // clone the whole vector to keep the front of it.
            rows: self
                .result
                .rows
                .iter()
                .take(GRID_ROW_CAP)
                .cloned()
                .collect(),
            // The result's own size, which a restored grid knows and does not
            // hold: recomputing it from the capped rows is what collapsed a
            // 20,000-row snapshot to 5,000 on the next save.
            total_rows: self.total_rows(),
            stopped: self.result.stopped,
            sort: self.sort.clone(),
            order_by: Vec::new(),
            client_sort: None,
            widths: self
                .columns
                .iter()
                .map(|column| f32::from(column.width))
                .collect(),
            active: self.active,
            last_query: None,
            limit: None,
            filter: String::new(),
            showing_structure: false,
            // A snapshot that is written back unchanged keeps its own age: it
            // is still the rows it was, and restamping it would make every
            // restart claim the cache was just taken.
            captured: self.captured.unwrap_or_else(captured_at),
            edit: self.result.edit.clone(),
            cell_types: self
                .result
                .cell_types
                .iter()
                .take(GRID_ROW_CAP)
                .map(|types| types.iter().map(|alias| alias.to_string()).collect())
                .collect(),
            data_types: self
                .result
                .columns
                .iter()
                .map(|column| column.data_type.clone())
                .collect(),
        }
    }

    /// The widths the table is drawing, which is where a drag lands: the
    /// library resizes its own copy of the columns.
    pub fn set_widths(&mut self, widths: &[gpui::Pixels]) {
        for (column, width) in self.columns.iter_mut().zip(widths) {
            column.width = *width;
        }
    }

    pub fn columns(&self) -> &[crate::db::Column] {
        &self.result.columns
    }

    pub fn layout(&self) -> (Vec<String>, Vec<gpui::Pixels>) {
        (
            self.result
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            self.columns.iter().map(|column| column.width).collect(),
        )
    }

    pub fn with_layout(mut self, names: &[String], widths: &[gpui::Pixels]) -> Self {
        let same = self.result.columns.len() == names.len()
            && self
                .result
                .columns
                .iter()
                .zip(names)
                .all(|(column, name)| column.name == *name);
        if same {
            self.set_widths(widths);
        }
        self
    }

    /// Record which of this result's columns carry a foreign key, given the
    /// column names the relation's structure says do.
    ///
    /// ponytail: matched by name, not by position. A preview is `SELECT *` of
    /// one relation, so its column names are the relation's; an aliased or
    /// computed projection therefore matches nothing and offers no arrow. That
    /// is the right failure — an arrow placed by position would filter the
    /// referenced relation by a value from some other column.
    pub fn mark_foreign_keys(&mut self, columns: &[String]) {
        self.foreign_keys = self.columns_named(columns);
        self.follow_groups = self
            .foreign_keys
            .iter()
            .map(|col| SharedString::from(format!("follow-key-{col}")))
            .collect();
    }

    /// Record which of this result's columns other relations point at, given
    /// the references from the structure and the schema the relation lives in.
    /// Matched by name for the reason [`ResultGrid::mark_foreign_keys`] is. A
    /// reference is labelled `table.column`, with the schema in front when it is
    /// not this relation's own.
    pub fn mark_references(&mut self, references: &[db::Reference], schema: &str) {
        self.reference_menus = self
            .result
            .columns
            .iter()
            .enumerate()
            .filter_map(|(col, column)| {
                let menu: Vec<(usize, SharedString)> = references
                    .iter()
                    .enumerate()
                    .filter(|(_, reference)| reference.referenced_column == column.name)
                    .map(|(index, reference)| {
                        let label = match reference.schema == schema {
                            true => format!("{}.{}", reference.table, reference.column),
                            false => format!(
                                "{}.{}.{}",
                                reference.schema, reference.table, reference.column
                            ),
                        };
                        (index, SharedString::from(label))
                    })
                    .collect();
                (!menu.is_empty()).then(|| (col, Rc::new(menu)))
            })
            .collect();
    }

    /// Wide enough for the last row's number and no wider, padded the same on
    /// both sides.
    fn gutter_width(&self) -> f32 {
        let digits = self.result.rows.len().max(1).to_string().len() as f32;
        layout::grid(digits * DIGIT_WIDTH) + 2.0 * layout::SPACE_SM
    }

    /// Cells and the gutter share one anchor, so Shift can be pressed after
    /// the first click and a range can be extended from either surface.
    fn click_row(&mut self, row: usize, shift: bool, toggle: bool) {
        if shift {
            let anchor = *self.row_selection_anchor.get_or_insert(row);
            let (start, end) = (anchor.min(row), anchor.max(row));
            if !toggle {
                self.selected_rows.clear();
            }
            self.selected_rows.extend(start..=end);
            return;
        }
        if toggle {
            if !self.selected_rows.remove(&row) {
                self.selected_rows.insert(row);
            }
            self.row_selection_anchor = Some(row);
            return;
        }
        self.selected_rows.clear();
        self.selected_rows.insert(row);
        self.row_selection_anchor = Some(row);
    }

    /// What a right click does to the selection: a row already in it is left
    /// untouched, so the menu acts on the selection the user made; a row
    /// outside it becomes the whole selection, the way a plain left click
    /// would -- so "Copy Row(s) As" never copies a selection the click
    /// landed nowhere near.
    fn right_click_row(&mut self, row: usize) {
        if !self.selected_rows.contains(&row) {
            self.click_row(row, false, false);
        }
    }

    /// What the user has picked out, to be carried onto the finished result
    /// of the rows it was picked from.
    pub fn selection(&self) -> GridSelection {
        GridSelection {
            active: self.active,
            rows: self.selected_rows.clone(),
            anchor: self.row_selection_anchor,
        }
    }

    /// Take `selection` back if every row and cell it names is still here:
    /// `false`, with nothing taken, otherwise or when it picks nothing.
    pub fn select(&mut self, selection: GridSelection) -> bool {
        let (rows, columns) = (self.result.rows.len(), self.columns.len());
        let fits = selection
            .active
            .is_none_or(|(row, col)| row < rows && col < columns)
            && selection.rows.iter().all(|row| *row < rows)
            && selection.anchor.is_none_or(|row| row < rows);
        if !fits || (selection.active.is_none() && selection.rows.is_empty()) {
            return false;
        }
        self.active = selection.active;
        self.selected_rows = selection.rows;
        self.row_selection_anchor = selection.anchor;
        true
    }

    pub fn clear_row_selection(&mut self) {
        self.selected_rows.clear();
        self.row_selection_anchor = None;
    }

    pub fn mark_primary_key(&mut self, columns: &[String]) {
        self.primary_key = self.columns_named(columns);
    }

    fn key_icon(&self, col: usize, color: Srgb) -> Option<impl IntoElement> {
        self.primary_key.contains(&col).then(|| {
            icon(icon::PRIMARY_KEY)
                .size(px(layout::grid(12.)))
                .flex_shrink_0()
                .text_color(color)
        })
    }

    pub fn reference_anchor(&self) -> Option<Point<Pixels>> {
        self.reference_anchor
    }

    /// The relations pointing at this column, as an index into the structure's
    /// `referenced_by` and the label to show for each.
    pub fn reference_choices(&self, col: usize) -> Vec<(usize, SharedString)> {
        self.reference_menu(col)
            .map(|menu| menu.as_ref().clone())
            .unwrap_or_default()
    }

    fn reference_menu(&self, col: usize) -> Option<&ReferenceMenu> {
        self.reference_menus
            .iter()
            .find(|(column, _)| *column == col)
            .map(|(_, menu)| menu)
    }

    /// Record which of this result's columns the server declared `NOT NULL`
    /// and which it gave a default, so the cell menu can drop an entry that
    /// would only ever be refused.
    ///
    /// Matched by name for the reason [`ResultGrid::mark_foreign_keys`] is, and
    /// degrading the same way: a column in neither list is one nothing has been
    /// said about, and every entry is offered there rather than none.
    ///
    /// The lists are the caller's, not read off a structure here, because
    /// `has_default` is not purely a fact about the column — see
    /// `Workspace::mark_columns`.
    pub fn mark_columns(&mut self, not_nullable: &[String], has_default: &[String]) {
        self.not_nullable = self.columns_named(not_nullable);
        self.has_default = self.columns_named(has_default);
    }

    fn columns_named(&self, names: &[String]) -> Vec<usize> {
        self.result
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| names.contains(&column.name))
            .map(|(index, _)| index)
            .collect()
    }

    /// The hover group a key column's cells share, and `None` for a column
    /// nothing can be followed from.
    fn follow_group(&self, col: usize) -> Option<&SharedString> {
        let key = self.foreign_keys.iter().position(|key| *key == col)?;
        self.follow_groups.get(key)
    }

    /// Whether this column's cells can be followed to the row they reference.
    pub fn follows_a_key(&self, col: usize) -> bool {
        self.foreign_keys.contains(&col)
    }

    /// The whole value behind a cell, not the clipped one the grid paints: a
    /// column is a couple of hundred pixels wide and a JSONB document is not,
    /// and copying what happens to fit would be the same bug as reading a value
    /// through the column.
    pub(crate) fn cell(&self, row_ix: usize, col_ix: usize) -> Option<&str> {
        self.result.rows.get(row_ix)?.get(col_ix)?.as_deref()
    }

    /// What a fetched cell paints, worked out as it is drawn rather than kept:
    /// a clipped copy of every cell held a second copy of most of a result,
    /// and cost a pass over all of it before the first frame, when only the
    /// cells on screen are ever drawn.
    ///
    /// A number too long to show whole is left ungrouped: grouping copies the
    /// entire value, and a `numeric` can run to 131,072 digits, which would be
    /// copied again on every frame it is on screen.
    fn shown(&self, row_ix: usize, col_ix: usize) -> Option<SharedString> {
        let value = self.cell(row_ix, col_ix)?;
        let shown = match self.is_numeric_column(col_ix) && value.len() <= CELL_DISPLAY_LIMIT {
            true => clip(&grouped_digits(value)),
            false => clip(value),
        };
        Some(shown.into())
    }

    /// Whether the row has no such field at all: a document store's absent
    /// field, which paints as nothing rather than as a NULL it does not hold.
    fn missing(&self, row_ix: usize, col_ix: usize) -> bool {
        self.result
            .cell_types
            .get(row_ix)
            .and_then(|types| types.get(col_ix))
            == Some(&db::MISSING)
    }

    /// Every column of one row, named, typed where the type is known, and
    /// carrying the value itself rather than the string the column had room
    /// for. This is what the row inspector reads.
    pub fn fields(&self, row_ix: usize) -> Vec<Field> {
        if row_ix >= self.result.rows.len() {
            return Vec::new();
        }

        self.result
            .columns
            .iter()
            .enumerate()
            .map(|(col_ix, column)| Field {
                name: column.name.clone().into(),
                // The cell's own type where it has one: the column's is `mixed`
                // as soon as two documents disagree.
                data_type: self
                    .result
                    .cell_types
                    .get(row_ix)
                    .and_then(|types| types.get(col_ix))
                    .map(|alias| SharedString::new_static(alias))
                    .or_else(|| column.data_type.clone().map(SharedString::from)),
                missing: self.missing(row_ix, col_ix),
                // Reformatted before it is clipped, never after: a document cut
                // at 4,000 characters does not parse, and the inspector would
                // fall back to the one long line for exactly the values big
                // enough to need the help.
                value: self.cell(row_ix, col_ix).map(|value| {
                    let value = indented_json(value);
                    clip_to(&value, FIELD_DISPLAY_LIMIT).into()
                }),
            })
            .collect()
    }

    /// What a cell sorts as, `None` for a NULL or a missing field. A column
    /// the server typed as a number, or a document's numeric field, sorts by
    /// value; one typed as anything else sorts as its text, as the server's
    /// own `ORDER BY` would; one nothing typed sorts by value wherever the
    /// text reads as a number.
    ///
    /// Dates and times sort as text: every engine renders them year first and
    /// zero-padded, so text order is time order.
    ///
    /// ponytail: not for BC dates or a column mixing UTC offsets, and `f64`
    /// ties integers past 2^53 that differ only in their last digits (they
    /// keep the server's order). Parse those properly if anyone sorts them in
    /// memory.
    fn sort_value(&self, row: usize, col: usize) -> Option<SortValue<'_>> {
        let text = self.cell(row, col)?;
        let tag = self
            .result
            .cell_types
            .get(row)
            .and_then(|types| types.get(col));
        let numeric = match (tag, self.column_type(col)) {
            (Some(tag), _) => db::is_numeric_type(tag),
            (None, Some(_)) => self.is_numeric_column(col),
            (None, None) => true,
        };
        if numeric && let Ok(number) = text.trim().parse() {
            return Some(SortValue::Number(number));
        }
        Some(SortValue::Text(text))
    }

    /// Where the server put the row this grid now holds at `row`.
    fn fetched_at(&self, row: usize) -> usize {
        self.fetched.get(row).copied().unwrap_or(row)
    }

    /// The rows in `keys`' order, as indices into the rows as they stand.
    /// Ties keep the order the server sent the rows in, and with no keys every
    /// row does, which is how a sort clicked away undoes itself.
    ///
    /// NULL goes last whichever way a key points: an absent value is not a
    /// small one, and a descending sort that opens on a screen of NULLs has
    /// buried what it was asked for.
    fn client_order(&self, keys: &[(usize, bool)]) -> Vec<usize> {
        // Read once per row rather than once per comparison: parsing a number
        // n log n times over is most of the cost of a sort.
        let values: Vec<Vec<Option<SortValue<'_>>>> = (0..self.result.rows.len())
            .map(|row| {
                keys.iter()
                    .map(|&(col, _)| self.sort_value(row, col))
                    .collect()
            })
            .collect();
        let mut order: Vec<usize> = (0..values.len()).collect();
        order.sort_by(|&a, &b| {
            keys.iter()
                .zip(values[a].iter().zip(&values[b]))
                .map(|(&(_, ascending), pair)| match pair {
                    (None, None) => Ordering::Equal,
                    (None, Some(_)) => Ordering::Greater,
                    (Some(_), None) => Ordering::Less,
                    (Some(x), Some(y)) if ascending => x.order(y),
                    (Some(x), Some(y)) => y.order(x),
                })
                .find(|ordering| ordering.is_ne())
                .unwrap_or_else(|| self.fetched_at(a).cmp(&self.fetched_at(b)))
        });
        order
    }

    /// Put the rows held here in `keys`' order. Nothing is fetched or run:
    /// this is a view sorted in memory, which a header click there and every
    /// result landing in it both come through.
    ///
    /// Pending edits move with their rows, since they are what the user typed
    /// against those rows. The ring, the row selection and an open input are
    /// dropped, as a sort the server runs drops them by replacing the grid:
    /// each names a position, and a different row is there now.
    pub fn sort_in_memory(&mut self, keys: Vec<(usize, bool)>) {
        let order = self.client_order(&keys);
        let mut moved_to = vec![0; order.len()];
        for (to, &from) in order.iter().enumerate() {
            moved_to[from] = to;
        }
        self.fetched = order.iter().map(|&row| self.fetched_at(row)).collect();
        reorder(&mut self.result.rows, &order);
        // All or none, as a snapshot restores them.
        if self.result.cell_types.len() == order.len() {
            reorder(&mut self.result.cell_types, &order);
        }
        for edit in &mut self.pending {
            edit.row = moved_to[edit.row];
        }
        self.active = None;
        self.editing = None;
        self.pointer_row = None;
        self.clear_row_selection();
        self.sort = keys;
        // A sort in memory asks nothing of the statement, so every header
        // takes a click.
        self.sortable = true;
    }
}

/// The editing half of the grid: what the user has changed, and not one
/// statement of SQL. Generating and running that is the workspace's job, which
/// is why every one of these is computable without a window.
impl ResultGrid {
    /// The active cell's column when the grid has been laid out at a new
    /// `width`, scrolled `offset_x` (zero or negative), and the cell showed at
    /// least partly at the old one. `None` while the width holds, since
    /// scrolling on every frame would notify on every frame, and `None` for a
    /// cell already scrolled out of sight, which a resize must not jump back to.
    /// Both widths are the scrolling columns' viewport, the gutter excluded.
    fn relaid_out(&mut self, width: Pixels, offset_x: Pixels) -> Option<usize> {
        let was = std::mem::replace(&mut self.laid_out_width, width);
        if was == width {
            return None;
        }
        let (_, col) = self.active?;
        let left: Pixels = self.columns.get(..col)?.iter().map(|c| c.width).sum();
        let right = left + self.columns.get(col)?.width;
        (right + offset_x > Pixels::ZERO && left + offset_x < was).then_some(col)
    }

    /// The cell `Enter` acts on, if the user has reached one. `None` on a
    /// result set nobody has touched yet, and on every new one.
    pub fn active(&self) -> Option<(usize, usize)> {
        self.active
    }

    /// What the server returned, whole: the rows an export writes out, and not
    /// the clipped strings the columns had room for. Pending edits are
    /// not folded in, because this is the result set, not the grid's view of it.
    pub fn result(&self) -> &QueryResult {
        &self.result
    }

    /// The selected rows as text in `kind`'s shape: what "Copy Rows As" puts on
    /// the clipboard. The whole values, not the clipped ones the cells paint,
    /// in row order. Every row of a multi-row selection, or else the active
    /// cell's row alone -- so the entry works exactly as it did before there
    /// was a selection to make plural.
    pub fn rows_as(&self, kind: RowsAs) -> Option<String> {
        let rows = self.selected_row_indices();
        if rows.is_empty() {
            return None;
        }
        let result = QueryResult {
            columns: self.result.columns.clone(),
            rows: rows
                .iter()
                .filter_map(|&row| self.result.rows.get(row).cloned())
                .collect(),
            cell_types: rows
                .iter()
                .filter_map(|&row| self.result.cell_types.get(row).cloned())
                .collect(),
            ..QueryResult::default()
        };
        Some(export::render_rows_as(
            kind,
            self.engine,
            self.result.edit.as_ref(),
            &result,
        ))
    }

    /// The rows "Copy Rows As" and the row-number gutter's own highlight agree
    /// on: every row the user has selected there, in order, or the active
    /// cell's row alone when nothing has been.
    pub fn selected_row_indices(&self) -> Vec<usize> {
        if self.row_selection_anchor.is_some() {
            return self.selected_rows.iter().copied().collect();
        }
        self.active.map(|(row, _)| vec![row]).unwrap_or_default()
    }

    /// The whole value behind the active cell, which is what `cmd+c` copies —
    /// not the clipped string the column had room for. `None` while an input is
    /// open, because there `cmd+c` is the input's own text selection, and on a
    /// NULL, which is an absent value rather than the word painted for one.
    pub fn active_value(&self) -> Option<&str> {
        if self.editing.is_some() {
            return None;
        }
        let (row, col) = self.active?;
        self.cell(row, col)
    }

    /// Fold the library's row selection into the active cell. Its own arrow-key
    /// actions move that selection, so folding the event here is what makes
    /// them move the ring, and there is no competing binding to fight.
    ///
    /// The column is kept: moving down a column is not moving out of it. With
    /// nothing active yet the first column is the origin, because a keystroke
    /// on a grid has to leave the ring somewhere readable.
    ///
    /// Also folds into the row selection, exactly as a plain click on this
    /// row would: without it, arrow-key movement would leave a stale
    /// selection behind for "Copy Row(s) As" to copy and the gutter to tint,
    /// while the ring itself moved on. Not for the echo of a pointer
    /// selection, which has already chosen the rows.
    pub fn select_row(&mut self, row: usize) {
        if self.pointer_row.take() != Some(row) {
            self.click_row(row, false, false);
        }
        self.set_active(row, self.active.map_or(0, |(_, col)| col));
    }

    /// The same fold for a column change, keeping the row. A header click lands
    /// here too, and is deliberately not special-cased: on a sortable result
    /// a sort drops the ring either way -- the server's by replacing this
    /// delegate wholesale, one in memory in `sort_in_memory` -- and on one
    /// that cannot be sorted the ring sitting at the top of the column the
    /// user just pointed at is where their last action was.
    pub fn select_col(&mut self, col: usize) {
        self.set_active(self.active.map_or(0, |(row, _)| row), col);
    }

    /// Move the ring to a cell. An input open on some other cell goes with it:
    /// one cell holding a focused input while another wears the ring `Enter`
    /// follows is two cells claiming the keyboard.
    fn set_active(&mut self, row: usize, col: usize) {
        if self
            .editing
            .as_ref()
            .is_some_and(|editing| (editing.row, editing.col) != (row, col))
        {
            self.editing = None;
        }
        self.active = Some((row, col));
    }

    /// `sort_in_memory` for rows a snapshot restored already in `keys`' order.
    /// The sort is then a no-op on positions, so the restored ring still names
    /// the cell it did.
    pub fn sort_restored_in_memory(&mut self, keys: Vec<(usize, bool)>) {
        let active = self.active;
        self.sort_in_memory(keys);
        self.active = active;
    }

    /// Called whenever the connection's mode changes, so a grid that was
    /// built before the change does not go on answering `editable` from a
    /// stale cache. See `Workspace::set_mode`.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Whether this cell can be written back at all: the result has to be
    /// traceable to one table, the column has to exist in it, and it must not
    /// be part of the key — a key edit is the one edit whose result cannot be
    /// re-verified afterwards, so the spec's §3 refuses it.
    ///
    /// Nor a binary column. What the grid shows there is a blob *literal* the
    /// user could not type a replacement for anyway, and the value that came
    /// back would be written as the text it looks like — see
    /// [`db::Engine::is_binary_type`].
    pub fn editable(&self, row: usize, col: usize) -> bool {
        // Structural only: the mode lives one step further in, on `set_pending`.
        // A Read-only connection still opens its cells, because an open input is
        // how a value is selected and copied out of one -- what it refuses is
        // recording the change, which is where the mode prompt belongs.
        let Some(edit) = &self.result.edit else {
            return false;
        };
        row < self.result.rows.len()
            && edit.columns.get(col).is_some_and(Option::is_some)
            && !edit.keys.contains(&col)
            && ![self.column_type(col), self.cell_type(row, col)]
                .into_iter()
                .flatten()
                .any(|data_type| self.engine.is_binary_type(data_type))
    }

    fn column_type(&self, col: usize) -> Option<&str> {
        self.result.columns.get(col)?.data_type.as_deref()
    }

    /// The cell's own type where the result types each cell (a MongoDB
    /// field holds whatever each document put there), else its column's.
    fn cell_type(&self, row: usize, col: usize) -> Option<&str> {
        match self
            .result
            .cell_types
            .get(row)
            .and_then(|types| types.get(col))
        {
            Some(tag) => Some(tag),
            None => self.column_type(col),
        }
    }

    fn is_numeric_column(&self, col: usize) -> bool {
        self.numeric.get(col).copied().unwrap_or(false)
    }

    /// Whether the cell menu offers each of the three staged values. Written
    /// here rather than read off the menu, so the gating is testable without a
    /// window: a menu is the one thing in the grid a test cannot open.
    ///
    /// Two of them are offered where nothing is known, and the third is not.
    /// A `NULL` or an empty string the column refuses is an error the server
    /// gives back on apply, which is a truthful answer; `DEFAULT` without a
    /// default is a keyword the statement cannot even carry.
    pub fn offers_null(&self, col: usize) -> bool {
        !self.not_nullable.contains(&col)
    }

    /// Never on a number, where the empty string is not a value at all — it
    /// fails on Postgres and is silently coerced to `0` on MySQL.
    pub fn offers_empty(&self, col: usize) -> bool {
        !self.is_numeric_column(col)
    }

    pub fn offers_default(&self, col: usize) -> bool {
        self.has_default.contains(&col)
    }

    /// The row's table and its whole primary key, named and valued, or nothing.
    ///
    /// The same question [`ResultGrid::editable`] asks, answered for a whole row
    /// instead of one cell, because deleting a row and editing a cell of it need
    /// exactly the same thing: a predicate that reaches this row and no other.
    /// `None` where the result traces to no table, where the index has outlived
    /// the rows, and where any key column came back NULL — `=` does not find a
    /// NULL, so a predicate built from one reaches nothing.
    pub fn row_key(&self, row: usize) -> Option<RowKey> {
        let edit = self.result.edit.as_ref()?;
        if row >= self.result.rows.len() {
            return None;
        }
        Some((
            edit.schema.clone(),
            edit.table.clone(),
            self.key_values(edit, row)?,
        ))
    }

    /// Open an input on a cell. `false` when the cell is not editable, and
    /// nothing at all happens then: the notice belongs to `main.rs`.
    pub fn begin_edit(&mut self, row: usize, col: usize) -> bool {
        if self.unconfirmed || !self.editable(row, col) {
            return false;
        }
        self.editing = Some(Editing {
            row,
            col,
            input: None,
        });
        true
    }

    /// Abandon the open input, leaving nothing behind.
    pub fn cancel_edit(&mut self) {
        self.editing = None;
    }

    /// Record a new value for a cell. `false` when the cell is not editable, in
    /// which case nothing is recorded.
    pub fn set_pending(&mut self, row: usize, col: usize, value: NewValue) -> bool {
        if self.unconfirmed || !self.editable(row, col) {
            return false;
        }

        // An input seeds itself with the value as fetched, so opening an edit
        // and closing it without typing arrives here carrying the server's own
        // value back. That is not an edit, and recording it would write a
        // rendered value over the value it was rendered from. Nulling a cell
        // the server already left NULL is the same non-edit, which comparing
        // the absences rather than their renderings is what catches.
        //
        // `Default` is never that no-op: what the default resolves to is the
        // server's answer, so a cell already holding it is indistinguishable
        // from one that is not.
        let unchanged = match &value {
            NewValue::Value(value) => self.cell(row, col) == Some(value.as_ref()),
            // A missing field is not a null, so nulling one is an edit.
            NewValue::Null => self.cell(row, col).is_none() && !self.missing(row, col),
            NewValue::Default => false,
        };
        if unchanged {
            self.pending
                .retain(|edit| (edit.row, edit.col) != (row, col));
            return true;
        }

        // The one mode gate behind every write the grid can start -- the input's
        // commit, `set_null`, and the double-click that reaches both. Below the
        // no-op above on purpose: closing an input without typing is not an edit,
        // and a Read-only connection should not be asked to raise its mode for it.
        if self.mode < Mode::ReadWrite {
            return false;
        }

        // Bytes, not characters: it only has to be cheap and never under-count,
        // and `clip` is a no-op on anything that turns out to fit. Grouped the
        // same way a fetched value is, on the same column check `shown`
        // uses, so a pending edit does not stand out from the rows around it
        // by losing its separators -- the value staged for the `UPDATE` stays
        // exactly as typed, since this is what the cell paints and nothing else.
        let shown = match &value {
            NewValue::Value(value) if self.is_numeric_column(col) => {
                let grouped = grouped_digits(value);
                Some(
                    match grouped.len() > CELL_DISPLAY_LIMIT || grouped.contains(['\n', '\r']) {
                        true => SharedString::from(clip(&grouped)),
                        false => SharedString::from(grouped),
                    },
                )
            }
            NewValue::Value(value) => Some(
                match value.len() > CELL_DISPLAY_LIMIT || value.contains(['\n', '\r']) {
                    true => SharedString::from(clip(value)),
                    false => value.clone(),
                },
            ),
            NewValue::Null | NewValue::Default => None,
        };
        match self
            .pending
            .iter_mut()
            .find(|edit| edit.row == row && edit.col == col)
        {
            Some(edit) => {
                edit.value = value;
                edit.shown = shown;
            }
            None => self.pending.push(PendingEdit {
                row,
                col,
                value,
                shown,
            }),
        }
        true
    }

    /// Stage a value on a cell from outside the input, closing any input open
    /// over it. `false` when the cell is not editable, and nothing happens then.
    ///
    /// The one implementation behind every such gesture. The cell menu, the
    /// palette and the keystroke all dispatch the same action, so there is
    /// nothing here that can behave differently depending on which was used.
    pub fn stage(&mut self, row: usize, col: usize, value: NewValue) -> bool {
        if !self.set_pending(row, col, value) {
            return false;
        }
        // An input still holding the old text would commit it back on the next
        // `Enter`, over the value that was just asked for.
        if self
            .editing
            .as_ref()
            .is_some_and(|editing| (editing.row, editing.col) == (row, col))
        {
            self.editing = None;
        }
        true
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The pending cell after the ring in reading order (before it, when
    /// `back`), wrapping at either end. Reading order rather than edit order:
    /// the walk should move through the grid the way the eye does.
    fn pending_beside(&self, back: bool) -> Option<(usize, usize)> {
        let mut cells: Vec<_> = self
            .pending
            .iter()
            .map(|edit| (edit.row, edit.col))
            .collect();
        cells.sort_unstable();
        let found = self.active.and_then(|here| match back {
            false => cells.iter().find(|&&cell| cell > here),
            true => cells.iter().rev().find(|&&cell| cell < here),
        });
        found
            .or(match back {
                false => cells.first(),
                true => cells.last(),
            })
            .copied()
    }

    /// Back to exactly what the server returned.
    pub fn discard_pending(&mut self) {
        self.pending.clear();
        self.editing = None;
    }

    /// One entry per changed row, in the order the rows were first edited, with
    /// every changed column of that row in a single `SET`.
    ///
    /// A row whose key is not fully readable is dropped rather than guessed at:
    /// a `NULL` in a key column, or a key column the result set does not carry,
    /// leaves dbdelve unable to name the row.
    pub fn pending_updates(&self) -> Vec<PendingRow> {
        let Some(edit) = &self.result.edit else {
            return Vec::new();
        };

        let mut rows: Vec<usize> = Vec::new();
        for row in self.pending.iter().map(|edit| edit.row) {
            if !rows.contains(&row) {
                rows.push(row);
            }
        }

        rows.into_iter()
            .filter_map(|row| {
                let sets: Vec<(String, NewValue)> = self
                    .pending
                    .iter()
                    .filter(|pending| pending.row == row)
                    .filter_map(|pending| {
                        let name = edit.columns.get(pending.col)?.clone()?;
                        Some((name, pending.value.clone()))
                    })
                    .collect();
                if sets.is_empty() {
                    return None;
                }
                Some(PendingRow {
                    schema: edit.schema.clone(),
                    table: edit.table.clone(),
                    sets,
                    keys: self.key_values(edit, row)?,
                    types: self.row_types(row),
                })
            })
            .collect()
    }

    /// Each edit target column's real name against the type the result gave
    /// its cell in `row`, for the writers that spell a literal by its type.
    pub fn row_types(&self, row: usize) -> Vec<(String, String)> {
        let Some(edit) = &self.result.edit else {
            return Vec::new();
        };
        edit.columns
            .iter()
            .enumerate()
            .filter_map(|(col, name)| Some((name.clone()?, self.cell_type(row, col)?.to_string())))
            .collect()
    }

    /// The row's key columns, named and carrying the value the server sent.
    /// `None` if any of them is missing, which is what refuses the whole row.
    fn key_values(&self, edit: &EditTarget, row: usize) -> Option<Vec<(String, String)>> {
        edit.keys
            .iter()
            .map(|&col| {
                let name = edit.columns.get(col)?.clone()?;
                Some((name, self.cell(row, col)?.to_string()))
            })
            .collect()
    }

    fn pending_at(&self, row: usize, col: usize) -> Option<&PendingEdit> {
        self.pending
            .iter()
            .find(|edit| edit.row == row && edit.col == col)
    }

    /// The input for the cell being edited, created on its first render. `None`
    /// for every other cell, which is every cell but one.
    fn editing_input(
        &mut self,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<InputState>> {
        let editing = self.editing.as_ref()?;
        if (editing.row, editing.col) != (row, col) {
            return None;
        }
        if let Some(input) = &editing.input {
            return Some(input.clone());
        }

        // The whole value, not the clipped one: this is the value being
        // changed. A NULL seeds as the empty string because that is the only
        // thing an input can hold; typing one back is the `SetNull` action's
        // job, not the input's (spec §3).
        let seed = match self.pending_at(row, col) {
            // Either keyword seeds empty for the same reason a fetched NULL
            // does: neither is text an input could hold and commit back.
            Some(pending) => match &pending.value {
                NewValue::Value(value) => value.clone(),
                NewValue::Null | NewValue::Default => SharedString::default(),
            },
            None => self
                .cell(row, col)
                .map(|value| SharedString::from(value.to_string()))
                .unwrap_or_default(),
        };
        let input = cx.new(|cx| InputState::new(window, cx));
        // `set_value` rather than `default_value`: only the former leaves the
        // caret after the text, and an edit starts from the end of the value.
        input.update(cx, |input, cx| input.set_value(seed, window, cx));
        // The keystrokes that follow belong to the value rather than to the
        // grid's selection, so the input takes focus as it appears.
        input.focus_handle(cx).focus(window, cx);
        self.editing.as_mut()?.input = Some(input.clone());
        Some(input)
    }

    /// Take the open input's value into the pending set. `false` when the mode
    /// refused the write, and the input is left open then: the prompt its caller
    /// raises is answered by raising the mode and pressing `enter` again.
    fn commit_edit(&mut self, cx: &App) -> bool {
        let Some(editing) = self.editing.as_ref() else {
            return true;
        };
        let (row, col) = (editing.row, editing.col);
        let Some(input) = editing.input.clone() else {
            self.editing = None;
            return true;
        };
        if !self.set_pending(row, col, NewValue::Value(input.read(cx).value().clone())) {
            return false;
        }
        self.editing = None;
        true
    }
}

/// One column of one row, for the inspector panel.
pub struct Field {
    pub name: SharedString,
    /// The server's own name for the column's type, when dbdelve could learn it
    /// without running the statement twice.
    pub data_type: Option<SharedString>,
    pub value: Option<SharedString>,
    /// The row has no such field at all, which is not a NULL.
    pub missing: bool,
}

enum Step {
    Rows(isize),
    Cols(isize),
}

/// Take an open input's value and hand the keyboard back to the grid. `false`
/// when the mode refused the write.
fn commit_from_input(
    table: &mut TableState<ResultGrid>,
    window: &mut Window,
    cx: &mut Context<TableState<ResultGrid>>,
) -> bool {
    cx.stop_propagation();
    cx.notify();
    if !table.delegate_mut().commit_edit(cx) {
        // A mode refusal has an action attached -- raise the mode -- and the
        // grid has nowhere to put one. The input stays open behind the prompt,
        // so answering it and pressing the key again commits what was typed.
        window.dispatch_action(Box::new(crate::RequestWriteMode), cx);
        return false;
    }
    table.focus_handle(cx).focus(window, cx);
    true
}

/// Commit, then carry the edit one cell along, the way a spreadsheet does:
/// `up`/`down` by row, `tab`/`shift-tab` by column. At an edge there is nowhere
/// to go, and the key does nothing, the same as `left` at the caret's start.
///
/// Stepped from the active cell rather than handed to the table's own
/// `SelectDown` or `SelectNextColumn`: the grid runs the library in row and
/// column selection, not its cell mode, so whichever half it last tracked is
/// stale.
fn commit_and_step(
    table: &mut TableState<ResultGrid>,
    step: Step,
    window: &mut Window,
    cx: &mut Context<TableState<ResultGrid>>,
) {
    cx.stop_propagation();
    let grid = table.delegate();
    let Some(to) = grid
        .active()
        .and_then(|from| step_target(from, &step, grid.rows_count(cx), grid.columns.len()))
    else {
        return;
    };
    if !commit_from_input(table, window, cx) {
        return;
    }
    // A cell that cannot be edited just takes the ring.
    table.delegate_mut().begin_edit(to.0, to.1);
    match step {
        Step::Rows(_) => table.set_selected_row(to.0, cx),
        Step::Cols(_) => table.set_selected_col(to.1 + GUTTER, cx),
    }
}

/// Move the ring to the next pending edit (the previous one when `back`) and
/// scroll it into view. The row goes through `set_selected_row`, whose echo
/// lands in `select_row` keeping the column set here, so the row selection
/// follows the ring the way an arrow key would leave it.
///
/// The grid takes focus because the button clicked to get here does not: an
/// input open on another cell closes with the move and takes focus with it,
/// and `Enter` on the new ring would otherwise reach nothing, or the buffer.
pub(crate) fn step_pending(
    table: &mut TableState<ResultGrid>,
    back: bool,
    window: &mut Window,
    cx: &mut Context<TableState<ResultGrid>>,
) {
    let Some((row, col)) = table.delegate().pending_beside(back) else {
        return;
    };
    table.focus_handle(cx).focus(window, cx);
    table.delegate_mut().set_active(row, col);
    table.set_selected_row(row, cx);
    table.scroll_to_col(col + GUTTER, cx);
}

/// Scroll the active cell back into view when the grid's width changes under
/// it. The row panel is what usually changes it: the click that selects a row
/// opens the panel beside the grid, and any scroll into view that click caused
/// measured the grid before it narrowed, leaving a cell in the right-most
/// columns under the panel.
///
/// Run after the table's prepaint, the first point the new width is known.
/// Answers the horizontal offset to glide to, leaving the offset where it was.
pub(crate) fn keep_active_in_view(
    table: &mut TableState<ResultGrid>,
    cx: &mut Context<TableState<ResultGrid>>,
) -> Option<Pixels> {
    let handle = table.horizontal_scroll_handle.clone();
    let width = handle.bounds().size.width;
    // At no width `scroll_to_col` defers to the list's next layout, which the
    // jump back below could not undo.
    if width <= Pixels::ZERO {
        return None;
    }
    let from = handle.offset();
    let col = table.delegate_mut().relaid_out(width, from.x)?;
    // The library keeps the offset that brings a column into view private;
    // jumping there is the only way to learn it, so jump back straight after.
    table.scroll_to_col(col + GUTTER, cx);
    let to = handle.offset().x;
    handle.set_offset(from);
    // A glide that goes nowhere would still stop a wheel's in flight.
    (to != from.x).then_some(to)
}

/// The cell one `step` from `from` in a `rows` by `cols` grid, or `None` at the
/// edge the step points off.
fn step_target(
    (row, col): (usize, usize),
    step: &Step,
    rows: usize,
    cols: usize,
) -> Option<(usize, usize)> {
    let to = match *step {
        Step::Rows(by) => (row.checked_add_signed(by).filter(|&r| r < rows)?, col),
        Step::Cols(by) => (row, col.checked_add_signed(by).filter(|&c| c < cols)?),
    };
    Some(to)
}

/// A plain number with its integer digits in threes, `1723858791` as
/// `1,723,858,791`, as the status bar groups its counts. Display only: `shown` is what the cell paints and the
/// copy, the inspector and every statement read the value as fetched. Anything
/// that is not a bare decimal -- an exponent, a currency sign -- is left alone.
fn grouped_digits(value: &str) -> String {
    let (sign, unsigned) = match value.strip_prefix(['-', '+']) {
        Some(rest) => (&value[..1], rest),
        None => ("", value),
    };
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (unsigned, None),
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(whole) || fraction.is_some_and(|fraction| !digits(fraction)) || whole.len() <= 3 {
        return value.to_string();
    }
    let mut grouped = format!("{sign}{}", crate::ui::group_digits(whole));
    if let Some(fraction) = fraction {
        grouped.push('.');
        grouped.push_str(fraction);
    }
    grouped
}

/// What a cell paints: one line, cut to the display limit.
///
/// A row is one line tall, and a value with line breaks in it was laid out over
/// several and centred, so the cell showed whichever line fell in the middle --
/// for a view's DDL, a column from half way down it. Each run of line breaks
/// becomes a space here, so the cell reads from the start of the value. The
/// value itself is untouched: the inspector lays it out as it is, and a copy
/// takes the original.
///
/// Reads no further into the value than what it keeps, and a bounded run of the
/// blank space it drops: a cell can hold a geometry of a couple of million
/// characters, and every visible cell is clipped on every frame. So only a
/// break inside the part that shows flattens it -- one further in is past the
/// `…` -- and a value that is mostly blank past an early break is cut at
/// [`CLIP_SCAN_LIMIT`] bytes rather than walked to its end.
fn clip(value: &str) -> String {
    let shown = match value.char_indices().nth(CELL_DISPLAY_LIMIT) {
        Some((end, _)) => &value[..end],
        None => value,
    };
    if !shown.contains(['\n', '\r']) {
        return clip_to(value, CELL_DISPLAY_LIMIT);
    }

    // Each line trimmed, the empty ones dropped, the rest joined by a space --
    // streamed, so it stops one character past the limit.
    let mut flattened = String::new();
    let mut kept = 0;
    let mut in_line = false;
    let mut joined = false;
    let mut space_from = None;
    for (at, c) in value.char_indices() {
        if at >= CLIP_SCAN_LIMIT {
            flattened.push('…');
            return flattened;
        }
        if c == '\n' || c == '\r' {
            joined |= in_line;
            in_line = false;
            space_from = None;
        } else if c.is_whitespace() {
            if in_line {
                space_from.get_or_insert(at);
            }
        } else {
            let gap = match space_from.take() {
                Some(from) => &value[from..at],
                None if !in_line && joined => " ",
                None => "",
            };
            for c in gap.chars().chain([c]) {
                flattened.push(c);
                kept += 1;
                if kept > CELL_DISPLAY_LIMIT {
                    return clip_to(&flattened, CELL_DISPLAY_LIMIT);
                }
            }
            in_line = true;
        }
    }
    flattened
}

/// A JSON object or array laid out over indented lines, or the value unchanged
/// where it is not one.
///
/// Parsed rather than taken from the column's type. SQLite has no JSON type to
/// report — a document there lives in whatever `text` column was declared for
/// it — so the type name answers this question for two engines out of three,
/// and the value itself answers it for all of them.
///
/// Objects and arrays only. A bare number, string or `true` is also valid JSON,
/// and reformatting one produces the same characters back.
///
/// ponytail: runs on every repaint of the inspector rather than caching, which
/// the leading-byte check is what makes affordable — it costs one comparison
/// for a UUID or a timestamp and only reaches the parser for a value already
/// shaped like a document. Cache per cell if a wide row of large documents ever
/// makes the panel feel slow.
fn indented_json(value: &str) -> String {
    let trimmed = value.trim_start();
    if !trimmed.starts_with(['{', '[']) {
        return value.to_string();
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .and_then(|parsed| serde_json::to_string_pretty(&parsed).ok())
        .unwrap_or_else(|| value.to_string())
}

/// Cut to a character count, never a byte count: slicing bytes panics in the
/// middle of a codepoint, and the values here are arbitrary user data.
fn clip_to(value: &str, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value.to_string(),
    }
}

/// `items` in `order`, where `order` holds each index exactly once.
fn reorder<T: Default>(items: &mut Vec<T>, order: &[usize]) {
    let mut taken = std::mem::take(items);
    *items = order
        .iter()
        .map(|&index| std::mem::take(&mut taken[index]))
        .collect();
}

impl TableDelegate for ResultGrid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len() + GUTTER
    }

    fn rows_count(&self, _: &App) -> usize {
        self.result.rows.len()
    }

    /// A wash across the row, for one the gutter has picked out. Painted here
    /// rather than per cell, so it reaches the gutter's own padding too and
    /// reads as one row rather than a strip of separately-tinted cells.
    ///
    /// A child rather than the row's own `bg`: the library wraps every row in
    /// its own `.hover()`, which it skips only for its single `selected_row`,
    /// not for the rest of a multi-row selection here. Anywhere else in that
    /// selection, its hover style replaces the row's background outright,
    /// turning the blue tint gray. A child paints above that background
    /// regardless of which one the library chose, so the tint survives.
    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let t = *theme(cx);
        // The library sizes every row `w_full` -- the table's whole width, not
        // the columns actually in it -- so its border and hover wash run on
        // past the last column to the edge of the screen over nothing. It
        // refines that with the style returned here, so a cap here wins.
        // A cap and not a width: each row scrolls its own cells sideways
        // within its bounds, so a row as wide as its columns has nothing to
        // scroll and the cells stay put while the header moves.
        // Not a mask painted over the margin: on a glass theme a second
        // `data_glass` stacks on the plane's own and the margin turns solid.
        let content_width = self.gutter_width()
            + self
                .columns
                .iter()
                .map(|column| f32::from(column.width))
                .sum::<f32>();
        let row = div()
            .id(("row", row_ix))
            .relative()
            .max_w(px(content_width));
        if !self.selected_rows.contains(&row_ix) {
            return row;
        }
        row.child(div().absolute().size_full().bg(t.selection))
    }

    /// Nothing, where the library leaves an empty strip: that strip is inside
    /// every row, so the row's selection wash, hover and hairline ran on past
    /// the last column over it.
    fn render_last_empty_col(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix.checked_sub(GUTTER) {
            Some(col) => self.columns[col].clone(),
            None => Column::new("row-number", "#")
                .width(px(self.gutter_width()))
                .resizable(false)
                .movable(false)
                .selectable(false)
                .fixed_left()
                .p_0(),
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        match col_ix.checked_sub(GUTTER) {
            Some(col) => self.data_th(col, window, cx).into_any_element(),
            None => {
                let faint = theme(cx).text_faint;
                gutter_cell()
                    .size_full()
                    // The library reserves a sort icon's room on the right of
                    // every header, which leaves this one off-centre unless
                    // the left side reserves the same.
                    .pl(px(layout::SPACE_SM)
                        + gpui_component::Size::Medium.table_cell_padding().right)
                    .text_size(px(layout::grid(layout::TEXT_SM)))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(faint)
                    .child("#")
                    .into_any_element()
            }
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        match col_ix.checked_sub(GUTTER) {
            Some(col) => self.data_td(row_ix, col, window, cx).into_any_element(),
            None => {
                let (faint, text, selected_bg) = {
                    let t = theme(cx);
                    (t.text_faint, t.text, t.selection)
                };
                let selected = self.selected_rows.contains(&row_ix);
                gutter_cell()
                    .id(("row-number", row_ix))
                    .size_full()
                    .when(selected, |cell| cell.bg(selected_bg))
                    .text_color(if selected { text } else { faint })
                    .child((row_ix + 1).to_string())
                    // Left click only: a right click still opens the cell
                    // menu the row's own cells offer, and a gutter with two
                    // different rules for the two buttons would be a gutter
                    // nobody could predict.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |table, event: &gpui::MouseDownEvent, window, cx| {
                            click_row(table, row_ix, event, window, cx);
                        }),
                    )
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .into_any_element()
            }
        }
    }

    /// The mouse's way to the writes a cell input cannot express. An input
    /// cannot be typed empty into a `NULL`, and neither the empty string nor
    /// `DEFAULT` can be typed into the other two (spec §3) -- so the gesture is
    /// this menu, which reaches the same actions the keystroke and the palette
    /// reach.
    ///
    /// Flat rather than nested under "Set Value": a submenu has to be an
    /// entity wired to its parent, and the delegate hook is handed a
    /// `Context<TableState>` that cannot build one.
    ///
    /// Nothing at all without an active cell; an empty menu is not opened. A
    /// cell that cannot be written still offers the copy, because copying is
    /// not a write. A mode too low withholds nothing: the entry is offered and
    /// the prompt is what answers it, the way the keystroke behaves.
    fn context_menu(
        &mut self,
        _: usize,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        // The right click that opened this pinned the cell on its way past
        // `render_td`, so the column the library never records is known here.
        let Some((row, col)) = self.active else {
            return menu;
        };
        let stages: Vec<(&str, Box<dyn gpui::Action>)> = match self.editable(row, col) {
            false => Vec::new(),
            true => [
                self.offers_null(col)
                    .then(|| ("Set Value to NULL", Box::new(crate::SetNull) as _)),
                self.offers_empty(col)
                    .then(|| ("Set Value to Empty", Box::new(crate::SetEmpty) as _)),
                self.offers_default(col)
                    .then(|| ("Set Value to Default", Box::new(crate::SetDefault) as _)),
            ]
            .into_iter()
            .flatten()
            .collect(),
        };

        // Built here rather than through `PopupMenu::submenu`, which wants the
        // menu's own context and a delegate is handed the table's. The
        // library wires the parent of a submenu added this way when it paints.
        let focus = self.focus.clone();
        let engine = self.engine;
        let rows_as = PopupMenu::build(window, cx, move |submenu, _, _| {
            RowsAs::ALL
                .into_iter()
                .filter(|kind| kind.offered_on(engine))
                .fold(
                    submenu.when_some(focus.clone(), PopupMenu::action_context),
                    |submenu, kind| {
                        submenu
                            .when(kind == RowsAs::Csv, PopupMenu::separator)
                            .when(kind == RowsAs::Json, PopupMenu::separator)
                            .menu(kind.label(), Box::new(crate::CopyRows { kind }))
                    },
                )
        });
        // "Copy Row" is redundant beside this once it reads as one row: the
        // submenu's own "Text" entry already puts the same TSV on the
        // clipboard.
        let rows_as_label = match self.selected_row_indices().len() {
            1 => "Copy Row As",
            _ => "Copy Rows As",
        };
        let menu = menu
            .when_some(self.focus.clone(), PopupMenu::action_context)
            .menu("Copy Cell", Box::new(crate::CopyCell))
            .item(PopupMenuItem::submenu(rows_as_label, rows_as))
            .when(!stages.is_empty(), PopupMenu::separator);
        stages
            .into_iter()
            .fold(menu, |menu, (label, action)| menu.menu(label, action))
    }
}

impl ResultGrid {
    fn data_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (muted, faint, text) = {
            let t = theme(cx);
            (t.text_muted, t.text_faint, t.text)
        };
        let key = self
            .sort
            .iter()
            .position(|(column, _)| *column == col_ix)
            .map(|position| (position, self.sort[position].1));

        let base = div()
            .id(("column-header", col_ix))
            .h_full()
            // Not `size_full`: the header cell clips, and the resize handle
            // lives at its trailing edge.
            .flex_1()
            .min_w_0()
            // A body cell's text starts a border's width in from its padding,
            // and a header name that did not would sit a pixel left of every
            // value under it.
            .pl(px(layout::SPACE_SM + 1.))
            .pr(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(layout::grid(layout::TEXT_SM)))
            .font_weight(gpui::FontWeight::MEDIUM)
            // Full strength whether or not this column is sorted. A header is
            // the only label for what is under it, and muted grey over a frosted
            // window is a row of names the user has to lean in to read; which
            // column the sort is on is already said by the arrow, in a way that
            // survives being glanced at.
            .text_color(text);

        base.children(self.key_icon(col_ix, ConnectionColor::Yellow.swatch()))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(self.columns[col_ix].name.clone()),
            )
            .children(
                self.result
                    .columns
                    .get(col_ix)
                    .and_then(|column| column.data_type.clone())
                    .map(|data_type| {
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(layout::grid(layout::TEXT_XS)))
                            .font_weight(gpui::FontWeight::NORMAL)
                            .text_color(faint)
                            .child(data_type.to_uppercase())
                    }),
            )
            .child(
                div()
                    .ml_auto()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .map(|control| match key {
                        Some((_, ascending)) => control.child(
                            icon(match ascending {
                                true => icon::SORT_UP,
                                false => icon::SORT_DOWN,
                            })
                            .size(px(layout::grid(12.)))
                            .text_color(text),
                        ),
                        // Faint rather than absent: a header that shows nothing
                        // until it is clicked does not read as clickable. And
                        // absent rather than faint where a click would do
                        // nothing, which is the same rule the other way round.
                        None => control.children(self.sortable.then(|| {
                            icon(icon::SORTABLE)
                                .size(px(layout::grid(12.)))
                                .text_color(faint)
                        })),
                    })
                    // Only worth saying which key this is when there is more
                    // than one of them.
                    .children(key.filter(|_| self.sort.len() > 1).map(|(position, _)| {
                        div()
                            .text_size(px(layout::grid(layout::TEXT_XS)))
                            .text_color(muted)
                            .child((position + 1).to_string())
                    })),
            )
            // The click goes to the workspace, which knows whether this view
            // is sorted by the server -- a change to the SQL and a re-run --
            // or in memory.
            .on_click(cx.listener(move |_, _, window, cx| {
                window.dispatch_action(Box::new(crate::SortColumn { column: col_ix }), cx);
            }))
    }

    fn data_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (text, faint, edited_bg, active_ring, divider, number, palette) = {
            let t = theme(cx);
            (
                t.text,
                t.text_faint,
                t.edited,
                t.accent,
                t.border,
                t.syntax_function,
                *t,
            )
        };
        let base = div()
            .id(("cell", row_ix * self.columns.len() + col_ix))
            .relative()
            .size_full()
            .px(px(layout::SPACE_SM))
            // A ring rather than a wash: the pending-edit wash is taken, and
            // the active cell has to be distinguishable while wearing it.
            // Unconditional width so the ring appearing costs the row no
            // reflow -- gpui lays a border out whether or not there is a
            // colour to paint it with.
            .border_1()
            .when(self.active == Some((row_ix, col_ix)), |cell| {
                cell.border_color(active_ring)
            })
            .flex()
            .items_center()
            .child(
                div()
                    .absolute()
                    // Out over the cell's own border, which insets every edge
                    // by a pixel: the header's hairline sits on the column edge
                    // and runs the height of the row.
                    .top(px(-1.))
                    .bottom(px(-1.))
                    .right(px(-1.))
                    .w(px(1.))
                    .bg(divider),
            )
            // The row menu is the library's, and it records the row a right
            // click landed on and never the column, so the cell is pinned here
            // the way the left click pins it. Focus with it: the menu dispatches
            // its action into whatever holds focus when it is confirmed.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |table, _, window, cx| {
                    let handle = table.focus_handle(cx);
                    handle.focus(window, cx);
                    let grid = table.delegate_mut();
                    grid.set_active(row_ix, col_ix);
                    grid.right_click_row(row_ix);
                    grid.focus = Some(handle);
                    grid.pointer_row = Some(row_ix);
                    table.set_selected_row(row_ix, cx);
                    // set_selected_row consumes the event; the row beneath
                    // still needs its own right click to record where the
                    // context menu opens.
                    cx.propagate();
                }),
            )
            // A plain cell click must establish the anchor before Shift is
            // pressed, exactly as a click on its row number does.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |table, event: &gpui::MouseDownEvent, window, cx| {
                    // A click on the cell being edited belongs to its input.
                    // `click_row` would focus the table and strand the input
                    // without the keyboard. Refocused explicitly because a
                    // click in the padding around the text misses the input's
                    // own hitbox, and `prevent_default` keeps the table's
                    // tracked focus from taking it back after this listener.
                    if let Some(editing) = table
                        .delegate()
                        .editing
                        .as_ref()
                        .filter(|editing| (editing.row, editing.col) == (row_ix, col_ix))
                    {
                        if let Some(input) = editing.input.clone() {
                            input.focus_handle(cx).focus(window, cx);
                        }
                        window.prevent_default();
                        return;
                    }
                    table.delegate_mut().set_active(row_ix, col_ix);
                    click_row(table, row_ix, event, window, cx);
                }),
            );

        if let Some(input) = self.editing_input(row_ix, col_ix, window, cx) {
            return base
                .bg(edited_bg)
                .gap(px(layout::SPACE_SM))
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&input)
                            // The cell is the frame; a second border and
                            // background inside one would read as a control in
                            // a hole.
                            .appearance(false)
                            .px_0()
                            // The cell is the frame, so take its height rather
                            // than the control's own `rems`-based one, which is
                            // sized for a standalone field and overflows the
                            // row.
                            .h_full()
                            .text_size(px(layout::grid(layout::TEXT_MD))),
                    ),
                )
                // The library's row click would reselect the row under the
                // caret, the same reason the read-only cell stops it.
                .on_click(|_, _, cx| cx.stop_propagation())
                // The input has focus, so both keystrokes arrive here on their
                // way out of it. Consumed rather than propagated: `escape`
                // otherwise reaches the workspace and moves focus to the editor.
                .on_action(cx.listener(
                    move |table, _: &gpui_component::input::Enter, window, cx| {
                        commit_from_input(table, window, cx);
                    },
                ))
                .on_action(cx.listener(
                    move |table, _: &gpui_component::input::Escape, window, cx| {
                        table.delegate_mut().cancel_edit();
                        table.focus_handle(cx).focus(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    },
                ))
                // The input hands `left` at its start and `right` at its end up
                // to its ancestors, where the table's own bindings for the same
                // keys would move the ring off the cell and drop the edit.
                // Swallowed here, so the caret never leaves the cell by arrow.
                .on_action(|_: &gpui_component::input::MoveLeft, _, cx| cx.stop_propagation())
                .on_action(|_: &gpui_component::input::MoveRight, _, cx| cx.stop_propagation())
                // Captured rather than bubbled: a single-line input swallows
                // `up` and `down` without doing anything, so they would never
                // reach a listener below it.
                .capture_action(cx.listener(
                    |table, _: &gpui_component::input::MoveUp, window, cx| {
                        commit_and_step(table, Step::Rows(-1), window, cx)
                    },
                ))
                .capture_action(cx.listener(
                    |table, _: &gpui_component::input::MoveDown, window, cx| {
                        commit_and_step(table, Step::Rows(1), window, cx)
                    },
                ))
                // `tab` reaches here the same way (the input is single-line, so
                // it never indents), and gets the spreadsheet meaning instead of
                // the table's: keep the value, then move along the row.
                .on_action(cx.listener(
                    |table, _: &gpui_component::input::IndentInline, window, cx| {
                        commit_and_step(table, Step::Cols(1), window, cx)
                    },
                ))
                .on_action(cx.listener(
                    |table, _: &gpui_component::input::OutdentInline, window, cx| {
                        commit_and_step(table, Step::Cols(-1), window, cx)
                    },
                ));
        }

        // A pending value is painted from the pending set; `result.rows`
        // stays exactly as fetched.
        let pending = self.pending_at(row_ix, col_ix);
        // Which word the italic branch below paints. A staged keyword has no
        // `shown`, so it falls into the same branch a fetched NULL does while
        // still wearing the edited wash — but it must say which keyword, or a
        // staged `DEFAULT` would read as a staged `NULL`.
        let keyword = match pending.map(|pending| &pending.value) {
            Some(NewValue::Default) => DEFAULT_LABEL,
            None if self.missing(row_ix, col_ix) => SharedString::default(),
            _ => NULL_LABEL,
        };
        let cell = match pending {
            Some(pending) => pending.shown.clone(),
            None => self.shown(row_ix, col_ix),
        };

        let follows_a_key = self.follows_a_key(col_ix)
            // A NULL references nothing, so there is nothing to follow it to.
            && self.cell(row_ix, col_ix).is_some();
        let group = follows_a_key
            .then(|| self.follow_group(col_ix).cloned())
            .flatten();

        base.overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_color(match (cell.is_some(), self.is_numeric_column(col_ix)) {
                (false, _) => faint,
                (true, true) => number,
                (true, false) => text,
            })
            // Italic so a NULL cannot be mistaken for the four-letter string.
            .when(cell.is_none(), |cell| cell.italic())
            .when(pending.is_some(), |cell| cell.bg(edited_bg))
            // The value is the child that gives way, beside an arrow or not.
            // Bare text in a row does not shrink and a flex container does not
            // ellipsise its own text, so a value as wide as its column -- every
            // cell of a column of 32-character keys -- either pushed the arrow
            // past the clipped edge or was cut with no ellipsis.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(cell.unwrap_or(keyword)),
            )
            // A NULL is referenced by nothing, so it has no arrow either.
            .children(
                self.cell(row_ix, col_ix)
                    .and_then(|_| self.reference_menu(col_ix))
                    .map(|_| {
                        icon_button(
                            ("references", row_ix * self.columns.len() + col_ix),
                            icon::REFERENCED_BY,
                            Tone::Quiet,
                            Control::Inline,
                            palette,
                        )
                        // Pinned on the way down, so the action the click
                        // dispatches finds this cell active and the table
                        // focused, the way the cell menu's right click does.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |table, event: &gpui::MouseDownEvent, window, cx| {
                                let handle = table.focus_handle(cx);
                                handle.focus(window, cx);
                                let grid = table.delegate_mut();
                                grid.set_active(row_ix, col_ix);
                                grid.focus = Some(handle);
                                grid.reference_anchor = Some(event.position);
                                cx.notify();
                            }),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(ShowReferences), cx);
                        })
                    }),
            )
            // The workspace owns the statement and the tabs and the grid owns
            // neither, so this leaves exactly as a header's sort click does.
            .children(group.map(|_| {
                div()
                    .id(("follow-key", row_ix * self.columns.len() + col_ix))
                    // A square at the trailing edge rather than a glyph
                    // trailing the text: somewhere to aim, in the same place in
                    // every row. It keeps its room while hidden and the value
                    // ellipsises around it, the way a header's name does around
                    // its sort control -- so nothing reflows under the pointer,
                    // and the arrow needs no ground of its own to stay legible.
                    .flex_shrink_0()
                    .h_full()
                    .aspect_square()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(faint)
                    .hover(|button| button.text_color(text))
                    .child(icon(icon::FOLLOW_KEY).size(px(layout::grid(14.))))
                    .on_click(cx.listener(move |table, _, window, cx| {
                        // The action follows the active cell, and this one is
                        // only hovered: without the move it would open the row
                        // the ring happens to be on instead of the row clicked.
                        table.delegate_mut().set_active(row_ix, col_ix);
                        // Or the cell underneath takes the click as a move of
                        // the ring it is already on.
                        cx.stop_propagation();
                        window.dispatch_action(Box::new(crate::FollowForeignKey), cx);
                    }))
            }))
            // Dispatched rather than editing the cell here, so the mouse and
            // the keystroke cannot drift -- and so a refusal (a mode below
            // Read-write, a key or computed column) reaches the same
            // explanation `EditCell` already gives from the keyboard.
            .on_double_click(cx.listener(move |table, _, window, cx| {
                table.delegate_mut().set_active(row_ix, col_ix);
                window.dispatch_action(Box::new(crate::EditCell), cx);
                cx.notify();
            }))
            // The library's row click would select a toggled-off row again.
            .on_click(|_, _, cx| cx.stop_propagation())
    }
}

fn click_row(
    table: &mut TableState<ResultGrid>,
    row: usize,
    event: &gpui::MouseDownEvent,
    window: &mut Window,
    cx: &mut Context<TableState<ResultGrid>>,
) {
    table.focus_handle(cx).focus(window, cx);
    table
        .delegate_mut()
        .click_row(row, event.modifiers.shift, event.modifiers.secondary());
    // Both states paint blue. Updating the library on mouse-up leaves its
    // old highlight behind, indefinitely if the button is released elsewhere.
    if table.delegate().selected_rows.contains(&row) {
        table.delegate_mut().pointer_row = Some(row);
        table.set_selected_row(row, cx);
    } else {
        table.clear_selection(cx);
    }
    // set_selected_row consumes the event; the cell still needs mouse-down
    // for its click and double-click handlers.
    cx.propagate();
}

pub(crate) fn new_grid(
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<TableState<ResultGrid>> {
    let grid = cx.new(|cx| {
        TableState::new(ResultGrid::empty(), window, cx)
            .col_movable(false)
            .col_resizable(true)
            .row_selectable(true)
            .col_selectable(true)
    });

    // The library's arrow keys move its own selection, which is a row or a
    // column and never a cell. Folded into the active cell here, they move the
    // ring instead -- so every grid is navigable by keyboard, and dbdelve needs
    // no arrow binding competing with the library's own actions.
    //
    // Hooked in the constructor because every relation tab builds its grid
    // through it: a subscription set up at one call site would leave the other
    // grid navigating an invisible selection.
    cx.subscribe(&grid, |_, table, event: &TableEvent, cx| match event {
        TableEvent::SelectRow(row) => {
            let row = *row;
            table.update(cx, |table, cx| {
                table.delegate_mut().select_row(row);
                cx.notify();
            });
        }
        TableEvent::ColumnWidthsChanged(widths) => {
            let widths = widths.get(GUTTER..).unwrap_or_default().to_vec();
            table.update(cx, |table, _| table.delegate_mut().set_widths(&widths));
        }
        TableEvent::SelectColumn(col) => {
            let Some(col) = col.checked_sub(GUTTER) else {
                return;
            };
            table.update(cx, |table, cx| {
                table.delegate_mut().select_col(col);
                cx.notify();
            });
        }
        _ => {}
    })
    .detach();

    grid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column as DbColumn;

    fn column(name: &str) -> DbColumn {
        DbColumn {
            name: name.into(),
            data_type: None,
        }
    }

    fn typed(name: &str, data_type: &str) -> DbColumn {
        DbColumn {
            name: name.into(),
            data_type: Some(data_type.into()),
        }
    }

    #[test]
    fn rows_streamed_in_before_their_types_are_rendered_once_the_types_arrive() {
        let point = "0101000000000000000000F03F000000000000F03F";
        let mut grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("n"), column("p")],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
        .with_engine(db::Engine::Postgres)
        .streaming(1);
        grid.append_rows(vec![vec![Some("1".into()), Some(point.into())]]);

        grid.set_column_types(vec![typed("n", "int4"), typed("p", "geometry")]);
        let rendered = db::render(db::unrendered(db::Engine::Postgres, grid.result()));
        grid.apply_rendered(rendered);

        assert!(grid.is_numeric_column(0));
        let shown = grid.result().rows[0][1].as_deref().unwrap_or_default();
        assert!(shown.starts_with("POINT"), "{shown}");
        // Rendered already, so a second pass finds nothing left to do.
        assert!(db::unrendered(db::Engine::Postgres, grid.result()).is_empty());
    }

    #[test]
    fn a_rendered_value_lands_only_where_the_cell_still_holds_what_was_rendered() {
        let mut grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("p")],
                rows: vec![vec![Some("moved".into())], vec![Some("0101".into())]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );
        grid.apply_rendered(vec![
            (0, 0, "0101".into(), "POINT(1 1)".into()),
            (1, 0, "0101".into(), "POINT(2 2)".into()),
        ]);
        assert_eq!(grid.result().rows[0][0].as_deref(), Some("moved"));
        assert_eq!(grid.result().rows[1][0].as_deref(), Some("POINT(2 2)"));
    }

    fn value(text: &str) -> NewValue {
        NewValue::Value(text.into())
    }

    #[test]
    fn a_sort_in_memory_follows_its_column_by_name_into_the_next_result() {
        // The re-run moved `b` to the front and dropped `c`: the key on `b`
        // still finds it, and the one on `c` orders nothing rather than some
        // other column that now sits where `c` was.
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![typed("b", "int4"), typed("a", "int4")],
                rows: vec![
                    vec![Some("1".into()), Some("9".into())],
                    vec![Some("2".into()), Some("8".into())],
                ],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
        .with_engine(db::Engine::Postgres)
        .with_client_sort(Some(&[
            SortKey::new("\"c\"", true),
            SortKey::new("\"b\"", false),
        ]));

        assert_eq!(grid.sort, vec![(0, false)]);
        assert_eq!(grid.cell(0, 0), Some("2"));
        assert!(grid.sortable);
    }

    /// A grid over one column of untyped cells, which is enough to order rows by.
    fn grid_of(values: &[Option<&str>]) -> ResultGrid {
        typed_column(None, values)
    }

    /// A grid over one column named `a`, typed `data_type`.
    fn typed_column(data_type: Option<&str>, values: &[Option<&str>]) -> ResultGrid {
        ResultGrid::new(
            QueryResult {
                columns: vec![DbColumn {
                    name: "a".into(),
                    data_type: data_type.map(str::to_string),
                }],
                rows: values
                    .iter()
                    .map(|value| vec![value.map(str::to_string)])
                    .collect(),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
    }

    /// Column 0's cells in the order `keys` puts the rows in.
    fn sorted<'a>(grid: &'a ResultGrid, keys: &[(usize, bool)]) -> Vec<Option<&'a str>> {
        grid.client_order(keys)
            .into_iter()
            .map(|row| grid.cell(row, 0))
            .collect()
    }

    #[test]
    fn a_number_column_sorts_by_value_and_a_text_column_by_its_text() {
        let values = [Some("10"), Some("9"), Some("-2.5"), Some("100")];
        let by_value = [Some("-2.5"), Some("9"), Some("10"), Some("100")];
        assert_eq!(
            sorted(&typed_column(Some("int4"), &values), &[(0, true)]),
            by_value
        );
        // What the server's own ORDER BY on a text column would say.
        assert_eq!(
            sorted(&typed_column(Some("text"), &values), &[(0, true)]),
            [Some("-2.5"), Some("10"), Some("100"), Some("9")]
        );
        // Nothing says what the column is, so a value that reads as a number is one.
        assert_eq!(sorted(&typed_column(None, &values), &[(0, true)]), by_value);
    }

    #[test]
    fn null_sorts_last_whichever_way_the_key_points() {
        let grid = typed_column(Some("int4"), &[None, Some("2"), Some("1")]);
        assert_eq!(sorted(&grid, &[(0, true)]), [Some("1"), Some("2"), None]);
        assert_eq!(sorted(&grid, &[(0, false)]), [Some("2"), Some("1"), None]);
    }

    #[test]
    fn text_sorts_without_regard_to_case_and_dates_sort_as_written() {
        let names = typed_column(
            Some("text"),
            &[Some("beta"), Some("Alpha"), Some("alpha2"), Some("Gamma")],
        );
        assert_eq!(
            sorted(&names, &[(0, true)]),
            [Some("Alpha"), Some("alpha2"), Some("beta"), Some("Gamma")]
        );

        let times = typed_column(
            Some("timestamptz"),
            &[
                Some("2024-10-02 09:00:00+00"),
                Some("2023-12-31 23:59:59+00"),
                Some("2024-10-02 10:00:00+00"),
            ],
        );
        assert_eq!(
            sorted(&times, &[(0, true)]),
            [
                Some("2023-12-31 23:59:59+00"),
                Some("2024-10-02 09:00:00+00"),
                Some("2024-10-02 10:00:00+00"),
            ]
        );
    }

    #[test]
    fn special_floats_and_mixed_fields_still_have_one_order() {
        // `sort_by` may panic on a comparison that is not a total order, and
        // NaN is where a naive float comparison stops being one.
        let floats = typed_column(
            Some("float8"),
            &[Some("NaN"), Some("1"), Some("-Infinity"), Some("Infinity")],
        );
        assert_eq!(
            sorted(&floats, &[(0, true)]),
            [Some("-Infinity"), Some("1"), Some("Infinity"), Some("NaN")]
        );

        // A document field: numbers by value first, then text, then the
        // field a document does not have. The string "10" is text.
        let field = ResultGrid::new(
            QueryResult {
                columns: vec![DbColumn {
                    name: "v".into(),
                    data_type: Some("mixed".into()),
                }],
                rows: vec![
                    vec![Some("b".into())],
                    vec![Some("10".into())],
                    vec![None],
                    vec![Some("9".into())],
                    vec![Some("10".into())],
                ],
                cell_types: vec![
                    vec!["string"],
                    vec!["int"],
                    vec![db::MISSING],
                    vec!["double"],
                    vec!["string"],
                ],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );
        assert_eq!(field.client_order(&[(0, true)]), [3, 1, 4, 0, 2]);
    }

    #[test]
    fn later_keys_break_earlier_ties_and_full_ties_keep_the_servers_order() {
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![typed("team", "text"), typed("score", "int4")],
                rows: [("b", "1"), ("a", "2"), ("b", "2"), ("a", "2")]
                    .iter()
                    .map(|(team, score)| vec![Some(team.to_string()), Some(score.to_string())])
                    .collect(),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );
        // Rows 1 and 3 tie on both keys, so they stay in the order they came.
        assert_eq!(grid.client_order(&[(0, true), (1, false)]), [1, 3, 2, 0]);
        assert_eq!(grid.client_order(&[]), [0, 1, 2, 3]);
    }

    /// A grid over `id, note, total` where `id` is the key, `note` is an alias
    /// for the real column `body`, and `total` is computed. One column of each
    /// kind that editing has to tell apart.
    fn editable_grid() -> ResultGrid {
        ResultGrid::new(
            QueryResult {
                columns: vec![column("id"), column("note"), column("total")],
                rows: vec![
                    vec![Some("7".into()), Some("first".into()), Some("1".into())],
                    vec![Some("8".into()), Some("second".into()), Some("2".into())],
                ],
                edit: Some(EditTarget {
                    schema: "public".into(),
                    table: "measurements".into(),
                    columns: vec![Some("id".into()), Some("body".into()), None],
                    keys: vec![0],
                }),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
    }

    /// A grid whose columns have been told what the relation's structure says
    /// about them: `note` is `NOT NULL`, `depth` is a number with a default.
    fn marked_grid() -> ResultGrid {
        let mut grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("id"), column("note"), typed("depth", "int4")],
                rows: vec![vec![
                    Some("7".into()),
                    Some("first".into()),
                    Some("1".into()),
                ]],
                edit: Some(EditTarget {
                    schema: "public".into(),
                    table: "measurements".into(),
                    columns: vec![Some("id".into()), Some("note".into()), Some("depth".into())],
                    keys: vec![0],
                }),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );
        grid.mark_columns(&["note".to_string()], &["depth".to_string()]);
        grid
    }

    /// The same shape as [`editable_grid`], but with the editable column left
    /// NULL by the server: the one row a staged NULL has to be told apart from.
    fn grid_with_a_null() -> ResultGrid {
        ResultGrid::new(
            QueryResult {
                columns: vec![column("id"), column("note")],
                rows: vec![vec![Some("7".into()), None]],
                edit: Some(EditTarget {
                    schema: "public".into(),
                    table: "measurements".into(),
                    columns: vec![Some("id".into()), Some("body".into())],
                    keys: vec![0],
                }),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
    }

    /// The same shape again, but the NULL is in the *key* column: the row the
    /// server left unnameable.
    fn grid_with_a_null_key() -> ResultGrid {
        ResultGrid::new(
            QueryResult {
                columns: vec![column("id"), column("note")],
                rows: vec![vec![None, Some("first".into())]],
                edit: Some(EditTarget {
                    schema: "public".into(),
                    table: "measurements".into(),
                    columns: vec![Some("id".into()), Some("body".into())],
                    keys: vec![0],
                }),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
    }

    #[test]
    fn a_row_offers_its_whole_key_or_nothing_at_all() {
        // The same condition that makes a cell editable, because it is the same
        // question: can dbdelve name this row.
        let grid = editable_grid();
        assert_eq!(
            grid.row_key(1),
            Some((
                "public".to_string(),
                "measurements".to_string(),
                vec![("id".to_string(), "8".to_string())]
            ))
        );
        // A row index can outlive the rows it was taken from.
        assert_eq!(grid.row_key(9), None);
        // And a result dbdelve cannot trace to one table has no key anywhere.
        assert_eq!(grid_of(&[Some("x")]).row_key(0), None);
    }

    #[test]
    fn a_row_whose_key_the_server_left_null_cannot_be_named() {
        // `=` does not find a NULL, so a predicate built from one matches
        // nothing -- and a delete that matches nothing is not the delete the
        // confirmation described.
        assert_eq!(grid_with_a_null_key().row_key(0), None);
        // The same row is nameable when it is a non-key column that is NULL.
        assert!(grid_with_a_null().row_key(0).is_some());
    }

    #[test]
    fn a_fetched_cell_paints_grouped_and_flattened_but_reads_as_fetched() {
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![typed("n", "int8"), column("ddl")],
                rows: vec![vec![
                    Some("1234567".into()),
                    Some("CREATE\n  VIEW v".into()),
                ]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        assert_eq!(grid.shown(0, 0).as_deref(), Some("1,234,567"));
        assert_eq!(grid.shown(0, 1).as_deref(), Some("CREATE VIEW v"));
        assert_eq!(grid.cell(0, 0), Some("1234567"));
        assert_eq!(grid.cell(0, 1), Some("CREATE\n  VIEW v"));
    }

    #[test]
    fn a_cell_is_clipped_without_walking_a_huge_value_to_its_end() {
        // Both run on every frame a cell is on screen, so neither may cost the
        // size of the value: a blank tail after an early break stops at the scan
        // limit, and a number too long to show is not grouped (which copies it).
        let blank_tail = format!("x\n{}", " ".repeat(2_000_000));
        assert_eq!(clip(&blank_tail), "x…");

        let digits = "9".repeat(CELL_DISPLAY_LIMIT * 4);
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![typed("n", "numeric")],
                rows: vec![vec![Some(digits.clone())]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );
        let shown = grid.shown(0, 0).unwrap();
        assert!(!shown.contains(','));
        assert_eq!(shown.as_ref(), clip(&digits));
    }

    #[test]
    fn digits_are_grouped_in_threes_and_only_when_the_value_is_a_plain_number() {
        for (value, shown) in [
            ("1723858791", "1,723,858,791"),
            ("-1234.5678", "-1,234.5678"),
            ("+1000", "+1,000"),
            ("999", "999"),
            ("12", "12"),
            ("1e10", "1e10"),
            ("$1234", "$1234"),
            ("1234abc", "1234abc"),
            ("", ""),
            (".5", ".5"),
        ] {
            assert_eq!(grouped_digits(value), shown, "{value}");
        }
    }

    #[test]
    fn pending_edits_are_walked_in_reading_order_and_wrap() {
        let mut grid = editable_grid();
        grid.result.edit.as_mut().unwrap().columns[2] = Some("total".into());
        assert_eq!(grid.pending_beside(false), None);

        assert!(grid.set_pending(1, 1, value("later")));
        assert!(grid.set_pending(0, 2, value("5")));
        assert!(grid.set_pending(0, 1, value("earlier")));
        assert_eq!(grid.pending_count(), 3);

        let walk = |grid: &mut ResultGrid, back| {
            let to = grid.pending_beside(back).unwrap();
            grid.set_active(to.0, to.1);
            to
        };
        assert_eq!(walk(&mut grid, false), (0, 1));
        assert_eq!(walk(&mut grid, false), (0, 2));
        assert_eq!(walk(&mut grid, false), (1, 1));
        assert_eq!(walk(&mut grid, false), (0, 1));
        assert_eq!(walk(&mut grid, true), (1, 1));
        assert_eq!(walk(&mut grid, true), (0, 2));

        // With no ring yet, back starts from the bottom.
        grid.active = None;
        assert_eq!(grid.pending_beside(true), Some((1, 1)));

        // From a cell that holds no edit, each way lands on its neighbour.
        grid.set_active(1, 0);
        assert_eq!(grid.pending_beside(false), Some((1, 1)));
        assert_eq!(grid.pending_beside(true), Some((0, 2)));
    }

    #[test]
    fn a_pending_edit_leaves_the_fetched_rows_alone() {
        // The grid shows changed-against-server by holding both. Writing the
        // edit into `result.rows` would lose the server's value for good.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, value("changed")));

        assert_eq!(grid.result.rows[0][1].as_deref(), Some("first"));
        assert_eq!(grid.shown(0, 1).as_deref(), Some("first"));
        assert_eq!(grid.cell(0, 1), Some("first"));
        assert!(grid.has_pending());
    }

    #[test]
    fn several_edits_on_one_row_become_one_update() {
        // One statement per row, not per cell: two `UPDATE`s against the same
        // key would be two round trips writing over each other's work.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, value("once")));
        assert!(grid.set_pending(0, 1, value("twice")));

        let updates = grid.pending_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].schema, "public");
        assert_eq!(updates[0].table, "measurements");
        // The alias is resolved here, so the caller generates SQL against the
        // column the table actually has.
        assert_eq!(updates[0].sets, vec![("body".to_string(), value("twice"))]);
    }

    #[test]
    fn two_edited_rows_become_two_updates() {
        let mut grid = editable_grid();
        assert!(grid.set_pending(1, 1, value("later")));
        assert!(grid.set_pending(0, 1, value("earlier")));

        let updates = grid.pending_updates();
        assert_eq!(updates.len(), 2);
        // In the order they were edited, so the batch reads the way it was made.
        assert_eq!(updates[0].keys, vec![("id".to_string(), "8".to_string())]);
        assert_eq!(updates[1].keys, vec![("id".to_string(), "7".to_string())]);
    }

    #[test]
    fn a_grid_survives_the_round_trip_through_a_snapshot() {
        // What reopening a profile shows. Every field here is one a person can
        // see is missing: a column that came back narrow, the sort arrows gone,
        // the ring on another cell.
        let mut grid = editable_grid();
        grid.sort = vec![(2, false), (0, true)];
        grid.set_widths(&[px(200.), px(190.), px(240.)]);
        grid.set_active(1, 2);
        // An edit nobody applied stays with the session that typed it.
        assert!(grid.set_pending(0, 1, value("changed")));

        let restored = ResultGrid::restored(&grid.stored(), Mode::ReadWrite);

        assert_eq!(
            restored
                .columns()
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "note", "total"]
        );
        assert_eq!(
            restored
                .columns
                .iter()
                .map(|column| column.width)
                .collect::<Vec<_>>(),
            [px(200.), px(190.), px(240.)]
        );
        assert_eq!(restored.result.rows, grid.result.rows);
        assert_eq!(restored.sort, vec![(2, false), (0, true)]);
        assert_eq!(restored.active, Some((1, 2)));
        assert!(!restored.has_pending());
        // The rows the cache holds are the rows the status bar counts.
        assert_eq!(grid.stored().total_rows, 2);
    }

    #[test]
    fn a_selection_carries_onto_rows_that_are_still_there_and_no_further() {
        let grid = |rows: usize| {
            ResultGrid::new(
                QueryResult {
                    columns: vec![db::Column {
                        name: "id".into(),
                        data_type: None,
                    }],
                    rows: (0..rows).map(|row| vec![Some(row.to_string())]).collect(),
                    ..QueryResult::default()
                },
                Mode::ReadWrite,
            )
        };
        let mut streaming = grid(3);
        streaming.click_row(1, false, false);
        streaming.click_row(2, true, false);
        streaming.set_active(2, 0);

        let mut landed = grid(5);
        assert!(landed.select(streaming.selection()));
        assert_eq!(landed.active(), Some((2, 0)));
        assert_eq!(landed.selected_rows, [1, 2].into());
        assert_eq!(landed.row_selection_anchor, Some(1));

        let mut shorter = grid(2);
        assert!(!shorter.select(streaming.selection()));
        assert_eq!(shorter.active(), None);
        assert!(shorter.selected_rows.is_empty());
        assert!(!grid(5).select(grid(3).selection()), "nothing was picked");
    }

    #[test]
    fn a_stopped_result_is_still_stopped_once_restored() {
        for stopped in [db::Stopped::AtLimit, db::Stopped::Cancelled] {
            let grid = ResultGrid::new(
                QueryResult {
                    columns: vec![db::Column {
                        name: "id".into(),
                        data_type: None,
                    }],
                    rows: vec![vec![Some("1".into())], vec![Some("2".into())]],
                    stopped: Some(stopped),
                    ..QueryResult::default()
                },
                Mode::ReadWrite,
            );
            let restored = ResultGrid::restored(&grid.stored(), Mode::ReadWrite);
            assert_eq!(restored.result().stopped, Some(stopped));
        }
    }

    #[test]
    fn the_inspector_reads_each_cells_own_type_and_knows_missing_from_null() {
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![
                    DbColumn {
                        name: "email".into(),
                        data_type: Some("mixed".into()),
                    },
                    column("phone"),
                ],
                rows: vec![vec![None, None]],
                cell_types: vec![vec![db::MISSING, "null"]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );
        let fields = grid.fields(0);
        assert!(fields[0].missing && !fields[1].missing);
        assert_eq!(fields[0].data_type.as_deref(), Some(db::MISSING));
        assert_eq!(fields[1].data_type.as_deref(), Some("null"));
    }

    #[test]
    fn a_missing_field_is_not_a_null_and_stays_one_across_a_snapshot() {
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("email"), column("phone")],
                rows: vec![vec![None, None]],
                cell_types: vec![vec![db::MISSING, "null"]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );
        assert!(grid.missing(0, 0));
        assert!(!grid.missing(0, 1));

        let restored = ResultGrid::restored(&grid.stored(), Mode::ReadWrite);
        assert!(restored.missing(0, 0));
        assert!(!restored.missing(0, 1));

        let mut unknown = grid.stored();
        unknown.cell_types[0][1] = "a type from elsewhere".into();
        assert!(
            ResultGrid::restored(&unknown, Mode::ReadWrite)
                .result
                .cell_types
                .is_empty()
        );
    }

    #[test]
    fn a_typed_cell_carries_its_own_type_to_the_write() {
        let mut grid = ResultGrid::new(
            QueryResult {
                columns: vec![typed("_id", "mixed"), typed("value", "mixed")],
                rows: vec![
                    vec![Some("ObjectId('65a4f1c0ffffffffffffffff')".into()), None],
                    vec![Some("2".into()), Some("BinData(0, 'AP8=')".into())],
                    vec![Some("3".into()), Some("42".into())],
                ],
                cell_types: vec![
                    vec!["objectId", db::MISSING],
                    vec!["int", "binData"],
                    vec!["int", "long"],
                ],
                edit: Some(EditTarget {
                    schema: "dbdelve_dev".into(),
                    table: "mixed_shapes".into(),
                    columns: vec![Some("_id".into()), Some("value".into())],
                    keys: vec![0],
                }),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
        .with_engine(db::Engine::MongoDb);

        // A binary cell is read-only even where its column is not all binary.
        assert!(!grid.editable(1, 1));
        assert!(grid.editable(2, 1));
        // Nulling a field the document does not have adds it.
        assert!(grid.set_pending(0, 1, NewValue::Null));
        assert!(grid.set_pending(2, 1, NewValue::Value("43".into())));
        let types: Vec<Vec<(String, String)>> = grid
            .pending_updates()
            .into_iter()
            .map(|row| row.types)
            .collect();
        assert_eq!(
            types,
            [
                [("_id", "objectId"), ("value", db::MISSING)],
                [("_id", "int"), ("value", "long")],
            ]
            .map(|row| row
                .map(|(name, alias)| (name.to_string(), alias.to_string()))
                .to_vec())
        );
    }

    #[test]
    fn a_capped_snapshot_keeps_the_result_size_across_a_re_save() {
        // Restore, then quit without re-running: the snapshot is written back
        // from a grid holding `GRID_ROW_CAP` rows, and recomputing the count
        // from those would collapse the real size to the cap for good.
        let restored = ResultGrid::restored(
            &StoredGrid {
                columns: vec!["n".into()],
                rows: (0..GRID_ROW_CAP)
                    .map(|n| vec![Some(n.to_string())])
                    .collect(),
                total_rows: 20_000,
                stopped: None,
                sort: Vec::new(),
                order_by: Vec::new(),
                client_sort: None,
                widths: Vec::new(),
                active: None,
                last_query: None,
                limit: None,
                filter: String::new(),
                showing_structure: false,
                captured: 1_700_000_000,
                edit: None,
                data_types: Vec::new(),
                cell_types: Vec::new(),
            },
            Mode::ReadWrite,
        );

        assert_eq!(restored.total_rows(), 20_000);
        let written = restored.stored();
        assert_eq!(written.total_rows, 20_000);
        assert_eq!(written.rows.len(), GRID_ROW_CAP);
        // And again, however many times the profile is reopened.
        assert_eq!(
            ResultGrid::restored(&written, Mode::ReadWrite)
                .stored()
                .total_rows,
            20_000
        );
    }

    #[test]
    fn a_result_larger_than_the_cap_is_capped_once_by_the_grid() {
        // `write_grid` caps too, but only as a backstop: cloning the whole row
        // vector to keep the front of it is what this avoids.
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("n")],
                rows: (0..GRID_ROW_CAP + 10)
                    .map(|n| vec![Some(n.to_string())])
                    .collect(),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        let written = grid.stored();
        assert_eq!(written.rows.len(), GRID_ROW_CAP);
        assert_eq!(written.total_rows, GRID_ROW_CAP + 10);
    }

    #[test]
    fn a_snapshots_active_cell_is_dropped_when_its_row_did_not_fit() {
        // `write_grid` caps the rows, so the cell that was active can be past
        // the end of what comes back -- and a ring around nothing is worse
        // than none.
        let grid = ResultGrid::restored(
            &StoredGrid {
                active: Some((9_000, 0)),
                ..editable_grid().stored()
            },
            Mode::ReadWrite,
        );

        assert_eq!(grid.active, None);
    }

    #[test]
    fn a_restored_grid_edits_nothing_until_its_stale_rows_are_confirmed() {
        let mut grid = ResultGrid::restored(&editable_grid().stored(), Mode::ReadWrite);
        assert!(
            grid.editable(0, 1),
            "the edit target came back with the rows"
        );
        assert!(grid.unconfirmed());
        assert!(!grid.begin_edit(0, 1));
        assert!(!grid.stage(0, 1, NewValue::Null));
        assert!(!grid.has_pending());

        grid.confirm_stale();
        assert!(grid.begin_edit(0, 1));
        assert!(grid.stage(0, 1, NewValue::Null));
        assert_eq!(
            grid.pending_updates()[0].keys,
            vec![("id".to_string(), "7".to_string())]
        );
    }

    #[test]
    fn a_restored_binary_column_stays_read_only() {
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("id"), typed("payload", "bytea")],
                rows: vec![vec![Some("7".into()), Some("\\xab".into())]],
                edit: Some(EditTarget {
                    schema: "public".into(),
                    table: "measurements".into(),
                    columns: vec![Some("id".into()), Some("payload".into())],
                    keys: vec![0],
                }),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );
        let mut restored = ResultGrid::restored(&grid.stored(), Mode::ReadWrite);
        restored.confirm_stale();

        assert!(!restored.editable(0, 1));
        assert!(!restored.begin_edit(0, 1));
    }

    #[test]
    fn a_snapshot_from_before_edit_targets_restores_read_only() {
        let older = r#"{"columns":["id"],"rows":[["1"]],"total_rows":1,"captured":1700000000}"#;
        let mut grid = ResultGrid::restored(
            &serde_json::from_str(older).expect("an older grid must decode"),
            Mode::ReadWrite,
        );

        // Nothing to confirm, and nothing to edit either way.
        assert!(!grid.unconfirmed());
        assert!(!grid.editable(0, 0));
        assert!(!grid.begin_edit(0, 0));
        assert_eq!(grid.row_key(0), None);
    }

    #[test]
    fn a_key_travels_as_the_value_the_server_sent() {
        // The `WHERE` names the row the server holds. Taking a key value from
        // the pending set would build a predicate that matches nothing.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, value("changed")));

        let updates = grid.pending_updates();
        assert_eq!(updates[0].keys, vec![("id".to_string(), "7".to_string())]);
        assert_eq!(
            updates[0].sets,
            vec![("body".to_string(), value("changed"))]
        );
    }

    #[test]
    fn a_row_dbdelve_cannot_name_produces_no_statement() {
        // A NULL key value leaves no predicate to write, and a row updated by
        // guesswork is the failure this whole feature is built to avoid.
        let mut grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("id"), column("note")],
                rows: vec![vec![None, Some("orphan".into())]],
                edit: Some(EditTarget {
                    schema: "public".into(),
                    table: "measurements".into(),
                    columns: vec![Some("id".into()), Some("body".into())],
                    keys: vec![0],
                }),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        assert!(grid.set_pending(0, 1, value("changed")));
        assert!(grid.pending_updates().is_empty());
    }

    #[test]
    fn a_read_only_grid_edits_nothing() {
        let mut grid = editable_grid();
        assert!(grid.editable(0, 1), "`note` is the editable column");

        grid.set_mode(Mode::ReadOnly);
        // The cell still opens -- that is how a value is selected and copied
        // out of one -- and refuses at the write, which is what raises the
        // mode prompt.
        assert!(grid.begin_edit(0, 1));
        assert!(!grid.set_pending(0, 1, value("x")));
        assert!(grid.pending_updates().is_empty());
        // Recording nothing is not a write, so it is not asked to be one.
        assert!(grid.set_pending(0, 1, value("first")));
        grid.cancel_edit();

        grid.set_mode(Mode::ReadWrite);
        assert!(grid.set_pending(0, 1, value("x")));
        assert!(
            !grid.pending_updates().is_empty(),
            "raising the mode writes"
        );
        grid.discard_pending();

        // Not a mode question: a key column stays uneditable at every mode.
        grid.set_mode(Mode::Full);
        assert!(!grid.editable(0, 0));
    }

    #[test]
    fn a_primary_key_column_cannot_be_edited() {
        // `SET id = new WHERE id = old` is the one edit whose result cannot be
        // re-verified afterwards -- spec §3.
        let mut grid = editable_grid();

        assert!(!grid.editable(0, 0));
        assert!(!grid.begin_edit(0, 0));
        assert!(!grid.set_pending(0, 0, value("99")));
        assert!(!grid.has_pending());
    }

    #[test]
    fn a_computed_column_cannot_be_edited() {
        // There is no column behind it to write to.
        let mut grid = editable_grid();

        assert!(!grid.editable(0, 2));
        assert!(!grid.set_pending(0, 2, value("99")));
        // Nor does a column past the end of the result set become editable.
        assert!(!grid.set_pending(0, 9, value("99")));
        // Nor a row past the end of it.
        assert!(!grid.set_pending(9, 1, value("99")));
        assert!(!grid.has_pending());
    }

    #[test]
    fn a_binary_column_cannot_be_edited() {
        // The grid paints a blob as the engine's own literal, and every value
        // written back goes through `quote_literal`, which would store the
        // literal as the text it looks like. Each engine's spelling of the
        // type, since one predicate answers for all three.
        for data_type in ["bytea", "blob", "longblob", "varbinary(16)", "BLOB"] {
            let mut grid = ResultGrid::new(
                QueryResult {
                    columns: vec![
                        column("id"),
                        DbColumn {
                            name: "payload".into(),
                            data_type: Some(data_type.into()),
                        },
                    ],
                    rows: vec![vec![Some("7".into()), Some("x'AB'".into())]],
                    edit: Some(EditTarget {
                        schema: "public".into(),
                        table: "measurements".into(),
                        columns: vec![Some("id".into()), Some("payload".into())],
                        keys: vec![0],
                    }),
                    ..QueryResult::default()
                },
                Mode::ReadWrite,
            );

            assert!(!grid.editable(0, 1), "{data_type}");
            assert!(!grid.begin_edit(0, 1), "{data_type}");
            assert!(!grid.set_pending(0, 1, value("x'CD'")), "{data_type}");
            assert!(!grid.has_pending(), "{data_type}");
        }
    }

    #[test]
    fn an_image_column_is_binary_only_on_sql_server() {
        // A SQLite column declared `image` holds whatever it is given, and a
        // Postgres domain may be called anything.
        let grid = |engine: db::Engine| {
            ResultGrid::new(
                QueryResult {
                    columns: vec![
                        column("id"),
                        DbColumn {
                            name: "picture".into(),
                            data_type: Some("image".into()),
                        },
                    ],
                    rows: vec![vec![Some("7".into()), Some("cat.png".into())]],
                    edit: Some(EditTarget {
                        schema: "main".into(),
                        table: "pets".into(),
                        columns: vec![Some("id".into()), Some("picture".into())],
                        keys: vec![0],
                    }),
                    ..QueryResult::default()
                },
                Mode::ReadWrite,
            )
            .with_engine(engine)
        };
        assert!(grid(db::Engine::Sqlite).editable(0, 1));
        assert!(grid(db::Engine::Postgres).editable(0, 1));
        assert!(!grid(db::Engine::SqlServer).editable(0, 1));
    }

    #[test]
    fn a_value_that_did_not_change_records_nothing() {
        // An input seeds itself with the fetched value, so opening an edit and
        // pressing Enter arrives here with that value: two keystrokes must not
        // become a write.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, value("first")));
        assert!(!grid.has_pending());

        // Typed back to what the server sent, an edit already recorded goes.
        assert!(grid.set_pending(0, 1, value("changed")));
        assert!(grid.has_pending());
        assert!(grid.set_pending(0, 1, value("first")));
        assert!(!grid.has_pending());

        // Emptying a cell that holds text is still a deliberate edit.
        assert!(grid.set_pending(0, 1, value("")));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), value(""))]
        );
    }

    #[test]
    fn a_staged_null_is_an_absence_and_not_the_empty_string() {
        let mut grid = editable_grid();

        assert!(grid.set_pending(0, 1, NewValue::Null));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), NewValue::Null)]
        );

        assert!(grid.set_pending(0, 1, value("")));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), value(""))]
        );

        // It paints the word a fetched NULL paints: the absent `shown` is what
        // sends it down the cell's own italic branch.
        assert!(grid.set_pending(0, 1, NewValue::Null));
        assert!(grid.pending_at(0, 1).unwrap().shown.is_none());
    }

    #[test]
    fn one_gesture_stages_a_null_and_closes_the_input_over_it() {
        // Both ways of asking end here: the action on the active cell, and the
        // editor's own affordance, which dispatches that same action rather
        // than doing this a second time.
        let mut grid = editable_grid();

        assert!(grid.begin_edit(0, 1));
        assert!(grid.stage(0, 1, NewValue::Null));
        assert!(
            grid.editing.is_none(),
            "an input left open over a nulled cell would commit its text back"
        );
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), NewValue::Null)]
        );

        // A key column is no more nullable than it is editable, and the same
        // predicate refuses both.
        assert!(!grid.stage(0, 0, NewValue::Null));
        assert!(!grid.stage(0, 2, NewValue::Null));
    }

    #[test]
    fn a_staged_default_reaches_the_update_as_the_keyword() {
        // The one value dbdelve cannot write as a literal: what the column's
        // default resolves to is the server's answer, so the statement has to
        // carry the word and let the server give it.
        let mut grid = editable_grid();
        assert!(grid.stage(0, 1, NewValue::Default));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), NewValue::Default)]
        );

        let batch =
            crate::sql::update_batch(db::Engine::Postgres, &grid.pending_updates()).unwrap();
        assert_eq!(
            batch,
            r#"UPDATE "public"."measurements" SET "body" = DEFAULT WHERE "id" = '7';"#
        );
        // The gate has to take it, the way it takes `SET x = NULL`. If the
        // grammar ever stops reading the keyword as part of an `update`, the
        // menu entry becomes a statement dbdelve refuses to run.
        assert!(
            crate::sql::is_generated_write(&batch),
            "{batch} was refused"
        );

        // Painted as its own word: `shown` is absent, which is what sends both
        // keywords down the cell's italic branch, and only the value says which.
        assert!(grid.pending_at(0, 1).unwrap().shown.is_none());
    }

    #[test]
    fn staging_a_default_is_never_the_no_op_an_unchanged_value_is() {
        // Every other staged value can equal what the server sent. This one
        // cannot be compared against anything, so it always records.
        let mut grid = grid_with_a_null();
        assert!(grid.stage(0, 1, NewValue::Default));
        assert!(grid.has_pending());
    }

    #[test]
    fn the_cell_menu_hides_an_entry_the_column_would_only_refuse() {
        let grid = marked_grid();

        // Declared NOT NULL, and an empty string is a value it can hold.
        assert!(!grid.offers_null(1));
        assert!(grid.offers_empty(1));
        assert!(!grid.offers_default(1));

        // A number takes a NULL and a default, and never the empty string.
        assert!(grid.offers_null(2));
        assert!(!grid.offers_empty(2));
        assert!(grid.offers_default(2));
    }

    #[test]
    fn a_column_nothing_has_been_said_about_offers_everything_but_the_default() {
        // Every column of a query tab: there is no one relation behind the
        // result, so there is no structure to mark it from. A NULL or an empty
        // string the column refuses comes back as the server's own error, which
        // is a truthful answer; `DEFAULT` without a default is a keyword the
        // statement could not carry.
        let grid = editable_grid();

        assert!(grid.offers_null(1));
        assert!(grid.offers_empty(1));
        assert!(!grid.offers_default(1));
    }

    #[test]
    fn the_empty_string_over_a_null_is_an_edit() {
        // It was not, while the two were the same three characters of SQL.
        // What now keeps an untouched input on a NULL from writing an empty
        // string over it is the editor -- `begin_edit` is a deliberate gesture
        // and `cancel_edit` records nothing -- not `set_pending`, which can no
        // longer tell a typed empty string from a seeded one and must not try.
        let mut grid = grid_with_a_null();

        assert!(grid.set_pending(0, 1, value("")));
        assert_eq!(
            grid.pending_updates()[0].sets,
            vec![("body".to_string(), value(""))]
        );
    }

    #[test]
    fn nulling_a_cell_the_server_already_left_null_records_nothing() {
        // Same reason an untouched value records nothing: this is not an edit.
        let mut grid = grid_with_a_null();

        assert!(grid.set_pending(0, 1, NewValue::Null));
        assert!(!grid.has_pending());
        assert!(grid.pending_updates().is_empty());
    }

    #[test]
    fn nothing_is_editable_without_an_edit_target() {
        // A join, an aggregate, or a select that dropped the key: `db` says the
        // rows cannot be addressed, and the grid stays read-only.
        let mut grid = grid_of(&[Some("x")]);

        assert!(!grid.editable(0, 0));
        assert!(!grid.begin_edit(0, 0));
        assert!(!grid.set_pending(0, 0, value("y")));
        assert!(grid.pending_updates().is_empty());
    }

    #[test]
    fn discarding_leaves_the_grid_as_fetched() {
        // Discarding is dropping a collection, which is the whole reason the
        // fetched rows are never written.
        let mut grid = editable_grid();
        grid.set_pending(0, 1, value("changed"));
        grid.begin_edit(1, 1);

        grid.discard_pending();

        assert!(!grid.has_pending());
        assert!(grid.editing.is_none());
        assert!(grid.pending_updates().is_empty());
        assert_eq!(grid.cell(0, 1), Some("first"));
    }

    #[test]
    fn a_long_pending_value_is_clipped_for_the_column_but_not_for_the_update() {
        // The same split as `shown` against `cell`: the column paints what
        // fits, the statement carries the value.
        let long = "x".repeat(CELL_DISPLAY_LIMIT * 2);
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, value(&long)));

        let pending = grid.pending_at(0, 1).unwrap();
        assert_eq!(pending.value, value(&long));
        assert_eq!(
            pending.shown.as_ref().unwrap().chars().count(),
            CELL_DISPLAY_LIMIT + 1
        );
        assert_eq!(grid.pending_updates()[0].sets[0].1, value(&long));
    }

    #[test]
    fn a_pending_numeric_edit_is_shown_grouped_but_staged_raw() {
        // `shown` groups a fetched number's digits; a pending edit over one
        // must read the same way or it stands out from the rows around it --
        // but what the `UPDATE` carries is the value exactly as typed.
        let mut grid = marked_grid();
        assert!(grid.set_pending(0, 2, value("7654321")));

        let pending = grid.pending_at(0, 2).unwrap();
        assert_eq!(pending.shown.as_deref(), Some("7,654,321"));
        assert_eq!(pending.value, value("7654321"));
        assert_eq!(grid.pending_updates()[0].sets[0].1, value("7654321"));
    }

    #[test]
    fn the_active_cell_is_scrolled_back_only_when_the_grid_is_relaid_out_at_a_new_width() {
        // Three columns of 180: the active one spans 360..540.
        let mut grid = editable_grid();
        assert_eq!(grid.relaid_out(px(800.), px(0.)), None);
        grid.set_active(0, 2);
        assert_eq!(grid.relaid_out(px(800.), px(0.)), None);
        assert_eq!(grid.relaid_out(px(500.), px(0.)), Some(2));
        assert_eq!(grid.relaid_out(px(500.), px(0.)), None);
        // Off the right edge of the old 300 viewport, then off its left edge
        // once scrolled 540 along: scrolled away from, so left where it is.
        assert_eq!(grid.relaid_out(px(300.), px(0.)), Some(2));
        assert_eq!(grid.relaid_out(px(250.), px(0.)), None);
        assert_eq!(grid.relaid_out(px(300.), px(-540.)), None);
    }

    #[test]
    fn a_clicked_cell_is_remembered_as_the_active_one() {
        // gpui-component has no cell selection -- `set_selected_row` and
        // `set_selected_col` are mutually exclusive modes, not two halves of a
        // coordinate -- so if the grid forgets this, `Enter` has no target.
        let mut grid = editable_grid();
        assert!(grid.active().is_none());

        grid.set_active(1, 1);
        assert_eq!(grid.active(), Some((1, 1)));
        grid.set_active(0, 2);
        assert_eq!(grid.active(), Some((0, 2)));
    }

    #[test]
    fn enter_opens_an_input_on_the_active_cell_and_refuses_where_it_must() {
        // The two halves of the keystroke: the active cell is what `Enter`
        // acts on, and being active does not make an unwritable cell writable.
        let mut grid = editable_grid();

        grid.set_active(1, 1);
        let (row, col) = grid.active().unwrap();
        assert!(grid.begin_edit(row, col));
        assert!(grid.editing.is_some());

        grid.cancel_edit();
        grid.set_active(1, 0);
        let (row, col) = grid.active().unwrap();
        assert!(!grid.begin_edit(row, col));
        assert!(grid.editing.is_none());
    }

    #[test]
    fn an_input_open_elsewhere_closes_when_another_cell_becomes_active() {
        // Two cells claiming the keyboard -- one holding a focused input, the
        // other wearing the ring `Enter` follows -- is a grid nobody can read.
        let mut grid = editable_grid();
        grid.set_active(0, 1);
        assert!(grid.begin_edit(0, 1));

        grid.set_active(1, 1);

        assert!(grid.editing.is_none());
    }

    #[test]
    fn a_double_click_opens_the_editor_when_the_cell_allows_one() {
        // GPUI's click plumbing needs a window this module does not build, so
        // this drives `begin_edit`, the function the listener calls, rather
        // than the listener itself. Copy-on-double-click is gone -- `cmd+c`
        // covers it -- so `begin_edit`'s answer is the whole outcome: an
        // editable cell opens, the primary key column (read-only) stays closed.
        let mut grid = editable_grid();

        assert!(grid.begin_edit(0, 1));
        assert!(grid.editing.is_some());

        grid.cancel_edit();
        assert!(!grid.begin_edit(0, 0));
        assert!(grid.editing.is_none());
    }

    #[test]
    fn folding_a_selected_row_keeps_the_column_and_a_selected_column_keeps_the_row() {
        // The library moves a row *or* a column; the ring is a cell. If a fold
        // dropped the other half, an arrow key would send the ring back to the
        // first column or the first row instead of one cell over.
        let mut grid = editable_grid();
        grid.set_active(1, 2);

        grid.select_row(0);
        assert_eq!(grid.active(), Some((0, 2)));
        grid.select_col(1);
        assert_eq!(grid.active(), Some((0, 1)));
    }

    #[test]
    fn folding_with_nothing_active_yet_lands_on_a_cell_that_exists() {
        // The first arrow key of a session arrives with no ring on screen. A
        // half-coordinate is not a cell, so the missing half has to be an
        // origin rather than nothing at all.
        let mut grid = editable_grid();
        grid.select_row(1);
        assert_eq!(grid.active(), Some((1, 0)));

        let mut grid = editable_grid();
        grid.select_col(2);
        assert_eq!(grid.active(), Some((0, 2)));
    }

    #[test]
    fn a_click_and_the_selection_event_it_causes_converge_on_one_cell() {
        // A cell click sets the coordinate here and makes the library emit
        // `SelectRow` for the same row. Whichever arrives first, both have to
        // leave the ring on the clicked cell -- a listener order that decided
        // the answer would be the same two-notions-of-position bug again.
        let mut clicked_first = editable_grid();
        clicked_first.set_active(1, 2);
        clicked_first.select_row(1);

        let mut event_first = editable_grid();
        event_first.select_row(1);
        event_first.set_active(1, 2);

        assert_eq!(clicked_first.active(), Some((1, 2)));
        assert_eq!(event_first.active(), Some((1, 2)));
    }

    #[test]
    fn a_step_off_the_edge_of_the_grid_goes_nowhere() {
        // At an edge the key has to do nothing rather than close the edit or
        // wrap: the input stays open on the cell the user is typing into.
        assert_eq!(step_target((1, 1), &Step::Rows(1), 3, 3), Some((2, 1)));
        assert_eq!(step_target((1, 1), &Step::Cols(-1), 3, 3), Some((1, 0)));
        assert_eq!(step_target((0, 1), &Step::Rows(-1), 3, 3), None);
        assert_eq!(step_target((2, 1), &Step::Rows(1), 3, 3), None);
        assert_eq!(step_target((1, 0), &Step::Cols(-1), 3, 3), None);
        assert_eq!(step_target((1, 2), &Step::Cols(1), 3, 3), None);
    }

    #[test]
    fn an_input_does_not_survive_the_ring_moving_off_its_cell() {
        // An arrow key routes through the same guard a click does. An input
        // holding focus on one cell while the ring sits on another is two cells
        // claiming the keyboard, and `Enter` acting on neither.
        let mut grid = editable_grid();
        grid.set_active(0, 1);
        assert!(grid.begin_edit(0, 1));

        grid.select_row(1);

        assert!(grid.editing.is_none());
        assert_eq!(grid.active(), Some((1, 1)));
    }

    #[test]
    fn a_copy_offers_the_whole_value_and_not_the_string_the_column_shows() {
        // `cmd+c` on a value wider than its column has to carry the value. The
        // clipped display string is what the previous copy gesture deliberately
        // did not read, and the reason it read the fetched row instead.
        let value = "x".repeat(CELL_DISPLAY_LIMIT * 2);
        let mut grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("a"), column("b")],
                rows: vec![vec![Some(value.clone()), None]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        // Nothing active, nothing to copy.
        assert!(grid.active_value().is_none());

        grid.select_row(0);
        assert_eq!(grid.active_value(), Some(value.as_str()));
        assert_ne!(grid.shown(0, 0).as_deref(), Some(value.as_str()));

        // A NULL is an absent value, not the word the grid paints for one.
        grid.select_col(1);
        assert!(grid.active_value().is_none());
    }

    #[test]
    fn a_copy_stays_out_of_the_way_of_an_open_input() {
        // Inside an input `cmd+c` is the text selection's. A grid copy firing
        // there would replace what the user just selected with the whole cell.
        let mut grid = editable_grid();
        grid.set_active(0, 1);
        assert_eq!(grid.active_value(), Some("first"));

        assert!(grid.begin_edit(0, 1));
        assert!(grid.active_value().is_none());

        grid.cancel_edit();
        assert_eq!(grid.active_value(), Some("first"));
    }

    #[test]
    fn a_read_only_cell_still_has_a_value_to_copy() {
        // The point of the gesture: a join, an aggregate or a key column can
        // never open an input, and every one of them has values worth copying.
        let mut keyless = grid_of(&[Some("joined")]);
        keyless.select_row(0);
        assert!(!keyless.editable(0, 0));
        assert_eq!(keyless.active_value(), Some("joined"));

        let mut grid = editable_grid();
        grid.set_active(0, 0);
        assert!(!grid.editable(0, 0));
        assert_eq!(grid.active_value(), Some("7"));
    }

    #[test]
    fn a_fresh_result_set_starts_with_no_active_cell() {
        // A coordinate outliving its rows is what `render_td`'s bounds checks
        // are about: it would put the ring on a cell nobody clicked and point
        // `Enter` at a row that no longer exists. Dropped with the delegate.
        let mut grid = editable_grid();
        grid.set_active(1, 1);

        let replaced = ResultGrid::new(
            QueryResult {
                columns: vec![column("a")],
                rows: vec![vec![Some("only".into())]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        assert!(replaced.active().is_none());
        // And an index that did outlive its rows opens nothing.
        assert!(!grid.begin_edit(9, 1));
    }

    /// A grid over `id, account_id, sku` — one key column between two that are
    /// not, so a mark by position would be visible as a mark on the wrong one.
    fn keyed_grid() -> ResultGrid {
        ResultGrid::new(
            QueryResult {
                columns: vec![column("id"), column("account_id"), column("sku")],
                rows: vec![vec![Some("7".into()), Some("42".into()), Some("x".into())]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
    }

    #[test]
    fn a_column_other_relations_point_at_lists_them_by_table_and_column() {
        let mut grid = keyed_grid();
        let reference = |schema: &str, table: &str, column: &str, referenced: &str| db::Reference {
            schema: schema.into(),
            table: table.into(),
            column: column.into(),
            referenced_column: referenced.into(),
        };
        grid.mark_references(
            &[
                reference("public", "orders", "account_id", "id"),
                reference("audit", "log", "subject", "id"),
                reference("public", "tags", "sku", "sku"),
            ],
            "public",
        );

        let labels: Vec<_> = grid
            .reference_menu(0)
            .expect("id is pointed at")
            .iter()
            .map(|(index, label)| (*index, label.as_ref()))
            .collect();
        assert_eq!(labels, [(0, "orders.account_id"), (1, "audit.log.subject")]);
        assert!(grid.reference_menu(1).is_none());
        assert_eq!(grid.reference_menu(2).map(|menu| menu[0].0), Some(2));
    }

    #[test]
    fn only_the_columns_a_key_names_offer_to_follow_it() {
        let mut grid = keyed_grid();
        grid.mark_foreign_keys(&["account_id".to_string()]);

        assert!(!grid.follows_a_key(0));
        assert!(grid.follows_a_key(1));
        assert!(!grid.follows_a_key(2));
        // One hover group per key column, built here rather than per cell per
        // frame, and none at all for a column with nothing to follow.
        assert_eq!(grid.follow_group(0), None);
        assert_eq!(
            grid.follow_group(1).map(SharedString::as_ref),
            Some("follow-key-1")
        );
        assert_eq!(grid.follow_group(2), None);
    }

    #[test]
    fn a_key_naming_a_column_this_result_does_not_have_marks_nothing() {
        // The aliased-projection case: the structure names `account_id` and the
        // projection called it something else, so nothing is marked -- rather
        // than the column that happens to sit where `account_id` sat.
        let mut grid = keyed_grid();
        grid.mark_foreign_keys(&["owner_id".to_string()]);

        assert!((0..3).all(|col| !grid.follows_a_key(col)));
    }

    #[test]
    fn a_fresh_result_follows_nothing_until_the_structure_says_so() {
        // Every run replaces the delegate whole, so the marks are reapplied
        // from the structure rather than assumed to have survived.
        let mut grid = keyed_grid();
        grid.mark_foreign_keys(&["account_id".to_string()]);
        assert!(grid.follows_a_key(1));

        assert!(!keyed_grid().follows_a_key(1));
    }

    #[test]
    fn a_column_starts_at_the_minimum_width_and_a_restore_never_goes_below_it() {
        let mut grid = editable_grid();
        assert!(
            grid.columns
                .iter()
                .all(|column| column.width == px(MIN_COLUMN_WIDTH))
        );

        grid.set_widths(&[px(100.), px(190.), px(240.)]);
        let restored = ResultGrid::restored(&grid.stored(), Mode::ReadWrite);
        assert_eq!(
            restored
                .columns
                .iter()
                .map(|column| column.width)
                .collect::<Vec<_>>(),
            [px(MIN_COLUMN_WIDTH), px(190.), px(240.)]
        );
    }

    fn four_row_grid() -> ResultGrid {
        ResultGrid::new(
            QueryResult {
                columns: vec![column("id")],
                rows: (0..4).map(|row| vec![Some(row.to_string())]).collect(),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        )
    }

    #[test]
    fn a_plain_click_replaces_the_selection_a_shift_click_extends_it_and_a_toggle_click_adds_or_removes_one()
     {
        let mut grid = four_row_grid();
        grid.click_row(1, false, false);
        assert_eq!(grid.selected_row_indices(), vec![1]);

        grid.click_row(3, true, false);
        assert_eq!(grid.selected_row_indices(), vec![1, 2, 3]);

        grid.click_row(0, false, true);
        assert_eq!(grid.selected_row_indices(), vec![0, 1, 2, 3]);

        // Toggling an already-selected row removes just that one.
        grid.click_row(2, false, true);
        assert_eq!(grid.selected_row_indices(), vec![0, 1, 3]);

        // A plain click replaces the whole selection with the one row.
        grid.click_row(3, false, false);
        assert_eq!(grid.selected_row_indices(), vec![3]);

        grid.clear_row_selection();
        assert!(grid.selected_row_indices().is_empty());
    }

    #[test]
    fn with_nothing_selected_the_active_cells_row_is_what_copies() {
        let mut grid = four_row_grid();
        assert!(grid.selected_row_indices().is_empty());
        grid.set_active(2, 0);
        assert_eq!(grid.selected_row_indices(), vec![2]);
    }

    #[test]
    fn shift_click_keeps_the_first_click_as_anchor_when_the_range_shrinks_or_reverses() {
        let mut grid = four_row_grid();
        grid.click_row(1, false, false);
        grid.set_active(1, 0);

        for (row, expected) in [(3, vec![1, 2, 3]), (2, vec![1, 2]), (0, vec![0, 1])] {
            grid.click_row(row, true, false);
            grid.set_active(row, 0);
            assert_eq!(grid.selected_row_indices(), expected);
            assert_eq!(grid.row_selection_anchor, Some(1));
        }
    }

    #[test]
    fn keyboard_movement_folds_into_the_selection_like_a_plain_click() {
        // `select_row` is what `TableEvent::SelectRow` -- the library's own
        // arrow-key movement -- folds into. Without updating the selection
        // too, the ring would move off a multi-row selection while the old
        // rows kept their tint and "Copy Rows As" kept copying them.
        let mut grid = four_row_grid();
        grid.click_row(1, false, false);
        grid.click_row(3, true, false);
        assert_eq!(grid.selected_row_indices(), vec![1, 2, 3]);

        grid.select_row(2);
        assert_eq!(grid.selected_row_indices(), vec![2]);
        assert_eq!(grid.row_selection_anchor, Some(2));
    }

    #[test]
    fn the_echo_of_a_pointer_selection_keeps_the_rows_it_chose() {
        // A shift- or toggle-click tells the library its row, and the library
        // answers with the same `SelectRow` a keyboard move sends. Folding that
        // echo like a keystroke collapsed every range to the clicked row.
        let mut grid = four_row_grid();
        grid.click_row(0, false, false);
        grid.click_row(2, true, false);
        grid.pointer_row = Some(2);
        grid.select_row(2);
        assert_eq!(grid.selected_row_indices(), vec![0, 1, 2]);

        grid.click_row(3, false, true);
        grid.pointer_row = Some(3);
        grid.select_row(3);
        assert_eq!(grid.selected_row_indices(), vec![0, 1, 2, 3]);

        // Spent by its echo: the next keyboard move collapses as before.
        grid.select_row(1);
        assert_eq!(grid.selected_row_indices(), vec![1]);
    }

    #[test]
    fn a_right_click_outside_the_selection_replaces_it_and_inside_it_leaves_it_alone() {
        // Scenario: left-click row 1, right-click row 3 -- "Copy Row(s) As"
        // must act on row 3, the row actually under the pointer, not the
        // stale selection from before.
        let mut grid = four_row_grid();
        grid.click_row(1, false, false);
        grid.right_click_row(3);
        assert_eq!(grid.selected_row_indices(), vec![3]);

        grid.click_row(1, false, false);
        grid.click_row(2, true, false);
        grid.right_click_row(2);
        assert_eq!(grid.selected_row_indices(), vec![1, 2]);
    }

    #[test]
    fn toggle_can_remove_the_last_row_without_copying_the_active_cell_instead() {
        let mut grid = four_row_grid();
        grid.click_row(1, false, false);
        grid.set_active(1, 0);
        grid.click_row(1, false, true);
        assert!(grid.selected_row_indices().is_empty());
        assert!(grid.rows_as(RowsAs::Text).is_none());
    }

    #[test]
    fn shift_alone_replaces_disjoint_rows_and_secondary_shift_adds_a_range() {
        let mut grid = four_row_grid();
        grid.click_row(0, false, false);
        grid.click_row(2, false, true);
        grid.click_row(3, true, true);
        assert_eq!(grid.selected_row_indices(), vec![0, 2, 3]);
        grid.click_row(3, true, false);
        assert_eq!(grid.selected_row_indices(), vec![2, 3]);

        grid.clear_row_selection();
        grid.click_row(1, true, false);
        grid.click_row(3, true, false);
        assert_eq!(grid.selected_row_indices(), vec![1, 2, 3]);
    }

    #[test]
    fn copy_rows_as_text_joins_every_selected_row_in_order() {
        let mut grid = four_row_grid();
        grid.click_row(2, false, false);
        grid.click_row(0, true, false);
        assert_eq!(grid.rows_as(RowsAs::Text).as_deref(), Some("0\n1\n2"));
    }

    #[test]
    fn a_row_reads_out_as_its_named_and_typed_fields() {
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![
                    DbColumn {
                        name: "id".into(),
                        data_type: Some("int4".into()),
                    },
                    column("note"),
                ],
                rows: vec![vec![Some("7".into()), None]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        let fields = grid.fields(0);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "id");
        assert_eq!(
            fields[0].data_type.as_ref().map(SharedString::as_ref),
            Some("int4")
        );
        assert_eq!(
            fields[0].value.as_ref().map(SharedString::as_ref),
            Some("7")
        );
        // A NULL has to stay absent rather than becoming the word for one.
        assert!(fields[1].value.is_none());
        // A type dbdelve could not learn is shown as nothing, never as a guess.
        assert!(fields[1].data_type.is_none());
        // A selection can outlive the rows it was made against.
        assert!(grid.fields(4).is_empty());
    }

    #[test]
    fn the_inspector_breaks_a_json_document_up_and_leaves_everything_else_alone() {
        let document = indented_json(r#"{"aoi":{"acres":69.7},"tags":[1,2]}"#);
        assert!(document.starts_with("{\n"), "{document}");
        assert!(document.contains("\n  \"tags\""), "{document}");

        // All valid JSON, and all already spelled the only way they can be.
        // Reformatting a scalar is the characters back again, so the parser is
        // never worth reaching for one.
        for scalar in ["22.38", "true", "null", "\"quoted\""] {
            assert_eq!(indented_json(scalar), scalar);
        }
        // Shaped like a document without being one -- a truncated value, or
        // prose that opens with a brace. Handed back as it arrived rather than
        // swallowed by a parse that failed.
        for other in ["{not json at all", "[1, 2", "an ordinary sentence", ""] {
            assert_eq!(indented_json(other), other);
        }
    }

    #[test]
    fn the_inspector_shows_more_of_a_value_than_the_column_does() {
        let value = "x".repeat(FIELD_DISPLAY_LIMIT * 2);
        let grid = grid_of(&[Some(&value)]);
        let field = grid.fields(0).remove(0);
        let shown = field.value.unwrap();

        assert!(shown.chars().count() > CELL_DISPLAY_LIMIT);
        assert_eq!(shown.chars().count(), FIELD_DISPLAY_LIMIT + 1);
    }

    #[test]
    fn a_short_value_is_left_alone() {
        assert_eq!(clip("SELECT"), "SELECT");
    }

    #[test]
    fn a_value_with_line_breaks_paints_as_one_line_from_its_start() {
        assert_eq!(
            clip("create table t (\n    id int,\r\n\n    note text\n)"),
            "create table t ( id int, note text )"
        );
        // Only what is painted: the grid still holds what the server sent.
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![Default::default()],
                rows: vec![vec![Some("a\nb".to_string())]],
                ..Default::default()
            },
            Mode::ReadWrite,
        );
        assert_eq!(grid.result.rows[0][0].as_deref(), Some("a\nb"));
    }

    #[test]
    fn clipping_never_splits_a_multibyte_character() {
        // Byte-slicing this at CELL_DISPLAY_LIMIT would panic mid-codepoint.
        let value = "🌍".repeat(CELL_DISPLAY_LIMIT * 2);
        let clipped = clip(&value);

        assert_eq!(clipped.chars().count(), CELL_DISPLAY_LIMIT + 1);
        assert!(clipped.ends_with('…'));
        assert!(clipped.chars().take(CELL_DISPLAY_LIMIT).all(|c| c == '🌍'));
    }

    #[test]
    fn streamed_flattening_paints_what_flattening_the_whole_value_did() {
        fn whole(value: &str) -> String {
            let flattened = value
                .split(['\n', '\r'])
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            clip_to(&flattened, CELL_DISPLAY_LIMIT)
        }
        let long = "é".repeat(CELL_DISPLAY_LIMIT);
        let inputs = [
            "a\nb".to_string(),
            "\n\n  lead \t  inner  \r\n\r\n\n trail  \n".to_string(),
            "\r\n".to_string(),
            " \n \n ".to_string(),
            format!("é\n{long}"),
            format!("{}\n{}", "x".repeat(150), "y".repeat(149)),
            format!("{}\n{}", "x".repeat(150), "y".repeat(150)),
            format!("{}\n{}", "x".repeat(150), "y".repeat(151)),
            format!("x\n{}  {}", "y".repeat(297), "z".repeat(10)),
            format!("x\n{}", " ".repeat(CELL_DISPLAY_LIMIT * 2)),
        ];
        for input in &inputs {
            assert_eq!(clip(input), whole(input), "{input:?}");
        }

        // No break inside the part that shows: the cell is the value's start,
        // whatever lies past it.
        let huge = format!("{}\nend", "g".repeat(4_000_000));
        assert_eq!(clip(&huge), clip_to(&huge, CELL_DISPLAY_LIMIT));
    }

    #[test]
    fn a_copy_takes_the_whole_value_the_column_could_not_show() {
        let value = "x".repeat(CELL_DISPLAY_LIMIT * 3);
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("a")],
                rows: vec![vec![Some(value.clone())], vec![None]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        assert_eq!(grid.cell(0, 0), Some(value.as_str()));
        assert_ne!(grid.shown(0, 0).as_deref(), Some(value.as_str()));
        // A NULL is an absent value, not the string the cell paints for one.
        assert_eq!(grid.cell(1, 0), None);
        assert_eq!(grid.cell(9, 9), None);
    }

    #[test]
    fn a_short_row_reads_as_absent_rather_than_panicking() {
        // A ragged result set must not be able to abort the render pass.
        let grid = ResultGrid::new(
            QueryResult {
                columns: vec![column("a"), column("b")],
                rows: vec![vec![Some("only one cell".into())]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        assert_eq!(grid.shown(0, 0).as_deref(), Some("only one cell"));
        assert!(
            grid.shown(0, 1).is_none(),
            "row should be short, not padded"
        );
        assert!(grid.shown(9, 9).is_none());
    }

    #[test]
    fn an_in_memory_sort_carries_pending_edits_with_their_rows() {
        // The edit is what the user typed against id 7's row. Left at index
        // 0 it would be applied to id 8, which the sort put there.
        let mut grid = editable_grid();
        assert!(grid.set_pending(0, 1, value("changed")));

        grid.sort_in_memory(vec![(0, false)]);

        assert_eq!(grid.cell(0, 0), Some("8"));
        assert_eq!(
            grid.pending_at(1, 1).map(|edit| edit.value.clone()),
            Some(value("changed"))
        );
        assert!(grid.pending_at(0, 1).is_none());
        let updates = grid.pending_updates();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].keys, vec![("id".to_string(), "7".to_string())]);
    }

    #[test]
    fn clicking_the_last_key_away_puts_the_rows_back_in_the_order_they_came() {
        let mut grid = ResultGrid::new(
            QueryResult {
                columns: vec![typed("n", "int4"), typed("tag", "text")],
                rows: [("2", "x"), ("3", "y"), ("1", "x")]
                    .iter()
                    .map(|(n, tag)| vec![Some(n.to_string()), Some(tag.to_string())])
                    .collect(),
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        grid.sort_in_memory(vec![(1, true)]);
        // The two `x` rows tie, and keep the server's order between them.
        assert_eq!(column_values(&grid, 0), [Some("2"), Some("1"), Some("3")]);
        grid.sort_in_memory(vec![(1, true), (0, false)]);
        grid.sort_in_memory(Vec::new());
        assert_eq!(column_values(&grid, 0), [Some("2"), Some("3"), Some("1")]);
    }

    /// One column's cells, top to bottom.
    fn column_values(grid: &ResultGrid, col: usize) -> Vec<Option<&str>> {
        (0..grid.result.rows.len())
            .map(|row| grid.cell(row, col))
            .collect()
    }

    #[test]
    fn an_in_memory_sort_moves_each_cells_type_with_its_row() {
        let mut grid = ResultGrid::new(
            QueryResult {
                columns: vec![DbColumn {
                    name: "v".into(),
                    data_type: Some("mixed".into()),
                }],
                rows: vec![vec![None], vec![Some("1".into())]],
                cell_types: vec![vec![db::MISSING], vec!["int"]],
                ..QueryResult::default()
            },
            Mode::ReadWrite,
        );

        grid.sort_in_memory(vec![(0, true)]);

        assert_eq!(grid.cell(0, 0), Some("1"));
        assert!(!grid.missing(0, 0));
        assert!(grid.missing(1, 0));
    }

    #[test]
    fn an_in_memory_sort_drops_the_ring_and_the_selection_and_lights_every_header() {
        // Each names a position, and a different row is there now -- the same
        // reason a sort the server runs drops them by replacing the grid.
        let mut grid = four_row_grid();
        grid.set_active(2, 0);
        grid.click_row(1, false, false);
        grid.editing = Some(Editing {
            row: 2,
            col: 0,
            input: None,
        });

        grid.sort_in_memory(vec![(0, false)]);

        assert_eq!(grid.active, None);
        assert!(grid.editing.is_none());
        assert!(grid.selected_row_indices().is_empty());
        assert_eq!(grid.sort, vec![(0, false)]);
        assert!(grid.sortable);
        assert_eq!(grid.cell(0, 0), Some("3"));
    }

    #[test]
    fn a_snapshot_of_rows_sorted_in_memory_keeps_the_order_on_screen() {
        let mut grid = four_row_grid();
        grid.sort_in_memory(vec![(0, false)]);

        let restored = ResultGrid::restored(&grid.stored(), Mode::ReadWrite);

        assert_eq!(restored.result.rows, grid.result.rows);
        assert_eq!(restored.cell(0, 0), Some("3"));
        assert_eq!(restored.sort, vec![(0, false)]);
    }

    #[test]
    fn re_sorting_restored_rows_keeps_the_active_cell() {
        let mut grid = four_row_grid();
        grid.sort_in_memory(vec![(0, false)]);
        grid.set_active(2, 0);

        let mut restored = ResultGrid::restored(&grid.stored(), Mode::ReadWrite);
        assert_eq!(restored.active(), Some((2, 0)));
        restored.sort_restored_in_memory(vec![(0, false)]);

        assert_eq!(restored.active(), Some((2, 0)));
        assert_eq!(restored.result.rows, grid.result.rows);
    }
}
