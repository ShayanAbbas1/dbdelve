//! Whether something other than an apply's own statements can rewrite the
//! rows of a table that one of its earlier statements wrote: a trigger, a
//! rule, or a foreign key's cascade. A row named by every value is proven to
//! be the one read only at its re-read, before any write, and such a rewrite
//! can move another row onto exactly those values, which the step's write
//! then counts as its one row ([`super::run_verified`]).

use super::Engine;

/// What [`probe`] asks of an engine's catalog.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Probe {
    /// Nothing on this engine rewrites a row behind a statement: DuckDB has no
    /// triggers, and its foreign keys never cascade.
    Never,
    /// A catalog read, run inside the apply's transaction, that returns a row
    /// when something can.
    Ask(String),
    /// The engine's catalog is not read here: taken as able to.
    Unknown,
}

/// A table as `sql::written_table` cuts it: its schema, when the statement
/// named one, and its name.
type Table<'a> = (Option<&'a str>, &'a str);

/// The read for a keyless step writing `later`, after earlier statements of
/// the same apply wrote `earlier`. It finds a trigger (or, on Postgres, a
/// rule) on any of those tables, or a foreign key from `later` to one of
/// `earlier` whose `ON DELETE` or `ON UPDATE` cascades, sets NULL or sets the
/// default. A table whose name cannot be read is [`Probe::Unknown`].
pub(super) fn probe(engine: Engine, later: &[String], earlier: &[Vec<String>]) -> Probe {
    if engine == Engine::DuckDb {
        return Probe::Never;
    }
    let Some(later) = table(later) else {
        return Probe::Unknown;
    };
    let Some(earlier) = earlier
        .iter()
        .map(|parts| table(parts))
        .collect::<Option<Vec<_>>>()
    else {
        return Probe::Unknown;
    };
    let mut all = earlier.clone();
    all.push(later);
    let literal = |text: &str| engine.quote_literal(text);
    match engine {
        Engine::Sqlite => Probe::Ask(sqlite(later, &earlier, &all, &literal)),
        Engine::Postgres => {
            let pair = |(schema, name): Table| {
                let schema = schema.map_or("current_schema()::text".into(), literal);
                format!("({schema}, {})", literal(name))
            };
            let pairs = |tables: &[Table]| tables.iter().map(|t| pair(*t)).collect::<Vec<_>>();
            let (all, earlier) = (pairs(&all).join(", "), pairs(&earlier).join(", "));
            let named = |namespace: &str, class: &str| {
                format!("({namespace}.nspname::text, {class}.relname::text)")
            };
            Probe::Ask(format!(
                "SELECT 1 FROM pg_catalog.pg_trigger AS t \
                 JOIN pg_catalog.pg_class AS c ON c.oid = t.tgrelid \
                 JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace \
                 WHERE NOT t.tgisinternal AND {} IN ({all}) \
                 UNION ALL \
                 SELECT 1 FROM pg_catalog.pg_rewrite AS r \
                 JOIN pg_catalog.pg_class AS c ON c.oid = r.ev_class \
                 JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace \
                 WHERE r.rulename <> '_RETURN' AND {} IN ({all}) \
                 UNION ALL \
                 SELECT 1 FROM pg_catalog.pg_constraint AS k \
                 JOIN pg_catalog.pg_class AS c ON c.oid = k.conrelid \
                 JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace \
                 JOIN pg_catalog.pg_class AS p ON p.oid = k.confrelid \
                 JOIN pg_catalog.pg_namespace AS pn ON pn.oid = p.relnamespace \
                 WHERE k.contype = 'f' \
                 AND (k.confdeltype IN ('c', 'n', 'd') OR k.confupdtype IN ('c', 'n', 'd')) \
                 AND {} = {} AND {} IN ({earlier}) \
                 LIMIT 1",
                named("n", "c"),
                named("n", "c"),
                named("n", "c"),
                pair(later),
                named("pn", "p"),
            ))
        }
        Engine::MySql | Engine::MariaDb => {
            let pair = |(schema, name): Table| {
                let schema = schema.map_or("DATABASE()".into(), literal);
                format!("({schema}, {})", literal(name))
            };
            let pairs = |tables: &[Table]| tables.iter().map(|t| pair(*t)).collect::<Vec<_>>();
            Probe::Ask(format!(
                "SELECT 1 FROM information_schema.TRIGGERS \
                 WHERE (EVENT_OBJECT_SCHEMA, EVENT_OBJECT_TABLE) IN ({}) \
                 UNION ALL \
                 SELECT 1 FROM information_schema.REFERENTIAL_CONSTRAINTS \
                 WHERE (CONSTRAINT_SCHEMA, TABLE_NAME) = {} \
                 AND (UNIQUE_CONSTRAINT_SCHEMA, REFERENCED_TABLE_NAME) IN ({}) \
                 AND (DELETE_RULE IN ('CASCADE', 'SET NULL', 'SET DEFAULT') \
                 OR UPDATE_RULE IN ('CASCADE', 'SET NULL', 'SET DEFAULT')) \
                 LIMIT 1",
                pairs(&all).join(", "),
                pair(later),
                pairs(&earlier).join(", "),
            ))
        }
        Engine::SqlServer => {
            let named = |schemas: &str, objects: &str, (schema, name): Table| {
                let schema = schema.map_or("SCHEMA_NAME()".into(), literal);
                format!(
                    "({schemas}.name = {schema} AND {objects}.name = {})",
                    literal(name)
                )
            };
            let any = |schemas: &str, objects: &str, tables: &[Table]| {
                tables
                    .iter()
                    .map(|t| named(schemas, objects, *t))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            };
            Probe::Ask(format!(
                "SELECT 1 AS rewrites FROM sys.triggers AS tr \
                 JOIN sys.objects AS o ON o.object_id = tr.parent_id \
                 JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 WHERE {} \
                 UNION ALL \
                 SELECT 1 FROM sys.foreign_keys AS fk \
                 JOIN sys.objects AS c ON c.object_id = fk.parent_object_id \
                 JOIN sys.schemas AS cs ON cs.schema_id = c.schema_id \
                 JOIN sys.objects AS p ON p.object_id = fk.referenced_object_id \
                 JOIN sys.schemas AS ps ON ps.schema_id = p.schema_id \
                 WHERE {} \
                 AND (fk.delete_referential_action <> 0 OR fk.update_referential_action <> 0) \
                 AND ({})",
                any("s", "o", &all),
                named("cs", "c", later),
                any("ps", "p", &earlier),
            ))
        }
        _ => Probe::Unknown,
    }
}

/// SQLite keeps a schema's triggers in its own `sqlite_master`, and a
/// temporary trigger on any table in `temp`'s. A foreign key names its parent
/// by name alone, in the child's schema. Names compare as SQLite's do,
/// without case.
fn sqlite(
    later: Table,
    earlier: &[Table],
    all: &[Table],
    literal: &dyn Fn(&str) -> String,
) -> String {
    let schema = |(schema, _): Table| schema.unwrap_or("main").to_string();
    let names = |tables: &mut dyn Iterator<Item = &Table>| {
        tables
            .map(|(_, name)| literal(name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut schemas: Vec<String> = all.iter().map(|t| schema(*t)).collect();
    schemas.sort();
    schemas.dedup();
    let mut reads: Vec<String> = schemas
        .iter()
        .map(|each| {
            format!(
                "SELECT 1 FROM {}.sqlite_master WHERE type = 'trigger' \
                 AND tbl_name COLLATE NOCASE IN ({})",
                Engine::Sqlite.quote_identifier(each),
                names(&mut all.iter().filter(|t| schema(**t) == *each)),
            )
        })
        .collect();
    reads.push(format!(
        "SELECT 1 FROM temp.sqlite_master WHERE type = 'trigger' \
         AND tbl_name COLLATE NOCASE IN ({})",
        names(&mut all.iter()),
    ));
    let parents = names(&mut earlier.iter().filter(|t| schema(**t) == schema(later)));
    if !parents.is_empty() {
        reads.push(format!(
            "SELECT 1 FROM pragma_foreign_key_list({}, {}) \
             WHERE \"table\" COLLATE NOCASE IN ({parents}) \
             AND (on_delete IN ('CASCADE', 'SET NULL', 'SET DEFAULT') \
             OR on_update IN ('CASCADE', 'SET NULL', 'SET DEFAULT'))",
            literal(later.1),
            literal(&schema(later)),
        ));
    }
    format!("{} LIMIT 1", reads.join(" UNION ALL "))
}

fn table(parts: &[String]) -> Option<Table<'_>> {
    match parts {
        [name] => Some((None, name.as_str())),
        [schema, name] => Some((Some(schema.as_str()), name.as_str())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn an_engine_whose_catalog_is_not_read_is_taken_as_rewriting_and_duckdb_never() {
        let later = parts(&["main", "t"]);
        let earlier = [parts(&["main", "t"])];
        assert_eq!(probe(Engine::Snowflake, &later, &earlier), Probe::Unknown);
        assert_eq!(probe(Engine::ClickHouse, &later, &earlier), Probe::Unknown);
        assert_eq!(probe(Engine::DuckDb, &later, &earlier), Probe::Never);
        assert_eq!(
            probe(Engine::Sqlite, &parts(&["a", "b", "c"]), &earlier),
            Probe::Unknown
        );
    }

    #[test]
    fn a_name_is_read_as_a_literal_in_every_catalog_read() {
        let later = parts(&["main", "o'brien"]);
        for engine in [
            Engine::Sqlite,
            Engine::Postgres,
            Engine::MySql,
            Engine::MariaDb,
            Engine::SqlServer,
        ] {
            let Probe::Ask(sql) = probe(engine, &later, std::slice::from_ref(&later)) else {
                panic!("{engine:?} reads its catalog");
            };
            assert!(sql.contains("o''brien"), "{engine:?}: {sql}");
            assert!(!sql.contains("o'brien'"), "{engine:?}: {sql}");
        }
    }
}
