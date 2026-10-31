//! What a profile connects to: the engine whose SQL it speaks, and the driver
//! that carries it there. The abilities to write, switch and pick are asked of
//! a `Target` rather than an `Engine`, because a plugin driver speaking a
//! dialect DBDelve knows has few of them, whatever that dialect could do.

use super::plugin::Plugin;
use super::spec::{Abilities, DdlNames};
use super::{
    DbError, Engine, Fields, RelationKind, ServerConfig, Syntax, clickhouse, duckdb, external,
    mssql, postgres, redis, turso,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    BuiltIn,
    Plugin(&'static Plugin),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub engine: Engine,
    pub driver: Driver,
}

impl Target {
    pub fn built_in(engine: Engine) -> Self {
        Self {
            engine,
            driver: Driver::BuiltIn,
        }
    }

    /// Whether `self`, a tile, is the one `current` was picked under: a built-in
    /// tile is its engine alone, a plugin's tile is the plugin whatever dialect
    /// the form picked under it.
    pub fn is_tile_of(self, current: Self) -> bool {
        match self.driver {
            Driver::BuiltIn => current == self,
            Driver::Plugin(_) => current.driver == self.driver,
        }
    }

    /// What a profile can be started for: every engine DBDelve has a driver
    /// for, then each plugin speaking Generic SQL until its form picks another
    /// dialect.
    pub fn tiles(plugins: &[&'static Plugin]) -> impl Iterator<Item = Self> {
        Engine::ALL
            .into_iter()
            .filter(|engine| engine.has_built_in_driver())
            .map(Self::built_in)
            .chain(plugins.iter().map(|&plugin| Self {
                engine: Engine::Generic,
                driver: Driver::Plugin(plugin),
            }))
    }

    /// A plugin edits, deletes and inserts rows only in a dialect proven
    /// through a real driver (`external::writes_rows`), and nothing else
    /// whatever it speaks: no import, no Copy DDL, no moving between
    /// databases or schemas. Even then a connection's rows are editable only
    /// once its plugin offers transactional writes and reads their key back
    /// exactly, which each result says.
    pub fn abilities(self) -> Abilities {
        match self.driver {
            Driver::BuiltIn => self.engine.spec().abilities,
            Driver::Plugin(_) if external::writes_rows(self.engine) => Abilities {
                edits: true,
                inserts: true,
                ..Abilities::NONE
            },
            Driver::Plugin(_) => Abilities::NONE,
        }
    }

    /// Which fields make a profile for this target: a plugin's address is in
    /// its connection string, whatever dialect it speaks.
    pub fn fields(self) -> Fields {
        match self.driver {
            Driver::BuiltIn => self.engine.fields(),
            Driver::Plugin(_) => Fields::ConnectionString,
        }
    }

    /// Whether a profile can name a connection service file's section: DBDelve
    /// reads that file for its own driver, and a plugin's string goes to its
    /// driver as typed.
    pub fn takes_service(self) -> bool {
        self.driver == Driver::BuiltIn && self.engine.takes_service()
    }

    /// Whether a profile can have a command print its password at each login.
    /// Every server login DBDelve performs itself, except MongoDB's: its driver
    /// redials with the credentials it parsed at connect, so a token would go
    /// stale under it unseen.
    pub fn takes_password_command(self) -> bool {
        self.driver == Driver::BuiltIn
            && matches!(
                self.engine,
                Engine::Postgres
                    | Engine::MySql
                    | Engine::MariaDb
                    | Engine::SqlServer
                    | Engine::Redis
                    | Engine::ClickHouse
            )
    }

    /// Whether a profile's SSH tunnel may end at a Unix socket on the SSH host,
    /// opened as another OS user: a Host starting with `/` is its directory.
    pub fn reaches_socket_over_ssh(self) -> bool {
        self.driver == Driver::BuiltIn && self.engine == Engine::Postgres
    }

    /// Why `server`'s SSH settings cannot reach where they point, if they
    /// cannot: the refusal the connection itself would give.
    pub fn ssh_socket_error(self, server: &ServerConfig) -> Option<String> {
        if !self.reaches_socket_over_ssh() {
            return None;
        }
        postgres::ssh_socket_error(server)
    }

    /// ponytail: every plugin wears one generic plug, since no manifest names
    /// artwork. A `logo.svg` beside the manifest arrives with the first
    /// third-party plugin.
    pub fn logo(self) -> &'static str {
        match self.driver {
            Driver::BuiltIn => self.engine.logo(),
            Driver::Plugin(_) => "icons/plugin.svg",
        }
    }

    pub fn label(self) -> String {
        match self.driver {
            Driver::BuiltIn => self.engine.label().to_string(),
            Driver::Plugin(plugin) => format!("{} via {}", self.engine.label(), plugin.name),
        }
    }

    /// Whether a grid's cells and rows may be written back at all, which a
    /// target in the read-only tier never is.
    pub fn edits_rows(self) -> bool {
        self.abilities().edits
    }

    /// Why no grid's rows are written back on this target, when none are,
    /// said when an edit is asked of one anyway.
    pub fn unwritable(self) -> Option<String> {
        match self.driver {
            Driver::Plugin(_) => external::unwritable(self.engine),
            Driver::BuiltIn => {
                (!self.edits_rows()).then(|| format!("{} rows are read-only here.", self.label()))
            }
        }
    }

    /// Whether a query tab's edits are shown in the review dialog an object
    /// tab's are, rather than appended to its buffer, because what runs is
    /// not the text alone: an engine whose rows are named by every value
    /// ([`Engine::reviews_query_edits`]), and a plugin, whose batch runs in a
    /// transaction of its driver's that holds each statement to one row.
    /// Either way a copy left in the buffer would run without that guard.
    pub fn reviews_query_edits(self) -> bool {
        match self.driver {
            Driver::BuiltIn => self.engine.reviews_query_edits(),
            Driver::Plugin(_) => true,
        }
    }

    /// What the review of a generated write says beyond its statements.
    pub fn review_notes(self, sql: &str) -> Vec<String> {
        match self.driver {
            Driver::BuiltIn => self.engine.review_notes(sql),
            Driver::Plugin(_) => vec![
                "Runs as one transaction through the driver. Each statement must change exactly \
                 one row, or nothing is saved."
                    .into(),
            ],
        }
    }

    /// Whether a query tab's grid takes a deleted row, as a relation's preview
    /// does, shown for review before it runs and never written into the
    /// buffer. Redis opens a key as a query tab of its own, its contents having
    /// no relation to open, and that is where its fields, items and members
    /// are taken out; a target that reviews a query tab's edits
    /// ([`Self::reviews_query_edits`]) takes them there too. A new row needs
    /// no such gate: any grid written back to one table takes one.
    pub fn edits_query_rows(self) -> bool {
        self.edits_rows()
            && match self.engine.syntax() {
                Syntax::Redis => true,
                Syntax::Sql => self.reviews_query_edits(),
                Syntax::Mongo => false,
            }
    }

    /// Whether New row may insert into a relation of `kind`.
    pub fn takes_inserts(self, kind: RelationKind) -> bool {
        self.abilities().takes_inserts(kind)
    }

    /// Whether Import Rows may insert a file's rows into a relation of
    /// `kind`: where New row may, on a target that takes imports at all.
    pub fn takes_imports(self, kind: RelationKind) -> bool {
        self.abilities().imports && self.takes_inserts(kind)
    }

    /// Whether a relation has a definition [`super::Connection::ddl`] can
    /// write out.
    pub fn copies_ddl(self) -> bool {
        self.abilities().copies_ddl
    }

    /// Whether a schema exports as one file of SQL, and how its DDL names its
    /// tables (see [`DdlNames`]).
    pub fn exports_schema(self) -> Option<DdlNames> {
        self.abilities().exports_schema
    }

    /// Whether one server holds several databases a profile can be moved
    /// between, which [`super::Connection::databases`] lists.
    pub fn switches_database(self) -> bool {
        self.abilities().switches_database
    }

    /// Whether moving the profile onto another database is a `USE` on the
    /// connection already open rather than a new connection.
    pub fn switches_database_in_session(self) -> bool {
        self.abilities().switches_database_in_session
    }

    /// Whether a profile picks the schema unqualified names resolve in.
    pub fn picks_schema(self) -> bool {
        self.abilities().picks_schema
    }

    /// Whether the catalog holds one database's objects, so naming an object
    /// takes its database as well as its schema: `public.users` is in every
    /// Postgres database, while MySQL lists every database as a schema.
    pub fn catalog_is_per_database(self) -> bool {
        let abilities = self.abilities();
        abilities.switches_database && !abilities.catalog_lists_databases
    }

    /// Whether the catalog lists every database on the server, each as a
    /// schema, as MySQL's does.
    pub fn catalog_lists_databases(self) -> bool {
        self.abilities().catalog_lists_databases
    }

    /// Whether the row count the catalog carries for a table is exact rather
    /// than the statistics' estimate.
    pub fn exact_row_estimates(self) -> bool {
        self.abilities().exact_row_estimates
    }
}

/// Whether `error` is the user's Cancel landing, in the words each engine's
/// stop comes back in, rather than anything else that ended the statement while
/// the cancel was on its way. A server answering in another language reads as
/// not a cancel, which drops the rows instead of keeping them as a result.
///
/// A plugin's words are its own, whatever dialect it speaks: the host turns
/// every cancelled terminal into one that starts with
/// [`external::CANCELLED`].
pub fn is_cancel(target: Target, error: &DbError) -> bool {
    let message = error.message.as_str();
    let engine = match target.driver {
        Driver::BuiltIn => target.engine,
        Driver::Plugin(_) => return message.starts_with(external::CANCELLED),
    };
    match engine {
        Engine::Postgres => message == "canceling statement due to user request",
        Engine::MySql | Engine::MariaDb => message == "Query execution was interrupted",
        Engine::Sqlite => message == "interrupted",
        Engine::Turso => message == turso::INTERRUPTED,
        // Followed by what became of the transaction a bracketed batch opened.
        Engine::DuckDb => message.starts_with(duckdb::INTERRUPTED),
        // The second is dbdelve's own, a cancel between partitions.
        Engine::Snowflake => matches!(message, "SQL execution canceled" | "Cancelled."),
        Engine::SqlServer => message.starts_with(mssql::CANCELLED),
        Engine::MongoDb => message == "Cancelled.",
        Engine::Redis => message == redis::CANCELLED,
        Engine::ClickHouse => message == clickhouse::CANCELLED,
        // Nothing built in reaches it.
        Engine::Generic => false,
    }
}

/// What a Cancel took beyond the statement it stopped, which the user has to
/// be told even where the rows it had are kept: on SQL Server the error
/// itself, which says the session was reset. Postgres and DuckDB cannot say
/// whether a transaction was open, and a cancel aborts the one it lands in,
/// even a read's; SQLite rolls back the transaction an interrupted write was
/// in. MySQL's `KILL QUERY` and SQLite's interrupt of a read undo only the
/// statement, and Snowflake has no session to lose. A ClickHouse `KILL QUERY`
/// ends only the wait on a mutation `statement` started, not the mutation.
///
/// Through a plugin nothing says what the driver's cancel does to an open
/// transaction, so it is assumed to abort it, as the worst of the built-ins
/// does.
pub fn cancel_cost(
    target: Target,
    error: &DbError,
    reads: bool,
    statement: &str,
) -> Option<String> {
    let engine = match target.driver {
        Driver::BuiltIn => target.engine,
        Driver::Plugin(_) => {
            return Some(
                "A cancel through a plugin driver may abort the transaction it lands in.".into(),
            );
        }
    };
    match engine {
        Engine::ClickHouse if crate::sql::mutation_as_dml(engine.spec(), statement).is_some() => {
            Some("The mutation keeps running in the background; KILL MUTATION stops it.".into())
        }
        Engine::SqlServer => Some(error.message.clone()),
        Engine::Postgres => Some(
            "A cancel aborts the transaction it lands in: if one was open, it refuses every \
             statement until a ROLLBACK."
                .into(),
        ),
        Engine::Sqlite if !reads => Some(
            "If the statement was inside a transaction, SQLite rolled the whole transaction \
             back."
                .into(),
        ),
        Engine::Turso if !reads => Some(
            "If the statement was inside a transaction, Turso will not commit it: the COMMIT \
             fails and the transaction is rolled back."
                .into(),
        ),
        Engine::DuckDb => Some(
            "A cancel aborts the transaction it lands in: if one was open, it refuses every \
             statement until a ROLLBACK, and a COMMIT rolls it back."
                .into(),
        ),
        // Redis has no way to stop a command short of ending its session.
        Engine::Redis => Some(
            "A cancel closes the session it lands on: a MULTI or WATCH it held is gone, and the \
             next command opens a new one."
                .into(),
        ),
        Engine::ClickHouse if !reads => Some(
            "ClickHouse has no transaction to roll back: what the statement had already written \
             stays."
                .into(),
        ),
        Engine::ClickHouse
        | Engine::Sqlite
        | Engine::Turso
        | Engine::MySql
        | Engine::MariaDb
        | Engine::Snowflake
        | Engine::MongoDb
        | Engine::Generic => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn odbc() -> &'static Plugin {
        Box::leak(Box::new(Plugin {
            id: "odbc".into(),
            name: "ODBC".into(),
            version: "1".into(),
            executable: "dbdelve-odbc".into(),
        }))
    }

    #[test]
    fn plugin_targets_write_rows_only_in_a_proven_dialect_and_nothing_else() {
        let plugin = odbc();
        // Every engine, not only the dialects a plugin is offered: the clamp
        // is the driver's, so no engine's own abilities may leak through it.
        for engine in Engine::ALL {
            let target = Target {
                engine,
                driver: Driver::Plugin(plugin),
            };
            let writes = matches!(engine, Engine::Generic | Engine::Postgres);
            let rows = [
                target.edits_rows(),
                target.edits_query_rows(),
                target.takes_inserts(RelationKind::Table),
            ];
            assert_eq!(rows, [writes; 3], "{engine:?}");
            assert_eq!(target.unwritable().is_none(), writes, "{engine:?}");
            assert!(target.reviews_query_edits(), "{engine:?}");
            let others = [
                target.takes_inserts(RelationKind::View),
                target.copies_ddl(),
                target.switches_database(),
                target.switches_database_in_session(),
                target.picks_schema(),
                target.catalog_is_per_database(),
                target.exact_row_estimates(),
                target.takes_service(),
            ]
            .into_iter()
            .chain(
                [RelationKind::Table, RelationKind::View]
                    .iter()
                    .map(|&kind| target.takes_imports(kind)),
            );
            assert!(others.into_iter().all(|on| !on), "{engine:?}");
            assert!(target.label().ends_with(" via ODBC"), "{engine:?}");
        }
        assert_eq!(
            Target {
                engine: Engine::MySql,
                driver: Driver::Plugin(plugin),
            }
            .unwritable()
            .unwrap(),
            "Editing rows through a plugin driver is not supported for MySQL yet, so these rows \
             are read-only."
        );
        assert_eq!(
            Target {
                engine: Engine::Generic,
                driver: Driver::Plugin(plugin),
            }
            .label(),
            "Generic SQL via ODBC"
        );
    }

    #[test]
    fn only_a_built_in_postgres_reaches_a_socket_over_ssh() {
        for engine in Engine::ALL {
            assert_eq!(
                Target::built_in(engine).reaches_socket_over_ssh(),
                engine == Engine::Postgres,
                "{engine:?}"
            );
        }
        let plugin = Target {
            engine: Engine::Postgres,
            driver: Driver::Plugin(odbc()),
        };
        assert!(!plugin.reaches_socket_over_ssh());
    }

    #[test]
    fn a_plugins_cancel_is_read_in_the_drivers_words_not_the_dialects() {
        let target = Target {
            engine: Engine::Postgres,
            driver: Driver::Plugin(odbc()),
        };
        let error = |message: &str| DbError {
            message: message.into(),
            position: None,
        };
        let cancelled = error(&format!("{} Operation canceled", external::CANCELLED));
        assert!(is_cancel(target, &cancelled));
        assert!(!is_cancel(
            target,
            &error("canceling statement due to user request")
        ));
        assert!(cancel_cost(target, &cancelled, true, "SELECT 1").is_some());
    }

    #[test]
    fn a_plugins_tile_follows_the_built_ins_and_speaks_generic_sql() {
        let plugin = odbc();
        let tiles: Vec<_> = Target::tiles(&[plugin]).collect();
        let (last, built_ins) = tiles.split_last().unwrap();
        assert!(built_ins.iter().all(|tile| tile.driver == Driver::BuiltIn));
        assert_eq!(last.engine, Engine::Generic);
        assert_eq!(last.label(), "Generic SQL via ODBC");
    }

    #[test]
    fn a_plugin_is_reached_through_its_connection_string_whatever_it_speaks() {
        let plugin = odbc();
        for engine in Engine::ALL {
            let target = Target {
                engine,
                driver: Driver::Plugin(plugin),
            };
            assert_eq!(target.fields(), Fields::ConnectionString, "{engine:?}");
            assert_eq!(target.logo(), "icons/plugin.svg", "{engine:?}");
            if engine.has_built_in_driver() {
                assert_eq!(Target::built_in(engine).fields(), engine.fields());
                assert_ne!(engine.fields(), Fields::ConnectionString, "{engine:?}");
            }
        }
    }

    #[test]
    fn only_a_built_in_server_login_takes_a_password_command() {
        let taking: Vec<_> = Engine::ALL
            .into_iter()
            .filter(|&engine| Target::built_in(engine).takes_password_command())
            .collect();
        assert_eq!(
            taking,
            [
                Engine::Postgres,
                Engine::MySql,
                Engine::MariaDb,
                Engine::ClickHouse,
                Engine::SqlServer,
                Engine::Redis,
            ]
        );
        let plugin = Target {
            engine: Engine::Postgres,
            driver: Driver::Plugin(odbc()),
        };
        assert!(!plugin.takes_password_command());
    }
}
