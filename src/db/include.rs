//! A profile's include lists: which databases of a server the connections tree
//! shows.
//!
//! A list is exact names and `*`/`?` globs, matched here in Rust against the
//! names the server listed. Nothing a person typed ever reaches a statement:
//! only a server-listed name that matched can, and an entry is never turned
//! into `LIKE`. An empty list shows everything. The connection's own database
//! is always shown, whatever the list says, so a filter cannot hide the place
//! the session is in.
//!
//! The lists are narrowed where an answer is read ([`Connection::databases`]
//! takes a [`Shown`]), so nothing stores an unnarrowed answer.
//!
//! [`Connection::databases`]: super::Connection::databases

use std::collections::HashMap;

use super::{Catalog, Databases, Engine, Levels, Relation, RelationKind, Schema, Sizes};

/// How many entries a list keeps. A file is a person's to edit: no more is
/// read than a profile would write.
pub const ENTRIES_CAP: usize = 500;

/// One schema's table list: the relations of `schema` the tree shows, as exact
/// names and `*`/`?` globs, matched like the other lists. A schema with no
/// entry shows every relation.
///
/// `database` is the database that holds the schema by name, where the tree
/// draws databases over schemas (`Levels::DatabaseThenSchema` and
/// `DatabaseHoldsObjects`). It is `None` where there is no database level
/// (`Levels::Flat`) or where the catalog's schemas are the databases
/// (`Levels::DatabaseIsSchema`): there `schema` is the database's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableFilter {
    pub database: Option<String>,
    pub schema: String,
    pub tables: Vec<String>,
}

impl TableFilter {
    /// Whether `self` is the list of `database` and `schema`, which are
    /// compared as the filter compares every name.
    fn is_for(&self, database: Option<&str>, schema: &str) -> bool {
        let same_database = match (&self.database, database) {
            (Some(mine), Some(theirs)) => same_name(mine, theirs),
            (None, None) => true,
            _ => false,
        };
        same_database && same_name(&self.schema, schema)
    }
}

/// What a profile's lists say, as written in `profiles.toml` and in the form.
/// Not part of the connection: changing it never reconnects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Include {
    pub databases: Vec<String>,
    pub schemas: Vec<String>,
    /// The per-schema table lists. Kept apart from the two above so the
    /// call sites that build those two stay as they were; see
    /// [`Include::with_tables`].
    pub tables: Vec<TableFilter>,
}

/// `list` as a file is read: each entry trimmed, those left empty dropped, at
/// most [`ENTRIES_CAP`] kept, in the order written.
fn kept_entries(list: Vec<String>) -> Vec<String> {
    list.into_iter()
        // ponytail: a name the server lists with leading or trailing
        // whitespace is matched by no exact entry once trimmed, since
        // matching runs on the server's own spelling; a glob such as
        // `*name*` still covers it. The upgrade is an entry spelling
        // that keeps edge whitespace (a quoted name), read here and
        // written by the picker.
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .take(ENTRIES_CAP)
        .collect()
}

impl Include {
    /// What a file held, or what Save is about to write: each entry trimmed,
    /// those left empty dropped, at most [`ENTRIES_CAP`] of each list kept, in
    /// the order written. The table lists start empty; [`Self::with_tables`]
    /// sets them.
    pub fn from_stored(databases: Vec<String>, schemas: Vec<String>) -> Self {
        Self {
            databases: kept_entries(databases),
            schemas: kept_entries(schemas),
            tables: Vec::new(),
        }
    }

    /// `self` with the table lists `lists`, normalised as a file is read: each
    /// key and entry trimmed, an entry left with no schema or no tables
    /// dropped, two lists of one key (names compared as the filter does)
    /// joined into the first, at most [`ENTRIES_CAP`] lists and as many
    /// tables in each, in the order written.
    pub fn with_tables(mut self, lists: Vec<TableFilter>) -> Self {
        self.tables = Vec::new();
        for list in lists {
            self.put_tables(list.database.as_deref(), &list.schema, list.tables, true);
        }
        self
    }

    /// The table list of `schema` in `database` (see [`TableFilter`] for when
    /// that is `None`), empty when it has none.
    pub fn tables_of(&self, database: Option<&str>, schema: &str) -> &[String] {
        self.tables
            .iter()
            .find(|list| list.is_for(database, schema))
            .map_or(&[], |list| list.tables.as_slice())
    }

    /// Replace the table list of one schema with `tables`, normalised as
    /// [`Self::with_tables`] does; an empty list removes it, so the schema
    /// shows every relation again. Every other list is left as it was.
    pub fn set_tables(&mut self, database: Option<&str>, schema: &str, tables: Vec<String>) {
        let database = database
            .map(str::trim)
            .filter(|database| !database.is_empty());
        let schema = schema.trim();
        let at = self
            .tables
            .iter()
            .position(|list| list.is_for(database, schema));
        let tables = kept_entries(tables);
        match (at, tables.is_empty()) {
            (Some(at), true) => {
                self.tables.remove(at);
            }
            (Some(at), false) => self.tables[at].tables = tables,
            (None, _) => self.put_tables(database, schema, tables, false),
        }
    }

    /// Add one list in normalised form, joining it to the list of the same
    /// key when `join` and there is one.
    fn put_tables(
        &mut self,
        database: Option<&str>,
        schema: &str,
        tables: Vec<String>,
        join: bool,
    ) {
        let database = database
            .map(str::trim)
            .filter(|database| !database.is_empty());
        let schema = schema.trim();
        let tables = kept_entries(tables);
        if schema.is_empty() || tables.is_empty() {
            return;
        }
        if let Some(existing) = self
            .tables
            .iter_mut()
            .find(|list| list.is_for(database, schema))
        {
            if join {
                existing.tables.extend(tables);
                existing.tables.truncate(ENTRIES_CAP);
            }
        } else if self.tables.len() < ENTRIES_CAP {
            self.tables.push(TableFilter {
                database: database.map(str::to_string),
                schema: schema.to_string(),
                tables,
            });
        }
    }

    /// The lists as they apply to a target of shape `levels` whose session is
    /// on `own_database` and `active_schema`. A list the shape does not use is
    /// kept on disk and ignored here, and a shape with no databases to choose
    /// between has none of the database and schema lists applied. The table
    /// lists apply on every shape, keyed as [`TableFilter`] says.
    pub fn resolve(
        &self,
        levels: Levels,
        own_database: Option<&str>,
        active_schema: Option<&str>,
    ) -> Shown {
        let entries = |list: &[String]| {
            let entries = list
                .iter()
                .filter(|entry| !entry.trim().is_empty())
                .cloned()
                .collect::<Vec<_>>();
            (!entries.is_empty()).then_some(entries)
        };
        // Where the tree draws a database over its schemas, a table list is
        // the one of its database; elsewhere it is the one with no database.
        let by_database = matches!(
            levels,
            Levels::DatabaseThenSchema | Levels::DatabaseHoldsObjects
        );
        let tables = self
            .tables
            .iter()
            .filter(|list| list.database.is_some() == by_database)
            .cloned()
            .collect();
        let tables = Shown {
            tables,
            table_database: own_database.filter(|_| by_database).map(str::to_string),
            tables_by_database: by_database,
            ..Shown::default()
        };
        match levels {
            Levels::Flat => tables,
            Levels::DatabaseIsSchema | Levels::DatabaseHoldsObjects => Shown {
                databases: entries(&self.databases),
                own_database: own_database.map(str::to_string),
                schemas_are_databases: levels == Levels::DatabaseIsSchema,
                ..tables
            },
            Levels::DatabaseThenSchema => Shown {
                databases: entries(&self.databases),
                own_database: own_database.map(str::to_string),
                schemas: entries(&self.schemas),
                active_schema: active_schema.map(str::to_string),
                ..tables
            },
        }
    }
}

/// The lists resolved for one target, owned so it moves into the closure that
/// reads on a background thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    /// The entries a database must match, or none for every database.
    databases: Option<Vec<String>>,
    /// The database the session is on, which is shown whatever matches.
    own_database: Option<String>,
    /// The entries a schema of a catalog must match, or none for every
    /// schema. Unused where the catalog's schemas are databases.
    schemas: Option<Vec<String>>,
    /// The schema the session is in, which is shown whatever matches, in the
    /// connection's own database only.
    active_schema: Option<String>,
    /// Whether the catalog's schemas are the server's databases, so the
    /// databases list narrows it.
    schemas_are_databases: bool,
    /// The table lists that apply to the catalogs this narrows: those with a
    /// database where the tree draws one over its schemas, else those with
    /// none.
    tables: Vec<TableFilter>,
    /// The database whose catalog is narrowed, which picks among `tables`
    /// where they are keyed by database. The session's own until
    /// [`Shown::in_another_database`] says another.
    table_database: Option<String>,
    /// Whether `tables` are keyed by database, as `Include::resolve` read the
    /// shape.
    tables_by_database: bool,
}

impl Shown {
    /// No narrowing: what a read that wants the whole server asks for, and
    /// what a target with nothing to filter resolves to.
    pub fn everything() -> Self {
        Self::default()
    }

    pub fn shows_database(&self, name: &str) -> bool {
        let Some(entries) = &self.databases else {
            return true;
        };
        self.is_own_database(name) || entries.iter().any(|entry| matches(entry, name))
    }

    fn is_own_database(&self, name: &str) -> bool {
        self.own_database
            .as_deref()
            .is_some_and(|own| same_name(own, name))
    }

    /// Whether a schema of a catalog is shown: by the databases list where the
    /// catalog's schemas are databases, else by the schemas list, with the
    /// active schema kept whatever it says.
    pub fn shows_schema(&self, name: &str) -> bool {
        if self.schemas_are_databases {
            return self.shows_database(name);
        }
        let Some(entries) = &self.schemas else {
            return true;
        };
        self.active_schema
            .as_deref()
            .is_some_and(|active| same_name(active, name))
            || entries.iter().any(|entry| matches(entry, name))
    }

    /// Whether an object in `schema` is listed: of the own database where
    /// `elsewhere` is `None`, else of that other database, which has to be
    /// shown itself and whose schemas are matched without the active one. With
    /// a `relation` named, the schema's table list has to admit it too. A
    /// tab or a shortcut on an object this does not list is hidden, not gone.
    ///
    /// ponytail: a partition is matched by its own name, since a tab or a pin
    /// does not know its parent; one whose parent the list admits but whose
    /// own name it does not reads as hidden, and a hidden relation still
    /// opens by name. The upgrade is a parent in the stored object.
    pub fn lists_object(
        &self,
        elsewhere: Option<&str>,
        schema: &str,
        relation: Option<&str>,
    ) -> bool {
        let there;
        let view = match elsewhere {
            None => self,
            Some(database) => {
                if !self.shows_database(database) {
                    return false;
                }
                there = self.in_another_database(database);
                &there
            }
        };
        view.shows_schema(schema) && relation.is_none_or(|name| view.shows_relation(schema, name))
    }

    /// What this shows of the catalog another database's session read, which
    /// is `database`'s: the same lists, with no active schema, which is the
    /// own database's, and the table lists of `database`.
    pub fn in_another_database(&self, database: &str) -> Self {
        Self {
            active_schema: None,
            table_database: Some(database.to_string()),
            ..self.clone()
        }
    }

    /// The table list of the catalog's schema `schema`, none for every
    /// relation.
    fn table_entries(&self, schema: &str) -> Option<&[String]> {
        let database = self
            .tables_by_database
            .then_some(self.table_database.as_deref())
            .flatten();
        // Keyed by database and the database is not known: no list applies.
        if self.tables_by_database && database.is_none() {
            return None;
        }
        self.tables
            .iter()
            .find(|list| list.is_for(database, schema))
            .map(|list| list.tables.as_slice())
    }

    /// Whether the relation `name` of `schema` is shown by the schema's table
    /// list, true when it has none.
    pub fn shows_relation(&self, schema: &str, name: &str) -> bool {
        self.table_entries(schema)
            .is_none_or(|entries| entries.iter().any(|entry| matches(entry, name)))
    }

    /// Drop from `schema` the relations its table list leaves out and note the
    /// ones it names, in `hidden`. A partition goes with its parent and is
    /// neither matched nor noted: only the relations that stand alone are, as
    /// the tree counts them.
    fn narrow_relations(
        &self,
        schema: &mut Schema,
        hidden: &mut HashMap<String, Vec<(String, RelationKind)>>,
    ) {
        let Some(entries) = self.table_entries(&schema.name) else {
            return;
        };
        let admits = |name: &str| entries.iter().any(|entry| matches(entry, name));
        // A parent the catalog did not list leaves its partition standing
        // alone, as the tree draws it.
        let by_name: HashMap<&str, usize> = schema
            .relations
            .iter()
            .enumerate()
            .map(|(at, relation)| (relation.name.as_str(), at))
            .collect();
        let root = |mut at: usize| {
            // Bounded, so a hand-made catalog with a cycle ends.
            for _ in 0..schema.relations.len() {
                match schema.relations[at]
                    .partition_of
                    .as_deref()
                    .and_then(|parent| by_name.get(parent))
                {
                    Some(&parent) if parent != at => at = parent,
                    _ => break,
                }
            }
            at
        };
        let keeps: Vec<bool> = (0..schema.relations.len())
            .map(|at| admits(&schema.relations[root(at)].name))
            .collect();
        let standing: Vec<bool> = (0..schema.relations.len())
            .map(|at| root(at) == at)
            .collect();
        let dropped: Vec<(String, RelationKind)> = schema
            .relations
            .iter()
            .enumerate()
            .filter(|(at, _)| !keeps[*at] && standing[*at])
            .map(|(_, relation): (usize, &Relation)| (relation.name.clone(), relation.kind))
            .collect();
        let mut at = 0;
        schema.relations.retain(|_| {
            at += 1;
            keeps[at - 1]
        });
        if !dropped.is_empty() {
            hidden
                .entry(schema.name.clone())
                .or_default()
                .extend(dropped);
        }
    }

    /// `catalog` without the schemas this does not show, in the order they
    /// were read, and how many that was added to `left_out`, so a count a
    /// read already made is kept.
    ///
    /// ponytail: the whole catalog is read and the schemas dropped after. The
    /// ceiling is a very large single database, which still reads every
    /// schema; the upgrade is a per-engine schema-names read pushed into the
    /// catalog SQL.
    ///
    /// The relations a schema's table list leaves out are dropped from it too,
    /// and their names and kinds kept in [`Catalog::hidden_relations`].
    pub fn narrow_catalog(&self, mut catalog: Catalog) -> Catalog {
        let read = catalog.schemas.len();
        catalog
            .schemas
            .retain(|schema| self.shows_schema(&schema.name));
        catalog.left_out += read - catalog.schemas.len();
        for schema in &mut catalog.schemas {
            self.narrow_relations(schema, &mut catalog.hidden_relations);
        }
        catalog
    }

    /// `routines` without the schemas this does not show, counting none: the
    /// relations' read has counted them, and a schema holding only a routine
    /// or a sequence must not come back through [`Catalog::merge`].
    pub fn narrow_routines(&self, mut routines: Catalog) -> Catalog {
        routines
            .schemas
            .retain(|schema| self.shows_schema(&schema.name));
        routines
    }

    /// `sizes` without the schemas this does not show.
    pub fn narrow_sizes(&self, mut sizes: Sizes) -> Sizes {
        sizes.retain(|schema, _| self.shows_schema(schema));
        sizes
    }

    /// Whether a list narrows what a catalog read returns, so a read with none
    /// need not list the databases first.
    pub fn narrows_catalog(&self) -> bool {
        if self.schemas_are_databases {
            self.databases.is_some()
        } else {
            self.schemas.is_some()
        }
    }

    /// Where one catalog read spans every database, the names of `listed`
    /// that read may ask about, and how many it need not: `None` when no list
    /// narrows the catalog, so the read stays exactly as it was. Otherwise the
    /// non-system names this shows, and the own database even when it is a
    /// system one, as the unfiltered read keeps it, in the server's order,
    /// each listed spelling once, and how many non-system names it hid. The
    /// names are the server's own, never the pattern text, so they are all a
    /// statement ever carries. An empty list means nothing matched, which is
    /// the caller's cue to send no statement.
    ///
    /// Both numbers are drawn from `listed`, whichever listing the caller
    /// passes. ClickHouse's and MySQL's `catalog` pass `catalog_databases`, the
    /// databases the catalog draws, so a database holding nothing the catalog
    /// reads is in neither number, as it is in no unfiltered count, and the
    /// shown and the hidden add up to the unfiltered tree's count. MySQL's
    /// `matched`, which `routines`, `sequences` and `sizes` call, passes the
    /// server's whole `SCHEMATA` listing and uses only the names, never the
    /// hidden count.
    pub fn matched_databases(
        &self,
        listed: &Databases,
        engine: Engine,
    ) -> Option<(Vec<String>, usize)> {
        if !self.narrows_catalog() {
            return None;
        }
        let (shown, left_out): (Vec<_>, Vec<_>) = listed
            .names
            .iter()
            .filter(|name| !engine.is_system_database(name) || self.is_own_database(name))
            .partition(|name| self.shows_schema(name));
        Some((shown.into_iter().cloned().collect(), left_out.len()))
    }

    /// `databases` with the names this does not show taken out, in the order
    /// the server listed them, and how many that was recorded in `left_out`.
    /// `current` is the session's own and never narrowed.
    pub fn narrow_databases(&self, mut databases: Databases) -> Databases {
        let listed = databases.names.len();
        databases.names.retain(|name| self.shows_database(name));
        databases.left_out = listed - databases.names.len();
        databases
    }
}

fn lowered(text: &str) -> impl Iterator<Item = char> {
    text.chars().flat_map(char::to_lowercase)
}

/// Whether two names are one as the filter reads them, ignoring case by
/// Unicode simple lowercase of each character.
pub(crate) fn same_name(a: &str, b: &str) -> bool {
    lowered(a).eq(lowered(b))
}

/// Whether `name` is `pattern`: exact, or with `*` for any run of characters
/// (none included) and `?` for any one. Case is ignored by Unicode simple
/// lowercase of each character, and nothing else is normalized: a name is
/// compared as the server lists it. `_` and `%` are plain characters. An empty
/// pattern matches nothing.
///
/// Iterative, remembering only the last `*` and what it has eaten, so a
/// hand-edited file cannot make it exponential.
///
/// ponytail: a database whose real name holds `*` or `?` reads as a pattern.
/// The ceiling is that such a name cannot be ticked alone; the upgrade is an
/// escape.
pub fn matches(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let pattern: Vec<char> = lowered(pattern).collect();
    let name: Vec<char> = lowered(name).collect();
    let (mut at, mut on) = (0, 0);
    // Where the last `*` was and how much of the name it has eaten.
    let mut resume: Option<(usize, usize)> = None;
    while on < name.len() {
        match pattern.get(at) {
            Some('*') => {
                resume = Some((at, on));
                at += 1;
            }
            Some('?') => {
                at += 1;
                on += 1;
            }
            Some(&literal) if literal == name[on] => {
                at += 1;
                on += 1;
            }
            _ => match resume {
                Some((star, eaten)) => {
                    at = star + 1;
                    on = eaten + 1;
                    resume = Some((star, eaten + 1));
                }
                None => return false,
            },
        }
    }
    pattern[at..].iter().all(|&rest| rest == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Routine, RoutineKind, Schema};

    fn schema(name: &str) -> Schema {
        Schema {
            name: name.into(),
            relations: Vec::new(),
            routines: Vec::new(),
            sequences: Vec::new(),
        }
    }

    fn catalog(names: &[&str]) -> Catalog {
        Catalog {
            schemas: names.iter().map(|name| schema(name)).collect(),
            ..Catalog::default()
        }
    }

    fn names(catalog: &Catalog) -> Vec<&str> {
        catalog.schemas.iter().map(|s| s.name.as_str()).collect()
    }

    fn listed(names: &[&str], current: Option<&str>) -> Databases {
        Databases {
            names: names.iter().map(|name| (*name).to_string()).collect(),
            current: current.map(str::to_string),
            left_out: 0,
        }
    }

    fn text(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    const FILTERED: [Levels; 3] = [
        Levels::DatabaseIsSchema,
        Levels::DatabaseThenSchema,
        Levels::DatabaseHoldsObjects,
    ];

    #[test]
    fn a_star_matches_any_run_and_a_question_mark_one_character() {
        for name in ["a", "ab", "", "dbdelve_dev"] {
            assert!(matches("*", name), "{name:?}");
        }
        assert!(matches("a*b", "ab"));
        assert!(matches("a*b", "axyb"));
        assert!(!matches("a*b", "abx"));
        assert!(matches("db?", "db1"));
        assert!(!matches("db?", "db"));
        assert!(!matches("db?", "db12"));
        assert!(matches("?", "x"));
    }

    #[test]
    fn an_empty_pattern_matches_nothing_and_underscore_and_percent_are_literal() {
        assert!(!matches("", "a"));
        assert!(!matches("", ""));
        assert!(matches("a_b", "a_b"));
        assert!(!matches("a_b", "axb"));
        assert!(matches("a%", "a%"));
        assert!(!matches("a%", "ab"));
    }

    #[test]
    fn matching_ignores_case_by_unicode_lowercase() {
        assert!(matches("PROD*", "prod_eu"));
        assert!(matches("prod_*", "PROD_EU"));
        assert!(matches("Ä*", "ä_db"));
        assert!(matches("snowflake_db", "SNOWFLAKE_DB"));
    }

    #[test]
    fn whether_a_name_is_shown_does_not_depend_on_entry_order() {
        let entries = ["a*", "b", "?c"];
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let answers = |order: [usize; 3]| {
            let include = Include::from_stored(
                order.iter().map(|at| entries[*at].to_string()).collect(),
                Vec::new(),
            );
            let shown = include.resolve(Levels::DatabaseThenSchema, None, None);
            ["ab", "b", "xc", "zz"].map(|name| shown.shows_database(name))
        };
        let first = answers(orders[0]);
        assert_eq!(first, [true, true, true, false]);
        for order in orders {
            assert_eq!(answers(order), first, "{order:?}");
        }
    }

    #[test]
    fn a_pathological_pattern_answers_without_blowing_up() {
        assert!(!matches("*a*a*a*a*a*a*a*a*a*a*b", &"a".repeat(10_000)));
    }

    #[test]
    fn an_absent_empty_or_blank_list_shows_everything() {
        let names = ["a", "b", "mysql"];
        for include in [
            Include::from_stored(Vec::new(), Vec::new()),
            Include::from_stored(text(&["", "  "]), Vec::new()),
            Include::default(),
        ] {
            for levels in FILTERED {
                let narrowed = include
                    .resolve(levels, None, None)
                    .narrow_databases(listed(&names, None));
                assert_eq!(narrowed.names, names, "{levels:?}");
                assert_eq!(narrowed.left_out, 0, "{levels:?}");
            }
        }
    }

    #[test]
    fn a_stored_list_drops_blank_entries_and_keeps_at_most_the_cap() {
        let kept = Include::from_stored(text(&["b", "", "a*", "   ", "c"]), Vec::new());
        assert_eq!(kept.databases, ["b", "a*", "c"]);

        let many = (0..ENTRIES_CAP + 7)
            .map(|at| format!("db{at}"))
            .collect::<Vec<_>>();
        let capped = Include::from_stored(many.clone(), many.clone());
        assert_eq!(capped.databases.len(), ENTRIES_CAP);
        assert_eq!(capped.schemas.len(), ENTRIES_CAP);
        assert_eq!(capped.databases, many[..ENTRIES_CAP]);
    }

    #[test]
    fn a_stored_entry_is_trimmed_so_a_padded_one_matches() {
        let include =
            Include::from_stored(text(&["  prod  ", " \t ", "app*\t"]), text(&[" public"]));
        assert_eq!(include.databases, ["prod", "app*"]);
        assert_eq!(include.schemas, ["public"]);

        let shown = include.resolve(Levels::DatabaseThenSchema, None, None);
        assert!(shown.shows_database("PROD"));
        assert!(shown.shows_database("app1"));
        assert!(shown.shows_schema("public"));
    }

    #[test]
    fn each_level_filters_the_list_it_owns() {
        let include = Include::from_stored(text(&["a*"]), text(&["s"]));
        for levels in FILTERED {
            let narrowed = include
                .resolve(levels, None, None)
                .narrow_databases(listed(&["a1", "b1"], None));
            assert_eq!(narrowed.names, ["a1"], "{levels:?}");
        }
        assert_eq!(
            include.resolve(Levels::Flat, None, None),
            Shown::everything(),
            "nothing to choose between, so nothing is filtered"
        );

        let schemas_only = Include::from_stored(Vec::new(), text(&["s"]));
        for levels in FILTERED {
            let narrowed = schemas_only
                .resolve(levels, None, None)
                .narrow_databases(listed(&["a1", "b1"], None));
            assert_eq!(narrowed.names, ["a1", "b1"], "{levels:?}");
        }
    }

    #[test]
    fn the_connections_own_database_is_always_shown_and_counted_as_shown() {
        let include = Include::from_stored(text(&["nothing*"]), Vec::new());
        let narrowed = include
            .resolve(Levels::DatabaseThenSchema, Some("Own"), None)
            .narrow_databases(listed(&["a", "own", "b"], Some("own")));
        assert_eq!(narrowed.names, ["own"]);
        assert_eq!(narrowed.left_out, 2);
        assert_eq!(narrowed.current.as_deref(), Some("own"));

        let narrowed = include
            .resolve(Levels::DatabaseThenSchema, None, None)
            .narrow_databases(listed(&["a", "own", "b"], None));
        assert!(narrowed.names.is_empty());
        assert_eq!(narrowed.left_out, 3);
    }

    #[test]
    fn a_catalog_drops_the_schemas_the_filter_hides_and_counts_each_once() {
        let include = Include::from_stored(Vec::new(), text(&["s*"]));
        let shown = include.resolve(Levels::DatabaseThenSchema, None, Some("public"));

        let narrowed = shown.narrow_catalog(catalog(&["public", "s1", "t1", "t2"]));
        assert_eq!(names(&narrowed), ["public", "s1"]);
        assert_eq!(narrowed.left_out, 2);

        let again = shown.narrow_catalog(narrowed);
        assert_eq!(names(&again), ["public", "s1"]);
        assert_eq!(again.left_out, 2, "nothing more was dropped");

        let mut arriving = catalog(&["s1", "t1"]);
        arriving.left_out = 3;
        assert_eq!(shown.narrow_catalog(arriving).left_out, 4);
    }

    #[test]
    fn routines_of_a_hidden_schema_are_dropped_before_they_merge() {
        let include = Include::from_stored(Vec::new(), text(&["main"]));
        let shown = include.resolve(Levels::DatabaseThenSchema, None, None);
        let mut relations = shown.narrow_catalog(catalog(&["main"]));

        let mut routines = catalog(&["main", "extra"]);
        routines.schemas[0].routines.push(Routine {
            name: "f".into(),
            kind: RoutineKind::Function,
            identity_arguments: String::new(),
            result_type: String::new(),
            language: String::new(),
            definition: String::new(),
        });
        routines.schemas[1].sequences.push("s".into());
        let narrowed = shown.narrow_routines(routines);
        assert_eq!(narrowed.left_out, 0, "the routines step counts nothing");

        relations.merge(narrowed);
        assert_eq!(names(&relations), ["main"]);
        assert_eq!(relations.schemas[0].routines.len(), 1);
        assert_eq!(relations.left_out, 0);
    }

    #[test]
    fn where_schemas_are_databases_the_database_list_narrows_the_catalog() {
        let include = Include::from_stored(text(&["app*"]), text(&["x"]));
        let shown = include.resolve(Levels::DatabaseIsSchema, Some("own"), None);
        let narrowed = shown.narrow_catalog(catalog(&["app1", "own", "other"]));
        assert_eq!(names(&narrowed), ["app1", "own"]);
        assert_eq!(narrowed.left_out, 1, "the schemas list is ignored here");

        let shown = include.resolve(Levels::DatabaseHoldsObjects, Some("own"), None);
        let kept = shown.narrow_catalog(catalog(&["other"]));
        assert_eq!(names(&kept), ["other"]);
        assert_eq!(kept.left_out, 0);
    }

    #[test]
    fn matching_server_listed_databases_leaves_out_the_system_ones_and_counts_the_rest() {
        let server = [
            "dbdelve_archive",
            "dbdelve_dev",
            "information_schema",
            "mysql",
            "performance_schema",
            "sys",
        ];
        let shown = |entries: &[&str], own: Option<&str>| {
            Include::from_stored(text(entries), Vec::new()).resolve(
                Levels::DatabaseIsSchema,
                own,
                None,
            )
        };
        let engine = Engine::MySql;

        assert_eq!(
            shown(&["dbdelve_arch*"], None).matched_databases(&listed(&server, None), engine),
            Some((text(&["dbdelve_archive"]), 1))
        );
        assert_eq!(
            shown(&["dbdelve_arch*"], Some("dbdelve_dev"))
                .matched_databases(&listed(&server, Some("dbdelve_dev")), engine),
            Some((text(&["dbdelve_archive", "dbdelve_dev"]), 0))
        );
        assert_eq!(
            shown(&["no_such*"], None).matched_databases(&listed(&server, None), engine),
            Some((Vec::new(), 2))
        );
        assert_eq!(
            shown(&[], None).matched_databases(&listed(&server, None), engine),
            None
        );
        assert_eq!(
            shown(&["ab"], None).matched_databases(&listed(&["Ab", "ab", "Zed"], None), engine),
            Some((text(&["Ab", "ab"]), 1)),
            "each listed spelling, in the server's order"
        );
        assert_eq!(
            shown(&["dbdelve_arch*"], Some("mysql"))
                .matched_databases(&listed(&server, Some("mysql")), engine),
            Some((text(&["dbdelve_archive", "mysql"]), 1)),
            "a system database the session is on is still its own"
        );
    }

    #[test]
    fn another_database_keeps_no_active_schema() {
        let include = Include::from_stored(Vec::new(), text(&["x"]));
        let shown = include.resolve(Levels::DatabaseThenSchema, Some("db"), Some("main"));
        assert_eq!(
            names(&shown.narrow_catalog(catalog(&["main", "x"]))),
            ["main", "x"]
        );

        let other = shown
            .in_another_database("db")
            .narrow_catalog(catalog(&["main", "x"]));
        assert_eq!(names(&other), ["x"]);
        assert_eq!(other.left_out, 1);
    }

    #[test]
    fn an_object_is_listed_by_its_schema_and_elsewhere_by_its_database_too() {
        let include = Include::from_stored(text(&["app*"]), text(&["public"]));
        let shown = include.resolve(Levels::DatabaseThenSchema, Some("db"), Some("main"));

        assert!(shown.lists_object(None, "public", None));
        assert!(shown.lists_object(None, "main", None), "the active schema");
        assert!(!shown.lists_object(None, "audit", None));
        assert!(shown.lists_object(Some("app1"), "public", None));
        assert!(
            !shown.lists_object(Some("app1"), "main", None),
            "active only at home"
        );
        assert!(!shown.lists_object(Some("other"), "public", None));

        let mysql = Include::from_stored(text(&["app*"]), Vec::new()).resolve(
            Levels::DatabaseIsSchema,
            Some("db"),
            None,
        );
        assert!(mysql.lists_object(None, "app1", None));
        assert!(mysql.lists_object(None, "db", None));
        assert!(!mysql.lists_object(None, "reporting", None));
        assert!(Shown::everything().lists_object(Some("any"), "thing", None));
    }

    #[test]
    fn a_filter_that_matches_nothing_still_keeps_the_own_database_and_the_active_schema() {
        let include = Include::from_stored(text(&["zzz"]), text(&["zzz"]));
        let shown = include.resolve(Levels::DatabaseThenSchema, Some("db"), Some("main"));

        let databases = shown.narrow_databases(listed(&["db", "a", "b"], Some("db")));
        assert_eq!(databases.names, ["db"]);
        assert_eq!(databases.left_out, 2, "1 of 3");

        let narrowed = shown.narrow_catalog(catalog(&["main", "s"]));
        assert_eq!(names(&narrowed), ["main"]);
        assert_eq!(narrowed.left_out, 1, "1 of 2");
    }

    fn table_list(database: Option<&str>, schema: &str, tables: &[&str]) -> TableFilter {
        TableFilter {
            database: database.map(str::to_string),
            schema: schema.into(),
            tables: text(tables),
        }
    }

    fn relation(name: &str, kind: RelationKind, partition_of: Option<&str>) -> Relation {
        Relation {
            name: name.into(),
            kind,
            partition_of: partition_of.map(str::to_string),
            size: None,
            rows: None,
        }
    }

    fn table(name: &str) -> Relation {
        relation(name, RelationKind::Table, None)
    }

    fn catalog_of(schema_name: &str, relations: Vec<Relation>) -> Catalog {
        let mut one = schema(schema_name);
        one.relations = relations;
        Catalog {
            schemas: vec![one],
            ..Catalog::default()
        }
    }

    fn relation_names(catalog: &Catalog, schema_name: &str) -> Vec<String> {
        catalog
            .schemas
            .iter()
            .find(|found| found.name == schema_name)
            .map(|found| found.relations.iter().map(|r| r.name.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_stored_table_list_is_trimmed_joined_by_key_and_dropped_when_it_names_nothing() {
        let include = Include::default().with_tables(vec![
            table_list(Some(" app "), " Public ", &["  users ", "", "audit_*"]),
            table_list(Some("APP"), "public", &["orders"]),
            table_list(None, "   ", &["ignored"]),
            table_list(None, "main", &[" ", ""]),
            table_list(Some("  "), "main", &["t?"]),
        ]);
        assert_eq!(
            include.tables,
            [
                table_list(Some("app"), "Public", &["users", "audit_*", "orders"]),
                table_list(None, "main", &["t?"]),
            ],
            "a blank database is none, and the lists of one key join into the first"
        );

        let many: Vec<String> = (0..ENTRIES_CAP + 5).map(|at| format!("t{at}")).collect();
        let capped = Include::default().with_tables(vec![TableFilter {
            database: None,
            schema: "s".into(),
            tables: many,
        }]);
        assert_eq!(capped.tables[0].tables.len(), ENTRIES_CAP);

        let lists: Vec<TableFilter> = (0..ENTRIES_CAP + 5)
            .map(|at| table_list(None, &format!("s{at}"), &["t"]))
            .collect();
        assert_eq!(
            Include::default().with_tables(lists).tables.len(),
            ENTRIES_CAP
        );
    }

    #[test]
    fn one_schemas_table_list_is_replaced_or_removed_without_touching_the_others() {
        let mut include = Include::from_stored(text(&["db"]), Vec::new()).with_tables(vec![
            table_list(Some("db"), "public", &["a"]),
            table_list(Some("db"), "audit", &["b"]),
        ]);
        include.set_tables(Some("DB"), "PUBLIC", text(&[" x* ", ""]));
        assert_eq!(include.tables_of(Some("db"), "public"), ["x*"]);
        assert_eq!(include.tables_of(Some("db"), "audit"), ["b"]);
        assert_eq!(include.tables_of(None, "public"), [] as [String; 0]);

        include.set_tables(Some("db"), "new", text(&["n"]));
        assert_eq!(include.tables.len(), 3);
        assert_eq!(include.tables[2].schema, "new", "appended after the others");

        include.set_tables(Some("db"), "public", Vec::new());
        assert_eq!(include.tables_of(Some("db"), "public"), [] as [String; 0]);
        assert_eq!(include.tables.len(), 2, "an empty list removes the entry");
        assert_eq!(include.databases, ["db"], "the other lists are left alone");
    }

    #[test]
    fn a_table_list_matches_exact_names_and_globs_ignoring_case() {
        let include = Include::default().with_tables(vec![table_list(
            None,
            "main",
            &["Users", "log_?", "tmp*"],
        )]);
        let shown = include.resolve(Levels::Flat, None, None);
        for name in ["users", "USERS", "log_1", "tmp", "TMP_old"] {
            assert!(shown.shows_relation("MAIN", name), "{name}");
        }
        for name in ["log_12", "user", "audit"] {
            assert!(!shown.shows_relation("main", name), "{name}");
        }
        assert!(
            shown.shows_relation("other", "anything"),
            "no list, no filter"
        );
    }

    #[test]
    fn each_level_keys_the_table_lists_as_its_catalog_is_keyed() {
        let include = Include::default().with_tables(vec![
            table_list(Some("db"), "public", &["a"]),
            table_list(Some("other"), "public", &["b"]),
            table_list(None, "public", &["c"]),
        ]);
        let admitted =
            |shown: &Shown| ["a", "b", "c"].map(|name| shown.shows_relation("public", name));

        // A database over its schemas: the lists of the catalog's database.
        for levels in [Levels::DatabaseThenSchema, Levels::DatabaseHoldsObjects] {
            let own = include.resolve(levels, Some("DB"), Some("public"));
            assert_eq!(admitted(&own), [true, false, false], "{levels:?} own");
            assert_eq!(
                admitted(&own.in_another_database("other")),
                [false, true, false],
                "{levels:?} another"
            );
            assert_eq!(
                admitted(&own.in_another_database("third")),
                [true, true, true],
                "{levels:?} a database with no list"
            );
            let unknown = include.resolve(levels, None, None);
            assert_eq!(
                admitted(&unknown),
                [true, true, true],
                "{levels:?} no database"
            );
        }

        // The schemas are the databases, or there is no database level: the
        // lists with no database, keyed by the schema's own name.
        for levels in [Levels::DatabaseIsSchema, Levels::Flat] {
            let shown = include.resolve(levels, Some("db"), None);
            assert_eq!(admitted(&shown), [false, false, true], "{levels:?}");
        }
    }

    #[test]
    fn a_flat_target_ignores_the_database_and_schema_lists_but_not_the_table_lists() {
        let none = Include::from_stored(text(&["x"]), text(&["y"]));
        assert_eq!(none.resolve(Levels::Flat, None, None), Shown::everything());

        let include = none.with_tables(vec![table_list(None, "main", &["t"])]);
        let shown = include.resolve(Levels::Flat, None, None);
        assert_ne!(shown, Shown::everything());
        assert!(shown.shows_schema("anything") && shown.shows_database("anything"));
        let narrowed = shown.narrow_catalog(catalog_of("main", vec![table("t"), table("u")]));
        assert_eq!(relation_names(&narrowed, "main"), ["t"]);
    }

    #[test]
    fn narrowing_drops_the_relations_a_table_list_leaves_out_and_records_them() {
        let include = Include::default().with_tables(vec![table_list(None, "main", &["keep*"])]);
        let shown = include.resolve(Levels::Flat, None, None);
        let read = Catalog {
            schemas: vec![
                {
                    let mut main = schema("main");
                    main.relations = vec![
                        table("keep_a"),
                        table("drop_b"),
                        relation("v_drop", RelationKind::View, None),
                        relation("keep_view", RelationKind::MaterializedView, None),
                    ];
                    main.sequences = vec!["seq".into()];
                    main
                },
                {
                    let mut other = schema("other");
                    other.relations = vec![table("untouched")];
                    other
                },
            ],
            ..Catalog::default()
        };

        let narrowed = shown.narrow_catalog(read);
        assert_eq!(relation_names(&narrowed, "main"), ["keep_a", "keep_view"]);
        assert_eq!(relation_names(&narrowed, "other"), ["untouched"]);
        assert_eq!(narrowed.schemas[0].sequences, ["seq"], "never filtered");
        assert_eq!(narrowed.left_out, 0, "no schema was left out");
        assert_eq!(
            narrowed.hidden_relations["main"],
            [
                ("drop_b".to_string(), RelationKind::Table),
                ("v_drop".to_string(), RelationKind::View)
            ]
        );
        assert!(!narrowed.hidden_relations.contains_key("other"));
        assert_eq!(narrowed.hidden_in("main"), 2);
        let names = narrowed.relations_of("main");
        assert_eq!(names.shown.len(), 2);
        assert_eq!(names.hidden.len(), 2);
        assert_eq!(names.total(), 4, "the M of `N of M`");
    }

    #[test]
    fn a_partition_is_shown_exactly_when_its_parent_is_and_is_never_counted() {
        let include = Include::default().with_tables(vec![table_list(None, "main", &["events"])]);
        let shown = include.resolve(Levels::Flat, None, None);
        let read = catalog_of(
            "main",
            vec![
                relation("events", RelationKind::PartitionedTable, None),
                table("events_2024"),
                relation("events_2024_q1", RelationKind::Table, Some("events_2024")),
                relation("logs", RelationKind::PartitionedTable, None),
                relation("events_2024_b", RelationKind::Table, Some("logs")),
                relation("orphan", RelationKind::Table, Some("not_listed")),
            ],
        );
        let mut read = read;
        read.schemas[0].relations[1].partition_of = Some("events".into());

        let narrowed = shown.narrow_catalog(read);
        assert_eq!(
            relation_names(&narrowed, "main"),
            ["events", "events_2024", "events_2024_q1"],
            "a partition of a partition follows the root, whatever its own name"
        );
        assert_eq!(
            narrowed.hidden_relations["main"],
            [
                ("logs".to_string(), RelationKind::PartitionedTable),
                ("orphan".to_string(), RelationKind::Table),
            ],
            "the partition of a hidden parent is not listed, a parent-less one stands alone"
        );
        assert_eq!(narrowed.relations_of("main").shown.len(), 1);
    }

    #[test]
    fn narrowing_twice_changes_nothing_and_a_merge_or_sizes_leave_the_record_alone() {
        let include = Include::default().with_tables(vec![table_list(None, "main", &["a"])]);
        let shown = include.resolve(Levels::Flat, None, None);
        let narrowed = shown.narrow_catalog(catalog_of("main", vec![table("a"), table("b")]));
        let again = shown.narrow_catalog(narrowed.clone());
        assert_eq!(again, narrowed);

        let mut merged = narrowed.clone();
        let mut routines = catalog(&["main"]);
        routines.schemas[0].sequences.push("s".into());
        merged.merge(shown.narrow_routines(routines));
        assert_eq!(merged.hidden_relations, narrowed.hidden_relations);
        assert_eq!(relation_names(&merged, "main"), ["a"]);

        let mut sized = merged.clone();
        let mut sizes = Sizes::new();
        sizes.entry("main".into()).or_default().insert(
            "b".into(),
            crate::db::Statistics {
                size: Some(9),
                rows: Some(1),
            },
        );
        sized.set_sizes(&shown.narrow_sizes(sizes));
        assert_eq!(sized.hidden_relations, narrowed.hidden_relations);
        assert_eq!(sized.schemas[0].relations[0].size, None);
    }

    #[test]
    fn another_databases_catalog_is_narrowed_by_that_databases_table_lists() {
        let include = Include::default().with_tables(vec![
            table_list(Some("db"), "public", &["own_only"]),
            table_list(Some("other"), "public", &["there_only"]),
        ]);
        let shown = include.resolve(Levels::DatabaseThenSchema, Some("db"), Some("public"));
        let read = || catalog_of("public", vec![table("own_only"), table("there_only")]);

        assert_eq!(
            relation_names(&shown.narrow_catalog(read()), "public"),
            ["own_only"]
        );
        assert_eq!(
            relation_names(
                &shown.in_another_database("other").narrow_catalog(read()),
                "public"
            ),
            ["there_only"]
        );
    }

    #[test]
    fn a_relation_is_listed_by_its_schemas_table_list_in_its_own_or_another_database() {
        let include = Include::default().with_tables(vec![
            table_list(Some("db"), "public", &["users"]),
            table_list(Some("app1"), "public", &["orders"]),
        ]);
        let shown = include.resolve(Levels::DatabaseThenSchema, Some("db"), Some("main"));

        assert!(shown.lists_object(None, "public", Some("users")));
        assert!(!shown.lists_object(None, "public", Some("orders")));
        assert!(shown.lists_object(None, "public", None), "a routine, say");
        assert!(
            shown.lists_object(None, "main", Some("anything")),
            "no list"
        );
        assert!(shown.lists_object(Some("app1"), "public", Some("orders")));
        assert!(!shown.lists_object(Some("app1"), "public", Some("users")));
        assert!(Shown::everything().lists_object(Some("x"), "y", Some("z")));
    }
}
