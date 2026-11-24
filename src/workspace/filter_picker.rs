//! The filter picker: a dialog that edits one profile's include list.
//!
//! On open it reads, unfiltered, the databases the tree draws
//! (`Connection::catalog_databases`), and the own database's schemas where the
//! target has both levels (the catalog with its routines and sequences merged,
//! as the tree assembles it), whatever the tree shows, so a name the filter
//! hides can be ticked again. Where the catalog lists databases as schemas that leaves
//! out the system databases and any holding nothing, so the preview's total is
//! the connection row's. Save writes the profile's lists
//! and reads the catalog again; it never reconnects, because the lists are not
//! part of the connection. Which sections exist comes from `Target::levels()`,
//! never from the engine.
//!
//! The same dialog has a second mode, one schema's table list. Its one section
//! lists the relations of that schema the catalog holds, the shown and the ones
//! its list leaves out ([`Catalog::relations_of`](crate::db::Catalog)), so it
//! reads nothing and has no loading state. Save writes only that list.

use std::ops::Range;

use gpui::SharedString;
use gpui_component::{
    Sizable,
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
};

use super::*;
use crate::db::{Include, Levels, RelationKind, Shown, same_name};
use crate::fuzzy::{Fuzzy, Query};
use crate::sidebar::{NodeKind, SelectedNode};
use crate::theme::Theme;
use crate::ui::{button_inked, highlighted, quiet_ink, well};

/// Which of the two lists a section edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Which {
    Databases,
    Schemas,
    /// The relations of one schema, in the table mode.
    Tables,
}

impl Which {
    fn label(self) -> &'static str {
        match self {
            Self::Databases => "Databases",
            Self::Schemas => "Schemas",
            Self::Tables => "Tables",
        }
    }

    /// Singular only when the list holds one name.
    fn noun(self, total: usize) -> &'static str {
        match (self, total == 1) {
            (Self::Databases, true) => "database",
            (Self::Databases, false) => "databases",
            (Self::Schemas, true) => "schema",
            (Self::Schemas, false) => "schemas",
            (Self::Tables, true) => "table",
            (Self::Tables, false) => "tables",
        }
    }

    /// What the dialog's ids and selectors call it.
    fn key(self) -> &'static str {
        match self {
            Self::Databases => "databases",
            Self::Schemas => "schemas",
            Self::Tables => "tables",
        }
    }
}

/// Whether a target of shape `levels` has schemas to choose between inside a
/// database. Where a database is its own schema, or holds objects directly,
/// the one list is the whole filter.
fn lists_schemas(levels: Levels) -> bool {
    levels == Levels::DatabaseThenSchema
}

/// What the dialog says the filter is not. A filter narrows what is listed;
/// a statement can still read what it hides, so the sentence is always
/// drawn. Completion and Go to Object read the narrowed catalog, which on
/// MySQL, MariaDB and ClickHouse is narrowed in the read itself, so the
/// sentence says they offer only what is listed.
pub(crate) fn boundary(levels: Levels) -> &'static str {
    match lists_schemas(levels) {
        true => {
            "This filter changes what is listed, not what can be reached: a statement can \
             still read a hidden database or schema, though completion and Go to Object \
             offer only what is listed."
        }
        false => {
            "This filter changes what is listed, not what can be reached: a statement can \
             still read a hidden database, though completion and Go to Object offer only \
             what is listed."
        }
    }
}

/// What the filter says it is not, for the table list: as [`boundary`], for the
/// relations of one schema.
const TABLE_BOUNDARY: &str = "This filter changes what is listed, not what can be reached: a \
     statement can still read a hidden table, though completion and Go to Object offer only \
     what is listed.";

/// The schema whose table list the picker edits, and where its catalog is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TableScope {
    /// The database that keys the list, as [`crate::db::TableFilter`] says.
    pub(crate) database: Option<String>,
    pub(crate) schema: String,
    /// The database whose catalog holds the schema when it is not the
    /// connection's own, which is read again over its own session on Save.
    pub(crate) elsewhere: Option<String>,
}

/// The table mode's state: what it edits, and the kind of each name it lists.
struct TablesOf {
    scope: TableScope,
    kinds: Vec<(String, RelationKind)>,
}

/// What a row says of a relation that is not a plain table.
fn kind_hint(kind: RelationKind) -> Option<&'static str> {
    match kind {
        RelationKind::Table | RelationKind::PartitionedTable => None,
        RelationKind::View => Some("view"),
        RelationKind::MaterializedView => Some("materialized view"),
        RelationKind::ForeignTable => Some("foreign table"),
        RelationKind::Dictionary => Some("dictionary"),
    }
}

/// What a section's full list of names has got to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Listing {
    Loading,
    Loaded(Vec<String>),
    Failed(String),
}

/// One list of the profile's, as the dialog edits it.
pub(crate) struct PickerSection {
    pub(crate) listing: Listing,
    /// The listed names ticked by hand, in the spelling the server lists.
    pub(crate) ticked: Vec<String>,
    /// What the pattern field holds, split on commas: wildcards, and names the
    /// server does not list. Kept in step with the field by its subscription.
    pub(crate) patterns: Vec<String>,
    /// The comma-separated entries that are not ticked names.
    pub(crate) pattern_field: Entity<InputState>,
    /// Narrows the drawn rows; the stored rule never sees it.
    pub(crate) search: Entity<InputState>,
}

pub(crate) struct FilterPicker {
    /// The connection being edited, by id.
    pub(crate) profile: String,
    /// Numbers each open, so a listing that lands after the dialog closed, or
    /// after another was put up, changes nothing.
    request: u64,
    levels: Levels,
    /// The database the connection is in, which is shown whatever the list says.
    own_database: Option<String>,
    /// The schema the connection is in, shown whatever the schemas list says
    /// in the own database.
    active_schema: Option<String>,
    /// What the profile held when the dialog opened.
    saved: Include,
    /// The databases section; in the table mode, the one section, the tables'.
    pub(crate) databases: PickerSection,
    /// Set in the table mode, which has no other section.
    tables: Option<TablesOf>,
    /// The own database's schemas, on a target that has both levels.
    pub(crate) schemas: Option<PickerSection>,
    /// The first draw after opening puts the keyboard in the first search box.
    focus_search: std::cell::Cell<bool>,
}

impl PickerSection {
    /// Before the names arrive every saved entry waits in the pattern field,
    /// so a list that never loads still writes back what it held.
    fn loading(
        saved: &[String],
        which: Which,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let seed = saved.join(", ");
        let pattern_field = cx.new(|cx| {
            let mut field =
                InputState::new(window, cx).placeholder("Patterns, e.g. prod_*, ?_test");
            field.set_value(seed, window, cx);
            field
        });
        cx.subscribe(
            &pattern_field,
            move |workspace, field, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    workspace.patterns_typed(which, &field, cx);
                }
            },
        )
        .detach();
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(format!("Search {}", which.key())));
        cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        Self {
            listing: Listing::Loading,
            ticked: Vec::new(),
            patterns: saved.to_vec(),
            pattern_field,
            search,
        }
    }

    /// The list Save writes: ticked names in the order and spelling the server
    /// lists them, then the typed patterns, blanks dropped and duplicates
    /// ignoring case dropped.
    fn entries(&self) -> Vec<String> {
        let ticked: Vec<String> = match &self.listing {
            Listing::Loaded(names) => names
                .iter()
                .filter(|name| self.ticked.contains(name))
                .cloned()
                .collect(),
            _ => self.ticked.clone(),
        };
        written_entries(&ticked, &self.patterns.join(","))
    }

    /// The server's names have arrived: each entry that names one becomes a
    /// tick, and the rest stay in the field as typed. What was typed while
    /// the names were read counts as much as what was saved.
    fn listed(&mut self, names: Vec<String>, window: &mut Window, cx: &mut Context<Workspace>) {
        let (ticked, kept) = split(&self.patterns, &names);
        if kept != self.patterns {
            self.set_patterns(kept, window, cx);
        }
        for name in ticked {
            if !self.ticked.contains(&name) {
                self.ticked.push(name);
            }
        }
        self.listing = Listing::Loaded(names);
    }

    /// Put `patterns` in the field and keep the list in step: `set_value`
    /// reports no change, so the subscription does not do it.
    fn set_patterns(
        &mut self,
        patterns: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let text = patterns.join(", ");
        self.pattern_field
            .update(cx, |field, cx| field.set_value(text, window, cx));
        self.patterns = patterns;
    }
}

/// `entries` in the order given, trimmed, blanks dropped and a repeat ignoring
/// case dropped.
fn merged<'a>(entries: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for entry in entries.map(str::trim) {
        if !entry.is_empty() && !kept.iter().any(|known| same_name(known, entry)) {
            kept.push(entry.to_string());
        }
    }
    kept
}

/// What Save writes for one list: the `ticked` names, then the comma-separated
/// `patterns` as typed, each once.
///
/// ponytail: an entry that itself holds a comma is split when the field is
/// edited; the ceiling is a database named with one, the upgrade is an escape.
fn written_entries(ticked: &[String], patterns: &str) -> Vec<String> {
    merged(ticked.iter().map(String::as_str).chain(patterns.split(',')))
}

/// Whether `pattern` admits `name`, by the rule the read sites apply: asked of
/// the stored rule's own matcher, so the preview cannot drift from what Save
/// will show. The databases list stands for any list here; a name this admits
/// is admitted by the same pattern in the schemas list.
fn admits(pattern: &str, name: &str) -> bool {
    Include {
        databases: vec![pattern.to_string()],
        ..Include::default()
    }
    .resolve(Levels::DatabaseHoldsObjects, None, None)
    .shows_database(name)
}

/// How many of the listed relation `names` a table list of `entries` shows.
fn tables_shown(entries: &[String], names: &[String]) -> usize {
    names
        .iter()
        .filter(|name| entries.iter().any(|entry| admits(entry, name)))
        .count()
}

/// The line naming the patterns that admit none of the listed `names`: the
/// first three, then how many more. `noun` is what the names are, `database` or
/// `schema in {database}`.
fn unmatched_line(noun: &str, patterns: &[String], names: &[String]) -> Option<String> {
    let patterns = merged(patterns.iter().map(String::as_str));
    let unmatched: Vec<&str> = patterns
        .iter()
        .map(String::as_str)
        .filter(|pattern| !names.iter().any(|name| admits(pattern, name)))
        .collect();
    if unmatched.is_empty() {
        return None;
    }
    let named = unmatched[..unmatched.len().min(3)].join(", ");
    Some(match unmatched.len() - unmatched.len().min(3) {
        0 => format!("No {noun} matches: {named}"),
        more => format!("No {noun} matches: {named} and {more} more"),
    })
}

impl FilterPicker {
    /// The always-shown rows say why they are: the connection's own database
    /// is checked and cannot be unticked.
    pub(crate) fn row_hint(&self, name: &str) -> Option<&'static str> {
        self.own_database
            .as_deref()
            .is_some_and(|own| same_name(own, name))
            .then_some("current database")
    }

    /// The same for the active schema, which only the own database's list
    /// holds: the schema list the dialog fetches is that one's.
    pub(crate) fn schema_hint(&self, name: &str) -> Option<&'static str> {
        self.active_schema
            .as_deref()
            .is_some_and(|active| same_name(active, name))
            .then_some("active schema")
    }

    fn hint(&self, which: Which, name: &str) -> Option<&'static str> {
        match which {
            Which::Databases => self.row_hint(name),
            Which::Schemas => self.schema_hint(name),
            Which::Tables => None,
        }
    }

    /// What a listed row says about itself: why it is shown whatever the list
    /// says, or that a typed pattern shows it. A name that is ticked as well
    /// shows once, as a ticked row.
    pub(crate) fn listed_hint(&self, which: Which, name: &str) -> Option<&'static str> {
        self.hint(which, name).or_else(|| {
            let section = self.section(which)?;
            let ticked = section.ticked.iter().any(|ticked| same_name(ticked, name));
            let matched = section.patterns.iter().any(|pattern| admits(pattern, name));
            (matched && !ticked).then_some("by pattern")
        })
    }

    /// The line under the preview naming the typed patterns that match no
    /// listed name, once the names are read.
    pub(crate) fn unmatched(&self, which: Which) -> Option<String> {
        let section = self.section(which)?;
        let Listing::Loaded(names) = &section.listing else {
            return None;
        };
        let noun = match (which, self.own_database.as_deref()) {
            (Which::Databases, _) => "database".to_string(),
            (Which::Schemas, Some(database)) => format!("schema in {database}"),
            (Which::Schemas, None) => "schema".to_string(),
            (Which::Tables, _) => format!("table in {}", self.tables.as_ref()?.scope.schema),
        };
        unmatched_line(&noun, &section.patterns, names)
    }

    fn section(&self, which: Which) -> Option<&PickerSection> {
        match which {
            Which::Databases => self.tables.is_none().then_some(&self.databases),
            Which::Schemas => self.schemas.as_ref(),
            Which::Tables => self.tables.as_ref().map(|_| &self.databases),
        }
    }

    fn section_mut(&mut self, which: Which) -> Option<&mut PickerSection> {
        match which {
            Which::Databases => self.tables.is_none().then_some(&mut self.databases),
            Which::Schemas => self.schemas.as_mut(),
            Which::Tables => self.tables.as_ref().map(|_| &mut self.databases),
        }
    }

    /// What a section says in place of its rows: that the names are being
    /// read, that they could not be, or that there are none.
    pub(crate) fn status_line(&self, which: Which) -> Option<String> {
        let key = which.key();
        Some(match &self.section(which)?.listing {
            Listing::Loading => format!("Listing {key}…"),
            Listing::Failed(message) => format!("The list of {key} could not be read: {message}"),
            Listing::Loaded(names) if names.is_empty() => match which {
                Which::Databases => "The server lists no databases.".into(),
                Which::Schemas => match self.own_database.as_deref() {
                    Some(database) => format!("No schemas in {database}."),
                    None => "No schemas.".into(),
                },
                Which::Tables => match &self.tables {
                    Some(tables) => format!("No tables in {}.", tables.scope.schema),
                    None => "No tables.".into(),
                },
            },
            Listing::Loaded(_) => return None,
        })
    }

    /// What a section says when its search leaves no name drawn.
    pub(crate) fn no_match_line(&self, which: Which) -> String {
        format!("No {} matches the search.", which.noun(1))
    }

    /// Whether either list holds anything Remove Filter would empty.
    pub(crate) fn can_remove(&self) -> bool {
        [Which::Databases, Which::Schemas, Which::Tables]
            .into_iter()
            .filter_map(|which| self.section(which))
            .any(|section| !section.ticked.is_empty() || !section.patterns.is_empty())
    }

    pub(crate) fn boundary(&self) -> &'static str {
        match self.tables {
            Some(_) => TABLE_BOUNDARY,
            None => boundary(self.levels),
        }
    }

    /// Says which schema the table list is of, and what it holds.
    pub(crate) fn tables_caption(&self) -> Option<String> {
        let scope = &self.tables.as_ref()?.scope;
        Some(format!(
            "Tables of {}, with its views and other relations.",
            scope.schema
        ))
    }

    /// What a listed row says of what it is, besides why it is shown: the
    /// kind of a relation that is not a plain table.
    pub(crate) fn kind_note(&self, name: &str) -> Option<&'static str> {
        let (_, kind) = self
            .tables
            .as_ref()?
            .kinds
            .iter()
            .find(|(listed, _)| listed == name)?;
        kind_hint(*kind)
    }

    pub(crate) fn heading(&self) -> &'static str {
        if self.tables.is_some() {
            return "Filter Tables";
        }
        match self.schemas {
            Some(_) => "Filter databases and schemas",
            None => "Filter databases",
        }
    }

    /// Says which database the schema list is of, and that the filter reaches
    /// the others too.
    pub(crate) fn schemas_caption(&self) -> Option<String> {
        self.schemas.as_ref()?;
        let database = self.own_database.as_deref()?;
        Some(format!(
            "Schemas of {database}. The schema filter applies inside every database."
        ))
    }

    fn loading(&self) -> bool {
        [Which::Databases, Which::Schemas]
            .into_iter()
            .filter_map(|which| self.section(which))
            .any(|section| section.listing == Listing::Loading)
    }

    /// What Save writes: each section's entries, and for a list with no
    /// section the one the profile held, written as a file is read (trimmed,
    /// blanks dropped, at most `ENTRIES_CAP` each) so the profile, the file
    /// and the next load agree.
    fn written(&self) -> Include {
        if let Some(tables) = &self.tables {
            let mut include = self.saved.clone();
            include.set_tables(
                tables.scope.database.as_deref(),
                &tables.scope.schema,
                self.databases.entries(),
            );
            return include;
        }
        let schemas = self
            .schemas
            .as_ref()
            .map_or_else(|| self.saved.schemas.clone(), PickerSection::entries);
        // The table lists are not this dialog's: Save keeps the profile's.
        Include::from_stored(self.databases.entries(), schemas)
            .with_tables(self.saved.tables.clone())
    }

    /// Nothing to write while a section is still being read, or when the lists
    /// are the ones the profile already holds.
    pub(crate) fn can_save(&self) -> bool {
        let written = self.written();
        let unchanged = match &self.tables {
            Some(tables) => {
                let (database, schema) = (tables.scope.database.as_deref(), &tables.scope.schema);
                same_entries(
                    written.tables_of(database, schema),
                    self.saved.tables_of(database, schema),
                )
            }
            None => unchanged(&written, &self.saved),
        };
        !self.loading() && !unchanged
    }

    /// The line under a list: how many of the listed names the list, as
    /// ticked and typed, shows now.
    pub(crate) fn preview(&self, which: Which) -> Option<String> {
        let section = self.section(which)?;
        let names = match &section.listing {
            Listing::Loading => return None,
            Listing::Failed(_) => {
                return Some("Matches cannot be previewed: the names were not read.".into());
            }
            Listing::Loaded(names) => names,
        };
        let total = names.len();
        let noun = which.noun(total);
        let place = match which {
            Which::Databases => String::new(),
            Which::Tables => self
                .tables
                .as_ref()
                .map(|tables| format!(" in {}", tables.scope.schema))
                .unwrap_or_default(),
            Which::Schemas => self
                .own_database
                .as_deref()
                .map(|database| format!(" in {database}"))
                .unwrap_or_default(),
        };
        let entries = section.entries();
        if entries.is_empty() {
            return Some(format!("Showing all {total} {noun}{place}"));
        }
        // The stored rule's own matcher, as `admits` asks it, so the count
        // cannot drift from what Save will show.
        if which == Which::Tables {
            let count = tables_shown(&entries, names);
            return Some(format!("Showing {count} of {total} {noun}{place}"));
        }
        let include = match which {
            Which::Databases | Which::Tables => Include::from_stored(entries, Vec::new()),
            Which::Schemas => Include::from_stored(Vec::new(), entries),
        };
        let shown = include.resolve(
            self.levels,
            self.own_database.as_deref(),
            self.active_schema.as_deref(),
        );
        let count = names
            .iter()
            .filter(|name| match which {
                Which::Databases | Which::Tables => shown.shows_database(name),
                Which::Schemas => shown.shows_schema(name),
            })
            .count();
        Some(format!("Showing {count} of {total} {noun}{place}"))
    }
}

/// Whether `written` and `saved` hold the same lists. Case alone is not a
/// change (the filter reads the two spellings alike, so a resave would rewrite
/// the file for nothing); order is, because it is what the file holds.
fn unchanged(written: &Include, saved: &Include) -> bool {
    same_entries(&written.databases, &saved.databases)
        && same_entries(&written.schemas, &saved.schemas)
}

/// Whether two lists hold the same entries in the same order, case aside.
fn same_entries(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_name(a, b))
}

/// The saved entries as rows: an entry that names a listed name, with no
/// wildcard, is that row ticked; every other entry is kept as typed.
fn split(saved: &[String], names: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut ticked, mut patterns) = (Vec::new(), Vec::new());
    for entry in saved {
        let listed = (!entry.contains(['*', '?']))
            .then(|| names.iter().find(|name| same_name(name, entry)))
            .flatten();
        match listed {
            Some(name) if ticked.contains(name) => {}
            Some(name) => ticked.push(name.clone()),
            None => patterns.push(entry.clone()),
        }
    }
    (ticked, patterns)
}

/// The names the search leaves drawn, by their place in the full list, each
/// with the bytes the search matched. The one matcher the palette and the
/// sidebar search use; the stored rule is `db::Include`'s and never goes
/// through it.
fn drawn(names: &[String], search: &str) -> Vec<(usize, Vec<Range<usize>>)> {
    let query = Query::new(search);
    let mut fuzzy = Fuzzy::new();
    let mut rows = Vec::new();
    for (index, name) in names.iter().enumerate() {
        if fuzzy.matches(&query, name) {
            rows.push((index, fuzzy.highlights(&query, name)));
        }
    }
    rows
}

/// What [`read_names_for_picker`] brings back: the databases and the schema
/// names, each present only when it was asked for.
type PickerNames = (
    Option<Result<Databases, DbError>>,
    Option<Result<Vec<String>, DbError>>,
);

/// The blocking read the picker sends on open and on retry, run off the UI
/// thread so a test can drive it without a window. The databases are the ones
/// the tree draws (`Connection::catalog_databases`), so the picker's total is
/// the connection row's; the schemas are the whole catalog's with its routines
/// and sequences merged in, as the tree assembles it.
fn read_names_for_picker(
    connection: Connection,
    engine: Engine,
    in_transaction: bool,
    databases: bool,
    schemas: bool,
) -> PickerNames {
    // A failed read inside the user's transaction fails the transaction on
    // Postgres, so both are read on another session, and one that cannot be
    // had fails them rather than fall back to the user's.
    let reader = match in_transaction {
        true => connection.apart(),
        false => Ok(connection),
    };
    let databases = databases.then(|| reader.clone()?.catalog_databases(engine));
    let schemas = schemas.then(|| {
        let reader = reader?;
        let mut catalog = reader.catalog(&Shown::everything())?;
        // A schema holding only a routine or a sequence is drawn by the tree
        // after its merge; a list without it would offer no row to tick and a
        // total short of the row's. When the routines read fails the tree
        // keeps its relations, so a schema list is never lost to it either.
        if let Ok(routines) = reader.routines(&Shown::everything()) {
            catalog.merge(routines);
        }
        Ok::<_, DbError>(
            catalog
                .schemas
                .into_iter()
                .map(|schema| schema.name)
                .collect::<Vec<_>>(),
        )
    });
    (databases, schemas)
}

impl Workspace {
    /// Put the picker up over `profile`, which must be connected and have
    /// databases to choose between, and read the server's databases for it,
    /// and the own database's schemas where there are two levels.
    pub(crate) fn open_filter_picker(
        &mut self,
        profile: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(found) = self.profiles.iter().find(|found| found.id == profile) else {
            return;
        };
        let target = found.config.target();
        let levels = target.levels();
        if levels == Levels::Flat {
            return;
        }
        if found.connection().is_none() {
            self.note("The connection is not open.".into(), cx);
            return;
        }
        let wants_schemas = lists_schemas(levels);
        self.filter_picker = Some(FilterPicker {
            profile: found.id.clone(),
            request: 0,
            levels,
            own_database: found.active_database(),
            active_schema: found.active_schema().map(str::to_string),
            saved: found.include.clone(),
            databases: PickerSection::loading(
                &found.include.databases,
                Which::Databases,
                window,
                cx,
            ),
            tables: None,
            schemas: wants_schemas.then(|| {
                PickerSection::loading(&found.include.schemas, Which::Schemas, window, cx)
            }),
            focus_search: std::cell::Cell::new(true),
        });
        self.list_names_for_picker(true, wants_schemas, window, cx);
    }

    /// The schema a table filter would edit for the row `node` is, and where
    /// its catalog is: a schema row on every shape, and where the database
    /// row holds the objects, that row. `None` when the row has no relations
    /// to choose between yet, or its connection is not open.
    pub(crate) fn table_scope_of(&self, node: &SelectedNode) -> Option<TableScope> {
        let profile = self
            .profiles
            .iter()
            .find(|found| found.id == node.profile)?;
        match node.kind {
            NodeKind::Schema => table_scope(profile, node.database.as_deref(), Some(&node.name)),
            NodeKind::Database
                if profile.config.target().levels() == Levels::DatabaseHoldsObjects =>
            {
                table_scope(profile, node.database.as_deref(), None)
            }
            _ => None,
        }
    }

    /// The same for the schema the front connection is in, which the palette
    /// offers it for.
    pub(crate) fn front_table_scope(&self) -> Option<TableScope> {
        let profile = self.profile()?;
        table_scope(profile, None, profile.schema_in().as_deref())
    }

    /// Put the picker up over the table list of `scope`, listing the
    /// relations its catalog holds: the ones shown and the ones the list
    /// leaves out. Nothing is read, so the section is loaded as it opens.
    pub(crate) fn open_table_picker(
        &mut self,
        scope: TableScope,
        profile: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(found) = self.profiles.iter().find(|found| found.id == profile) else {
            return;
        };
        let Some(catalog) = found.catalog_of(scope.elsewhere.as_deref()) else {
            return;
        };
        let relations = catalog.relations_of(&scope.schema);
        let mut kinds: Vec<(String, RelationKind)> = relations
            .shown
            .into_iter()
            .chain(relations.hidden)
            .collect();
        kinds.sort_by_cached_key(|(name, _)| name.to_lowercase());
        let names: Vec<String> = kinds.iter().map(|(name, _)| name.clone()).collect();
        let saved = found.include.clone();
        let mut section = PickerSection::loading(
            saved.tables_of(scope.database.as_deref(), &scope.schema),
            Which::Tables,
            window,
            cx,
        );
        section.listed(names, window, cx);
        self.filter_picker = Some(FilterPicker {
            profile: found.id.clone(),
            request: 0,
            levels: found.config.target().levels(),
            own_database: found.active_database(),
            active_schema: None,
            saved,
            databases: section,
            tables: Some(TablesOf { scope, kinds }),
            schemas: None,
            focus_search: std::cell::Cell::new(true),
        });
        cx.notify();
    }

    /// Read the names of the lists asked for, unfiltered, since the names the
    /// tree holds are only what the filter lets through: the databases the
    /// tree draws, which leaves out the server's own and any holding nothing
    /// where the catalog lists databases as schemas, and every schema. A new
    /// `request` number goes with it, so what an earlier read brings back is
    /// dropped.
    fn list_names_for_picker(
        &mut self,
        databases: bool,
        schemas: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = self.filter_picker.as_ref() else {
            return;
        };
        let Some(found) = self
            .profiles
            .iter()
            .find(|found| found.id == picker.profile)
        else {
            return;
        };
        let Some(connection) = found.connection() else {
            return;
        };
        let engine = found.config.target().engine;
        let in_transaction = found.in_transaction;
        self.last_filter_request += 1;
        let request = self.last_filter_request;
        if let Some(picker) = self.filter_picker.as_mut() {
            picker.request = request;
            for (which, wanted) in [(Which::Databases, databases), (Which::Schemas, schemas)] {
                if let Some(section) = picker.section_mut(which).filter(|_| wanted) {
                    section.listing = Listing::Loading;
                }
            }
        }
        cx.notify();
        let fetch = spawn_blocking(cx.background_executor(), move || {
            read_names_for_picker(connection, engine, in_transaction, databases, schemas)
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let (databases, schemas) = fetch.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                workspace.names_listed_for_picker(request, databases, schemas, window, cx);
            });
        })
        .detach();
    }

    /// Ask again for a list that could not be read, and for any list still
    /// being read, which the new request number would otherwise drop.
    fn retry_listing(&mut self, which: Which, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.filter_picker.as_ref() else {
            return;
        };
        let wanted = |other: Which| {
            which == other
                || picker
                    .section(other)
                    .is_some_and(|section| section.listing == Listing::Loading)
        };
        let (databases, schemas) = (wanted(Which::Databases), wanted(Which::Schemas));
        self.list_names_for_picker(databases, schemas, window, cx);
    }

    pub(crate) fn names_listed_for_picker(
        &mut self,
        request: u64,
        databases: Option<Result<Databases, DbError>>,
        schemas: Option<Result<Vec<String>, DbError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Closed, or put up again, since: the answer is not this dialog's.
        if self
            .filter_picker
            .as_ref()
            .is_none_or(|picker| picker.request != request)
        {
            return;
        }
        // Its connection went from the front while it was read, and nothing
        // draws the dialog to answer.
        if self.filter_picker_in_front().is_none() {
            self.filter_picker = None;
            return;
        }
        let Some(picker) = self.filter_picker.as_mut() else {
            return;
        };
        match databases {
            None => {}
            Some(Ok(listed)) => {
                picker.databases.listed(listed.names, window, cx);
                if listed.current.is_some() {
                    picker.own_database = listed.current;
                }
            }
            Some(Err(error)) => picker.databases.listing = Listing::Failed(error.message),
        }
        if let (Some(section), Some(result)) = (picker.schemas.as_mut(), schemas) {
            match result {
                Ok(names) => section.listed(names, window, cx),
                Err(error) => section.listing = Listing::Failed(error.message),
            }
        }
        cx.notify();
    }

    /// The picker, when it is the front connection's: one left up for a
    /// connection that is no longer in front is not drawn and `escape` does
    /// not stop at it.
    pub(crate) fn filter_picker_in_front(&self) -> Option<&FilterPicker> {
        let profile = self.profile()?;
        self.filter_picker
            .as_ref()
            .filter(|picker| picker.profile == profile.id)
    }

    /// Close the dialog and hand the keyboard back. Answers whether it was
    /// drawn, so `escape` is only spent on a dialog that was.
    pub(crate) fn close_filter_picker(&mut self, cx: &mut Context<Self>) -> bool {
        if self.filter_picker_in_front().is_none() {
            return false;
        }
        self.filter_picker = None;
        self.refocus_front();
        cx.notify();
        true
    }

    /// A change in a section's pattern field: what Save writes and what the
    /// rows and the preview say follow it.
    fn patterns_typed(&mut self, which: Which, field: &Entity<InputState>, cx: &mut Context<Self>) {
        let typed = field.read(cx).value();
        let Some(section) = self
            .filter_picker
            .as_mut()
            .and_then(|picker| picker.section_mut(which))
            .filter(|section| section.pattern_field == *field)
        else {
            return;
        };
        section.patterns = typed
            .split(',')
            .map(str::trim)
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_string)
            .collect();
        cx.notify();
    }

    fn tick_name(&mut self, which: Which, name: &str, on: bool, cx: &mut Context<Self>) {
        let Some(section) = self
            .filter_picker
            .as_mut()
            .and_then(|picker| picker.section_mut(which))
        else {
            return;
        };
        section.ticked.retain(|ticked| ticked != name);
        if on {
            section.ticked.push(name.to_string());
        }
        cx.notify();
    }

    /// Tick, or untick, the names the search leaves drawn. A name that is
    /// shown whatever the list says, or by a typed pattern, has no tick to
    /// set; the pattern field is not touched.
    fn tick_drawn(&mut self, which: Which, on: bool, cx: &mut Context<Self>) {
        let Some(picker) = self.filter_picker.as_ref() else {
            return;
        };
        let Some(section) = picker.section(which) else {
            return;
        };
        let Listing::Loaded(names) = &section.listing else {
            return;
        };
        let search = section.search.read(cx).value();
        let drawn: Vec<String> = drawn(names, &search)
            .into_iter()
            .map(|(index, _)| names[index].clone())
            .filter(|name| !on || picker.listed_hint(which, name).is_none())
            .collect();
        let Some(section) = self
            .filter_picker
            .as_mut()
            .and_then(|picker| picker.section_mut(which))
        else {
            return;
        };
        section.ticked.retain(|ticked| !drawn.contains(ticked));
        if on {
            section.ticked.extend(drawn);
        }
        cx.notify();
    }

    /// Empty both lists in the dialog. The profile keeps what it holds until
    /// Save Filter writes this.
    fn remove_picker_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.filter_picker.as_mut() else {
            return;
        };
        for section in [Some(&mut picker.databases), picker.schemas.as_mut()]
            .into_iter()
            .flatten()
        {
            section.ticked.clear();
            section.set_patterns(Vec::new(), window, cx);
        }
        cx.notify();
    }

    /// Write the lists to the profile and read its catalog again. The lists
    /// are not part of the connection, so nothing reconnects and no tab
    /// closes.
    pub(crate) fn save_filter(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = self
            .filter_picker_in_front()
            .filter(|picker| picker.can_save())
        else {
            return;
        };
        if let Some(tables) = &picker.tables {
            let (id, include, elsewhere) = (
                picker.profile.clone(),
                picker.written(),
                tables.scope.elsewhere.clone(),
            );
            self.save_table_filter(&id, include, elsewhere, cx);
            return;
        }
        let id = picker.profile.clone();
        let include = picker.written();
        let Some(profile) = self.profiles.iter_mut().find(|found| found.id == id) else {
            return;
        };
        profile.include = include;
        // A refresh declines to start while a read is under way, so the one
        // that lands reads again instead (the filter is saved either way).
        let under_way = profile.refreshing_schema()
            || matches!(
                profile.catalog,
                CatalogState::Loading | CatalogState::Loaded(_, Routines::Loading)
            );
        profile.reread_after_load = under_way;
        // What the filter now hides is not read: its catalog goes.
        let shown = profile.shown();
        profile
            .database_catalogs
            .retain(|database, _| shown.shows_database(database));
        // A session opened only for the tree to read a database now hidden
        // has nothing left to read; one a tab is bound to stays.
        profile.release_hidden_sessions(&shown);
        // A listing asked under the old filter may still land and put back a
        // database this one hides; it is overtaken here. Until the next one
        // lands -- or for good, if it fails -- the list held is narrowed by
        // the new filter, its hidden count raised by what that takes out.
        profile.start_listing();
        let held = profile.databases.names.len();
        profile
            .databases
            .names
            .retain(|database| shown.shows_database(database));
        profile.databases.left_out += held - profile.databases.names.len();
        self.remember_profiles(cx);
        self.close_filter_picker(cx);
        if !under_way {
            self.refresh_schema(cx);
        }
    }

    /// Write one schema's table list and read the catalog it narrows again.
    /// Another database's is read over its own session; the own one by the
    /// refresh, which a read under way hands over to the one that lands. The
    /// lists of databases and schemas, and every connection, stay as they
    /// were.
    fn save_table_filter(
        &mut self,
        id: &str,
        include: Include,
        elsewhere: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profiles.iter_mut().find(|found| found.id == id) else {
            return;
        };
        profile.include = include;
        let under_way = profile.refreshing_schema()
            || matches!(
                profile.catalog,
                CatalogState::Loading | CatalogState::Loaded(_, Routines::Loading)
            );
        if elsewhere.is_none() {
            profile.reread_after_load = under_way;
        }
        self.remember_profiles(cx);
        self.close_filter_picker(cx);
        match elsewhere {
            Some(database) => self.reload_database(id, database, cx),
            None if !under_way => self.refresh_schema(cx),
            None => {}
        }
    }

    /// One list of the dialog: its label, what it is of, a search over its
    /// names, the names over a well with the field for patterns, and how many
    /// of them Save will show.
    fn render_section(
        &self,
        picker: &FilterPicker,
        which: Which,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let section = picker.section(which)?;
        let t = popup_theme(cx);
        let key = which.key();
        let text_size = px(layout::chrome(layout::TEXT_SM));
        let row_height = px(layout::chrome(layout::CONTROL_HEIGHT_COMPACT));
        let line = |selector: String, ink: crate::theme::color::Srgb, text: String| {
            div()
                .debug_selector(move || selector.clone())
                .px(px(layout::SPACE_SM))
                .py(px(layout::SPACE_XS))
                .text_size(text_size)
                .text_color(ink)
                .child(text)
                .into_any_element()
        };
        let search_text = section.search.read(cx).value();
        let shown = match &section.listing {
            Listing::Loaded(names) => drawn(names, &search_text),
            _ => Vec::new(),
        };
        let status = picker.status_line(which).unwrap_or_default();
        let list = match &section.listing {
            Listing::Loading => line(format!("filter-picker-loading-{key}"), t.text_muted, status),
            Listing::Failed(_) => div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(layout::SPACE_XS))
                .child(line(
                    format!("filter-picker-failed-{key}"),
                    t.danger,
                    status,
                ))
                .child(
                    div()
                        .debug_selector(move || format!("filter-picker-retry-{key}"))
                        .px(px(layout::SPACE_SM))
                        .pb(px(layout::SPACE_XS))
                        .child(
                            button(
                                SharedString::from(format!("filter-picker-retry-button-{key}")),
                                "Retry Listing",
                                Tone::Quiet,
                                Control::Compact,
                                t,
                            )
                            .on_click(cx.listener(
                                move |workspace, _: &gpui::ClickEvent, window, cx| {
                                    workspace.retry_listing(which, window, cx);
                                },
                            )),
                        ),
                )
                .into_any_element(),
            Listing::Loaded(names) if names.is_empty() => {
                line(format!("filter-picker-empty-{key}"), t.text_muted, status)
            }
            Listing::Loaded(_) if shown.is_empty() => line(
                format!("filter-picker-no-match-{key}"),
                t.text_muted,
                picker.no_match_line(which),
            ),
            // ponytail: every row is drawn, not virtualised, so a server
            // listing thousands of names draws thousands of checkboxes.
            // The upgrade is `uniform_list`.
            Listing::Loaded(names) => div()
                .id(SharedString::from(format!("filter-picker-list-{key}")))
                .debug_selector(move || format!("filter-picker-list-{key}"))
                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(shown.iter().map(|(index, ranges)| {
                    let index = *index;
                    let listed = &names[index];
                    let hint = picker.listed_hint(which, listed);
                    let checked = hint.is_some() || section.ticked.contains(listed);
                    let toggled = listed.clone();
                    div()
                        .h(row_height)
                        .px(px(layout::SPACE_SM))
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_XS))
                        .child(
                            div()
                                .debug_selector(move || format!("filter-picker-row-{key}-{index}"))
                                .flex_1()
                                .min_w_0()
                                .child(
                                    Checkbox::new(SharedString::from(format!(
                                        "filter-picker-{key}-{index}"
                                    )))
                                    .w_full()
                                    .text_size(text_size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .accessibility_label(listed.clone())
                                    .checked(checked)
                                    .disabled(hint.is_some())
                                    .on_click(cx.listener(move |workspace, on: &bool, _, cx| {
                                        workspace.tick_name(which, &toggled, *on, cx);
                                    }))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(highlighted(listed.clone(), ranges, t)),
                                    ),
                                ),
                        )
                        .children(hint.or_else(|| picker.kind_note(listed)).map(|hint| {
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                .text_color(t.text_muted)
                                .child(hint)
                        }))
                }))
                .into_any_element(),
        };
        // Select and Clear act on the rows drawn, so they have none to act on
        // until the names are read and the search leaves some.
        let acts = !shown.is_empty();
        let action = |selector: String, label: &'static str, on: bool, cx: &mut Context<Self>| {
            div().debug_selector(move || selector.clone()).child(
                button_inked(
                    SharedString::from(format!("{label}-{key}")),
                    label,
                    Tone::Quiet,
                    Control::Compact,
                    quiet_ink(t, acts),
                )
                .disabled(!acts)
                .on_click(cx.listener(
                    move |workspace, _: &gpui::ClickEvent, _, cx| {
                        workspace.tick_drawn(which, on, cx);
                    },
                )),
            )
        };
        // The actions follow the search box in the tree, so Tab reaches them
        // right after it.
        let search_row = div()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_XS))
            .px(px(layout::SPACE_SM))
            .py(px(layout::SPACE_XS))
            .child(field_box(
                format!("filter-picker-search-{key}"),
                &section.search,
                t,
                row_height,
            ))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(layout::SPACE_XS))
                    .child(action(
                        format!("filter-picker-select-all-{key}"),
                        "Select Names",
                        true,
                        cx,
                    ))
                    .child(action(
                        format!("filter-picker-clear-{key}"),
                        "Clear Names",
                        false,
                        cx,
                    )),
            )
            .into_any_element();
        let patterns_row = div()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_XS))
            .px(px(layout::SPACE_SM))
            .py(px(layout::SPACE_XS))
            .child(field_box(
                format!("filter-picker-patterns-{key}"),
                &section.pattern_field,
                t,
                row_height,
            ))
            .child(
                div()
                    .text_size(text_size)
                    .text_color(t.text_muted)
                    .child("* matches any run of characters and ? matches one. Case is ignored."),
            )
            .into_any_element();
        let caption = match which {
            Which::Databases => None,
            Which::Schemas => picker.schemas_caption(),
            Which::Tables => picker.tables_caption(),
        };
        Some(
            div()
                .debug_selector(move || format!("filter-picker-section-{key}"))
                .flex_1()
                .min_w(px(layout::SETTINGS_TEXT_MIN_WIDTH))
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(section_label(t, which.label()))
                .children(caption.map(|caption| {
                    div()
                        .text_size(text_size)
                        .text_color(t.text_muted)
                        .child(caption)
                }))
                .child(well(t, [search_row, list, patterns_row]))
                .children(picker.preview(which).map(|preview| {
                    div()
                        .debug_selector(move || format!("filter-picker-preview-{key}"))
                        .text_size(text_size)
                        .text_color(t.text_muted)
                        .child(preview)
                }))
                .children(picker.unmatched(which).map(|unmatched| {
                    div()
                        .debug_selector(move || format!("filter-picker-unmatched-{key}"))
                        .text_size(text_size)
                        .text_color(t.text_muted)
                        .child(unmatched)
                }))
                .into_any_element(),
        )
    }

    /// The dialog: a checklist per list over what Save will show of it.
    pub(crate) fn render_filter_picker(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let picker = self.filter_picker_in_front()?;
        let name = self.profile()?.name.clone();
        let t = popup_theme(cx);
        let text_size = px(layout::chrome(layout::TEXT_SM));
        // The layer takes the keyboard for the dialog as it goes up, so the
        // search box takes it from the layer on the frame after.
        if picker.focus_search.replace(false) {
            let search = picker.databases.search.clone();
            cx.on_next_frame(window, move |_, window, cx| {
                search.update(cx, |input, cx| input.focus(window, cx));
            });
        }
        let removable = picker.can_remove();
        let sections: Vec<AnyElement> = [Which::Databases, Which::Schemas, Which::Tables]
            .into_iter()
            .filter_map(|which| self.render_section(picker, which, cx))
            .collect();
        let card = dialog(t).debug_selector(|| "filter-picker".into());
        // Two lists need two columns.
        let card = match picker.schemas {
            Some(_) => card
                .w(px(layout::FILTER_DIALOG_WIDTH))
                .max_w(gpui::relative(0.9)),
            None => card,
        };
        let save = cx.entity().downgrade();
        let discard = save.clone();
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    card.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_XS))
                            .child(section_label(t, picker.heading()))
                            .child(
                                div()
                                    .text_size(text_size)
                                    .text_color(t.text_muted)
                                    .child(name),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(layout::SPACE_LG))
                            .children(sections),
                    )
                    .child(
                        div()
                            .debug_selector(|| "filter-picker-boundary".into())
                            .text_size(text_size)
                            .text_color(t.text_muted)
                            .child(picker.boundary()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .debug_selector(|| "filter-picker-remove".into())
                                    .child(
                                        button_inked(
                                            "filter-picker-remove-button",
                                            "Remove Filter",
                                            Tone::Quiet,
                                            Control::Standard,
                                            quiet_ink(t, removable),
                                        )
                                        .disabled(!removable)
                                        .on_click(
                                            cx.listener(
                                                |workspace, _: &gpui::ClickEvent, window, cx| {
                                                    workspace.remove_picker_filter(window, cx);
                                                },
                                            ),
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(layout::SPACE_SM))
                                    .child(
                                        self.choice(
                                            "filter-picker",
                                            1,
                                            button(
                                                "filter-picker-discard",
                                                "Discard Changes",
                                                Tone::Quiet,
                                                Control::Standard,
                                                t,
                                            )
                                            .on_click(
                                                move |_, _, cx| {
                                                    _ = discard.update(cx, |workspace, cx| {
                                                        workspace.close_filter_picker(cx);
                                                    });
                                                },
                                            ),
                                            t,
                                        ),
                                    )
                                    .child(
                                        self.choice(
                                            "filter-picker",
                                            0,
                                            button(
                                                "filter-picker-save",
                                                "Save Filter",
                                                Tone::Primary,
                                                Control::Standard,
                                                t,
                                            )
                                            .disabled(!picker.can_save())
                                            .on_click(
                                                move |_, _, cx| {
                                                    _ = save.update(cx, |workspace, cx| {
                                                        workspace.save_filter(cx);
                                                    });
                                                },
                                            ),
                                            t,
                                        ),
                                    ),
                            ),
                    ),
                )
                .into_any_element(),
        )
    }
}

/// The scope of a table filter for the schema `schema` of `database` (the
/// connection's own where that is `None`), or for the one schema the database
/// holds where `schema` is `None`. Only where the catalog is loaded and the
/// schema has relations, and where the key's database is known on a shape that
/// keys by one: no list applies to a key that names none.
fn table_scope(
    profile: &Profile,
    database: Option<&str>,
    schema: Option<&str>,
) -> Option<TableScope> {
    if !matches!(profile.state, ProfileState::Connected(_)) {
        return None;
    }
    let own = profile.active_database();
    let elsewhere = database
        .filter(|database| own.as_deref() != Some(database))
        .map(str::to_string);
    let catalog = profile.catalog_of(elsewhere.as_deref())?;
    let schema = match schema {
        Some(schema) => catalog.schemas.iter().find(|found| found.name == schema)?,
        None => catalog.schemas.first()?,
    }
    .name
    .clone();
    if catalog.relations_of(&schema).total() == 0 {
        return None;
    }
    let by_database = matches!(
        profile.config.target().levels(),
        Levels::DatabaseThenSchema | Levels::DatabaseHoldsObjects
    );
    let key = match by_database {
        true => Some(elsewhere.clone().or(own)?),
        false => None,
    };
    Some(TableScope {
        database: key,
        schema,
        elsewhere,
    })
}

/// A text field in a box of its own: the box is the field's edge, so the
/// input draws nothing around itself.
fn field_box(
    selector: String,
    input: &Entity<InputState>,
    t: Theme,
    height: gpui::Pixels,
) -> gpui::Div {
    div()
        .debug_selector(move || selector.clone())
        .h(height)
        .px(px(layout::SPACE_SM))
        .flex()
        .items_center()
        .rounded(px(layout::RADIUS_CONTROL))
        .border_1()
        .border_color(t.border)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Sizable::small(Input::new(input).appearance(false).p_0())),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The picker's own blocking read, off the window, against a server whose
    /// `system.databases` lists more than the tree draws.
    #[test]
    fn the_picker_lists_the_databases_the_clickhouse_tree_draws_not_the_servers_list() {
        use crate::db::Engine;
        use crate::db::clickhouse_mock::{Mock, Response};

        let mock = Mock::start();
        let connection = mock.connection();
        let columns = [("name", "String"), ("is_current", "UInt8")];
        mock.on_statement(
            "SELECT DISTINCT database",
            [Response::rows(
                &columns,
                &[&["analytics", "0"], &["dbdelve_dev", "1"]],
            )],
        );
        mock.on_statement(
            "FROM system.databases",
            [Response::rows(
                &columns,
                &[
                    &["INFORMATION_SCHEMA", "0"],
                    &["analytics", "0"],
                    &["dbdelve_dev", "1"],
                    &["default", "0"],
                    &["information_schema", "0"],
                    &["system", "0"],
                ],
            )],
        );

        let (databases, schemas) =
            read_names_for_picker(connection, Engine::ClickHouse, false, true, false);

        let databases = databases.expect("asked for").expect("should list");
        assert_eq!(databases.names, ["analytics", "dbdelve_dev"]);
        assert_eq!(databases.current.as_deref(), Some("dbdelve_dev"));
        assert!(schemas.is_none());
        assert!(
            mock.requests()
                .iter()
                .all(|request| !request.statement.contains("FROM system.databases"))
        );
    }

    /// The tree draws a schema that holds only a sequence once its routines
    /// are merged in; the picker's list has to hold it too, or there is no row
    /// to tick and the total falls short of the connection row's.
    #[test]
    fn the_picker_lists_a_schema_that_holds_only_a_sequence_as_the_tree_draws_it() {
        use crate::db::{
            CancelToken, Connection, ConnectionConfig, DUCKDB_IN_MEMORY, Engine, Fetch,
        };

        let connection =
            Connection::open(ConnectionConfig::duckdb(DUCKDB_IN_MEMORY.to_string(), 0)).unwrap();
        for statement in [
            "CREATE TABLE t (id INTEGER)",
            "CREATE SCHEMA only_seq",
            "CREATE SEQUENCE only_seq.s",
        ] {
            connection
                .query(statement, &CancelToken::default(), Fetch::default())
                .unwrap();
        }

        let (databases, schemas) =
            read_names_for_picker(connection.clone(), Engine::DuckDb, false, false, true);

        assert!(databases.is_none());
        let listed = schemas.expect("asked for").expect("should list");
        let mut drawn = connection.catalog(&Shown::everything()).unwrap();
        drawn.merge(connection.routines(&Shown::everything()).unwrap());
        let drawn: Vec<String> = drawn
            .schemas
            .into_iter()
            .map(|schema| schema.name)
            .collect();
        assert!(listed.contains(&"main".to_string()));
        assert!(listed.contains(&"only_seq".to_string()));
        assert_eq!(listed, drawn);
    }

    /// Inside the user's transaction neither listing goes on its session: a
    /// failed read there would fail the transaction, so one already failed
    /// stands in for it here and both still list.
    #[test]
    fn the_picker_lists_beside_the_users_transaction() {
        use crate::db::{
            CancelToken, Connection, ConnectionConfig, DUCKDB_IN_MEMORY, Engine, Fetch,
        };

        let connection =
            Connection::open(ConnectionConfig::duckdb(DUCKDB_IN_MEMORY.to_string(), 0)).unwrap();
        let run = |sql| connection.query(sql, &CancelToken::default(), Fetch::default());
        run("CREATE TABLE t (id INTEGER)").unwrap();
        run("BEGIN").unwrap();
        run("SELECT * FROM missing").unwrap_err();
        assert!(run("SELECT 1").is_err(), "the transaction failed");

        let (databases, schemas) =
            read_names_for_picker(connection.clone(), Engine::DuckDb, true, true, true);

        databases.expect("asked for").expect("should list");
        let schemas = schemas.expect("asked for").expect("should list");
        assert!(schemas.contains(&"main".to_string()));
    }

    /// The picker's tick and the filter's hide are one comparison: Greek
    /// capital sigma lowercases to `σ` per character, so `ΑΣ` is not `ας`
    /// (whole-string lowercase would apply the final-sigma rule and say it is).
    #[test]
    fn the_picker_reads_two_names_as_one_exactly_when_the_stored_rule_does() {
        assert_eq!(
            split(&["ΑΣ".into()], &["ας".into()]),
            (vec![], vec!["ΑΣ".to_string()])
        );
        let include = Include {
            databases: vec!["ΑΣ".into()],
            ..Include::default()
        };
        assert!(
            !include
                .resolve(Levels::DatabaseIsSchema, None, None)
                .shows_database("ας")
        );
        assert_eq!(
            split(&["Billing".into()], &["billing".into()]),
            (vec!["billing".to_string()], vec![])
        );
    }

    #[test]
    fn lists_that_differ_from_the_saved_ones_only_in_case_are_unchanged() {
        let include = |databases: &[&str], schemas: &[&str]| Include {
            databases: databases.iter().map(|name| name.to_string()).collect(),
            schemas: schemas.iter().map(|name| name.to_string()).collect(),
            ..Include::default()
        };
        assert!(unchanged(&include(&["e2e"], &[]), &include(&["E2E"], &[])));
        assert!(unchanged(
            &include(&["a", "s*"], &["Public"]),
            &include(&["A", "S*"], &["public"])
        ));
        assert!(!unchanged(
            &include(&["a", "b"], &[]),
            &include(&["b", "a"], &[])
        ));
        assert!(!unchanged(
            &include(&["a"], &[]),
            &include(&["a", "b"], &[])
        ));
        assert!(!unchanged(
            &include(&[], &["main"]),
            &include(&[], &["extra"])
        ));
    }

    #[test]
    fn a_table_list_is_written_for_its_own_key_and_leaves_every_other_list_alone() {
        let mut include = Include::from_stored(vec!["app".into()], vec!["public".into()])
            .with_tables(vec![crate::db::TableFilter {
                database: Some("app".into()),
                schema: "audit".into(),
                tables: vec!["log".into()],
            }]);
        let before = include.clone();
        // What Save writes: the ticked names, then the typed patterns.
        let written = written_entries(&["orders".into()], "tmp_*, ORDERS");
        include.set_tables(Some("app"), "public", written);
        assert_eq!(
            include.tables_of(Some("app"), "public"),
            ["orders", "tmp_*"]
        );
        assert_eq!(include.tables_of(Some("app"), "audit"), ["log"]);
        assert_eq!(include.databases, before.databases);
        assert_eq!(include.schemas, before.schemas);
        // Emptied, the key goes and the schema shows everything again.
        include.set_tables(Some("app"), "public", Vec::new());
        assert_eq!(include, before);
    }

    #[test]
    fn the_table_preview_counts_what_the_list_shows_by_the_stored_rule() {
        let names: Vec<String> = ["junk", "orders", "orders_old", "Users"]
            .map(String::from)
            .to_vec();
        let entries = |list: &[&str]| list.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        assert_eq!(tables_shown(&entries(&["orders"]), &names), 1);
        assert_eq!(tables_shown(&entries(&["orders*", "users"]), &names), 3);
        assert_eq!(tables_shown(&entries(&["nothing"]), &names), 0);
    }

    #[test]
    fn a_table_list_is_unchanged_only_when_it_holds_the_same_names_in_order_ignoring_case() {
        let list = |entries: &[&str]| entries.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        assert!(same_entries(&list(&["Orders"]), &list(&["orders"])));
        assert!(!same_entries(&list(&["a", "b"]), &list(&["b", "a"])));
        assert!(!same_entries(&list(&["a"]), &list(&[])));
    }

    #[test]
    fn only_views_and_the_like_say_what_they_are() {
        assert_eq!(kind_hint(RelationKind::Table), None);
        assert_eq!(kind_hint(RelationKind::PartitionedTable), None);
        assert_eq!(kind_hint(RelationKind::View), Some("view"));
        assert_eq!(kind_hint(RelationKind::Dictionary), Some("dictionary"));
    }

    #[test]
    fn only_a_database_then_schema_target_lists_schemas() {
        assert!(lists_schemas(Levels::DatabaseThenSchema));
        assert!(!lists_schemas(Levels::DatabaseIsSchema));
        assert!(!lists_schemas(Levels::DatabaseHoldsObjects));
        assert!(!lists_schemas(Levels::Flat));
    }

    #[test]
    fn names_then_patterns_are_written_once_each_ignoring_case() {
        assert_eq!(
            written_entries(&["other".into()], " OTHER, sc*, , SC* ,tmp_"),
            ["other", "sc*", "tmp_"]
        );
    }

    #[test]
    fn the_unmatched_line_names_three_patterns_then_how_many_more() {
        let names = ["alpha".to_string(), "beta".to_string()];
        let patterns = |entries: &[&str]| entries.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(
            unmatched_line("database", &patterns(&["al*", "?eta"]), &names),
            None
        );
        assert_eq!(
            unmatched_line("database", &patterns(&["a*", "al*"]), &names),
            None,
            "a pattern is unmatched only when no listed name fits it"
        );
        assert_eq!(
            unmatched_line("database", &patterns(&["z*"]), &names).as_deref(),
            Some("No database matches: z*")
        );
        assert_eq!(
            unmatched_line("schema in e2e", &patterns(&["z*", "y?"]), &names).as_deref(),
            Some("No schema in e2e matches: z*, y?")
        );
        assert_eq!(
            unmatched_line(
                "database",
                &patterns(&["a1", "b1", "c1", "d1", "e1"]),
                &names
            )
            .as_deref(),
            Some("No database matches: a1, b1, c1 and 2 more")
        );
    }
}
