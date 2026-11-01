//! What a profile connects to: the engine whose SQL it speaks, and the driver
//! that carries it there. The abilities to write, switch and pick are asked of
//! a `Target` rather than an `Engine`, because a plugin driver speaking a
//! dialect DBDelve knows has none of them, whatever that dialect could do.

use super::plugin::Plugin;
use super::spec::Abilities;
use super::{Engine, RelationKind, Syntax};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    BuiltIn,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "made once a profile can name a plugin")
    )]
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

    /// A plugin is the read-only tier whatever it speaks: none of the write
    /// paths has been proven against a driver DBDelve does not ship.
    pub fn abilities(self) -> Abilities {
        match self.driver {
            Driver::BuiltIn => self.engine.spec().abilities,
            Driver::Plugin(_) => Abilities::NONE,
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

    /// Whether a query tab's grid takes a deleted row, as a relation's preview
    /// does, shown for review before it runs and never written into the
    /// buffer. Redis opens a key as a query tab of its own, its contents having
    /// no relation to open, and that is where its fields, items and members
    /// are taken out; an engine that reviews a query tab's edits
    /// ([`Engine::reviews_query_edits`]) takes them there too. A new row needs
    /// no such gate: any grid written back to one table takes one.
    pub fn edits_query_rows(self) -> bool {
        self.edits_rows()
            && match self.engine.syntax() {
                Syntax::Redis => true,
                Syntax::Sql => self.engine.reviews_query_edits(),
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

    /// Whether the row count the catalog carries for a table is exact rather
    /// than the statistics' estimate.
    pub fn exact_row_estimates(self) -> bool {
        self.abilities().exact_row_estimates
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
    fn plugin_targets_are_the_read_only_tier() {
        let plugin = odbc();
        // Every engine, not only the dialects a plugin is offered: the clamp
        // is the driver's, so no engine's own abilities may leak through it.
        for engine in Engine::ALL {
            let target = Target {
                engine,
                driver: Driver::Plugin(plugin),
            };
            let kinds = [RelationKind::Table, RelationKind::View];
            let abilities = [
                target.edits_rows(),
                target.edits_query_rows(),
                target.copies_ddl(),
                target.switches_database(),
                target.switches_database_in_session(),
                target.picks_schema(),
                target.catalog_is_per_database(),
                target.exact_row_estimates(),
            ]
            .into_iter()
            .chain(kinds.iter().map(|&kind| target.takes_inserts(kind)))
            .chain(kinds.iter().map(|&kind| target.takes_imports(kind)));
            assert!(abilities.into_iter().all(|on| !on), "{engine:?}");
            assert!(target.label().ends_with(" via ODBC"), "{engine:?}");
        }
        assert_eq!(
            Target {
                engine: Engine::Generic,
                driver: Driver::Plugin(plugin),
            }
            .label(),
            "Generic SQL via ODBC"
        );
    }
}
