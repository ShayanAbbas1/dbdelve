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

use super::{Catalog, Databases, Engine, Levels, Sizes};

/// How many entries a list keeps. A file is a person's to edit: no more is
/// read than a profile would write.
pub const ENTRIES_CAP: usize = 500;

/// What a profile's lists say, as written in `profiles.toml` and in the form.
/// Not part of the connection: changing it never reconnects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Include {
    pub databases: Vec<String>,
    pub schemas: Vec<String>,
}

impl Include {
    /// What a file held, or what Save is about to write: each entry trimmed,
    /// those left empty dropped, at most [`ENTRIES_CAP`] of each list kept, in
    /// the order written.
    pub fn from_stored(databases: Vec<String>, schemas: Vec<String>) -> Self {
        let kept = |list: Vec<String>| {
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
        };
        Self {
            databases: kept(databases),
            schemas: kept(schemas),
        }
    }

    /// The lists as they apply to a target of shape `levels` whose session is
    /// on `own_database` and `active_schema`. A list the shape does not use is
    /// kept on disk and ignored here, and a shape with no databases to choose
    /// between is never filtered.
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
        match levels {
            Levels::Flat => Shown::everything(),
            Levels::DatabaseIsSchema | Levels::DatabaseHoldsObjects => Shown {
                databases: entries(&self.databases),
                own_database: own_database.map(str::to_string),
                schemas_are_databases: levels == Levels::DatabaseIsSchema,
                ..Shown::default()
            },
            Levels::DatabaseThenSchema => Shown {
                databases: entries(&self.databases),
                own_database: own_database.map(str::to_string),
                schemas: entries(&self.schemas),
                active_schema: active_schema.map(str::to_string),
                ..Shown::default()
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
    /// shown itself and whose schemas are matched without the active one. A
    /// tab or a shortcut on an object this does not list is hidden, not gone.
    pub fn lists_object(&self, elsewhere: Option<&str>, schema: &str) -> bool {
        match elsewhere {
            None => self.shows_schema(schema),
            Some(database) => {
                self.shows_database(database) && self.in_another_database().shows_schema(schema)
            }
        }
    }

    /// What this shows of a catalog another database's session read: the same
    /// lists, with no active schema, which is the own database's.
    pub fn in_another_database(&self) -> Self {
        Self {
            active_schema: None,
            ..self.clone()
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
    pub fn narrow_catalog(&self, mut catalog: Catalog) -> Catalog {
        let read = catalog.schemas.len();
        catalog
            .schemas
            .retain(|schema| self.shows_schema(&schema.name));
        catalog.left_out += read - catalog.schemas.len();
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
            .in_another_database()
            .narrow_catalog(catalog(&["main", "x"]));
        assert_eq!(names(&other), ["x"]);
        assert_eq!(other.left_out, 1);
    }

    #[test]
    fn an_object_is_listed_by_its_schema_and_elsewhere_by_its_database_too() {
        let include = Include::from_stored(text(&["app*"]), text(&["public"]));
        let shown = include.resolve(Levels::DatabaseThenSchema, Some("db"), Some("main"));

        assert!(shown.lists_object(None, "public"));
        assert!(shown.lists_object(None, "main"), "the active schema");
        assert!(!shown.lists_object(None, "audit"));
        assert!(shown.lists_object(Some("app1"), "public"));
        assert!(
            !shown.lists_object(Some("app1"), "main"),
            "active only at home"
        );
        assert!(!shown.lists_object(Some("other"), "public"));

        let mysql = Include::from_stored(text(&["app*"]), Vec::new()).resolve(
            Levels::DatabaseIsSchema,
            Some("db"),
            None,
        );
        assert!(mysql.lists_object(None, "app1"));
        assert!(mysql.lists_object(None, "db"));
        assert!(!mysql.lists_object(None, "reporting"));
        assert!(Shown::everything().lists_object(Some("any"), "thing"));
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
}
