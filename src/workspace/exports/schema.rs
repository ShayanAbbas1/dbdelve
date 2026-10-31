//! Writing a whole schema to one file of SQL: each table's `CREATE`, then each
//! table's rows, in an order the file runs back in.

use std::io::Write;

use super::*;
use crate::db::{DdlNames, ExportDdl, ForeignKey, ForeignKeyReplay, Sequences, Structure};

impl Workspace {
    /// Asked again once the save panel has answered, as `start_export` is.
    pub(super) fn start_schema_export(
        &mut self,
        id: &str,
        generation: u64,
        schema: String,
        request: SchemaRequest,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.issued_to(id, generation) else {
            return;
        };
        let engine = profile.config.engine();
        let refusal = match &profile.session.exporting {
            Some(_) => Some("an export of every row is already running on this connection."),
            None => request.tables.iter().find_map(|table| {
                match relation_refetch(engine, &schema, table, "", &[], &[]) {
                    Ok(refetch) => refetch_refusal(
                        engine,
                        &refetch,
                        profile.mode,
                        &profile.confirmed,
                        profile.in_turn().is_some(),
                    ),
                    Err(reason) => Some(reason),
                }
            }),
        };
        let refusal = refusal.or(profile
            .connection()
            .is_none()
            .then_some("the connection is not open."));
        if let Some(reason) = refusal {
            profile.session.notice = Some(format!("The schema was not exported: {reason}"));
            cx.notify();
            return;
        }
        self.run_export(
            id,
            generation,
            path,
            FileAs::Inserts,
            move |connection, read_only, path, progress| {
                export_schema(
                    connection, &schema, &request, read_only, engine, path, progress,
                )
            },
            cx,
        );
    }
}

/// What a schema export writes: which tables, and whether their structure,
/// their rows or both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SchemaRequest {
    pub tables: Vec<String>,
    pub structure: bool,
    pub rows: bool,
    /// The schema's partitioned tables, which no file writes yet, so the
    /// header can say they are left out.
    pub partitioned: Vec<String>,
}

/// One table as the file needs it, read before anything is written.
struct Table {
    name: String,
    ddl: ExportDdl,
    structure: Structure,
}

/// The tables of `request` in `schema`, written to `path` as their structure,
/// their rows or both, a table always after the ones it points at, in the way
/// the engine replays foreign keys (`ForeignKeyReplay`). The rows written, all
/// tables told.
///
/// Everything that can refuse is asked before the file is opened, so a refusal
/// leaves whatever was at `path` as it was.
pub(super) fn export_schema(
    connection: &Connection,
    schema: &str,
    request: &SchemaRequest,
    read_only: bool,
    engine: Engine,
    path: &Path,
    progress: &ExportProgress,
) -> Result<usize, Ended> {
    let refused = |reason: String| Ended::Failed(format!("The schema was not exported: {reason}"));
    let export = engine
        .exports_schema()
        .ok_or_else(|| refused("this engine's structure does not run back as written.".into()))?;
    let (asking, named, names) = (
        connection.clone(),
        schema.to_string(),
        request.tables.clone(),
    );
    // On a thread of its own, as `knowing_the_table` is: a read behind a
    // schema lock must not hold a Cancel up.
    let (read, sequences) =
        unless_cancelled(progress, move || read_tables(&asking, &named, &names))?
            .ok_or_else(|| refused("the read of its tables stopped.".into()))?
            .map_err(refused)?;
    if let Some(reason) = sequence_refusal(request, &sequences) {
        return Err(refused(reason));
    }
    let keys = read
        .iter()
        .map(|table| (table.name.clone(), table.structure.foreign_keys.clone()))
        .collect();
    let mut ordered = dependency_order(schema, keys);
    if export.foreign_keys == ForeignKeyReplay::RefusesCycles
        && request.structure
        && !ordered.cyclic.is_empty()
    {
        return Err(refused(format!(
            "{} reference each other or themselves, so no order of them runs back.",
            ordered.cyclic.join(", ")
        )));
    }
    // With the checks off or none enforced, no order of the rows fails.
    if matches!(
        export.foreign_keys,
        ForeignKeyReplay::ChecksOff { .. } | ForeignKeyReplay::NotEnforced
    ) {
        ordered.cyclic.clear();
    }
    let mut planned = Vec::with_capacity(ordered.order.len());
    for name in &ordered.order {
        let table = read
            .iter()
            .find(|table| &table.name == name)
            .expect("ordered from what was read");
        let refetch = relation_refetch(engine, schema, name, "", &[], &[])
            .map_err(|reason| refused(reason.into()))?;
        let target = InsertTarget {
            relation: Some((schema.to_string(), name.clone())),
            unqualified: export.names == DdlNames::Unqualified,
            ..InsertTarget::default()
        }
        .knowing(engine, &table.structure.columns);
        planned.push((table, refetch, target));
    }

    // An `OWNED BY` ties a sequence to a column the structure creates, so it
    // goes with the structure; a `setval` sets one where the rows left it.
    let (owned, set): (Vec<&String>, Vec<&String>) = sequences
        .after_rows
        .iter()
        .partition(|statement| statement.starts_with("ALTER SEQUENCE"));
    let before_tables: &[String] = match request.structure {
        true => &sequences.before_tables,
        false => &[],
    };
    let mut after_rows = Vec::new();
    if request.rows {
        after_rows.extend(set);
    }
    if request.structure {
        after_rows.extend(owned);
    }
    let checks = match export.foreign_keys {
        ForeignKeyReplay::ChecksOff { off, on } => Some((off, on)),
        _ => None,
    };
    let sequenced = !before_tables.is_empty() || !after_rows.is_empty();

    write_replacing(path, |out| {
        let failed = could_not_write(path);
        out.write_all(header(schema, request, &ordered, sequenced).as_bytes())
            .map_err(&failed)?;
        if let Some((off, _)) = checks {
            write!(out, "{off}\n\n").map_err(&failed)?;
        }
        for statement in before_tables {
            write!(out, "{statement}\n\n").map_err(&failed)?;
        }
        if request.structure {
            for (table, _, _) in &planned {
                write!(out, "{}\n\n", table.ddl.create).map_err(&failed)?;
            }
        }
        let mut written = 0;
        if request.rows {
            for (_, refetch, target) in &planned {
                let sink = Sink {
                    format: FileAs::Inserts,
                    engine,
                    target,
                    path,
                    out: Some(&mut *out),
                    writer: None,
                    progress,
                };
                // The count is the progress's, which every table adds to.
                written = stream(connection, refetch, read_only, sink, progress)?;
                out.write_all(b"\n").map_err(&failed)?;
            }
        }
        for statement in &after_rows {
            write!(out, "{statement}\n\n").map_err(&failed)?;
        }
        if request.structure {
            for (table, _, _) in &planned {
                for statement in &table.ddl.foreign_keys {
                    write!(out, "{statement}\n\n").map_err(&failed)?;
                }
            }
        }
        if let Some((_, on)) = checks {
            writeln!(out, "{on}").map_err(&failed)?;
        }
        match progress.cancelled.load(Ordering::Relaxed) {
            true => Err(Ended::Cancelled),
            false => Ok(written),
        }
    })
}

/// Why the rows cannot be written with where their sequences stand: a
/// `setval` to the start for a sequence the role may not read would replay
/// into a key the next `INSERT` collides with. The structure alone sets none.
fn sequence_refusal(request: &SchemaRequest, sequences: &Sequences) -> Option<String> {
    (request.rows && !sequences.unreadable.is_empty()).then(|| {
        format!(
            "this role may not read where these sequences stand, so the file cannot set them: \
             {}. Grant it SELECT on them, or export the structure alone.",
            sequences.unreadable.join(", ")
        )
    })
}

fn read_tables(
    connection: &Connection,
    schema: &str,
    tables: &[String],
) -> Result<(Vec<Table>, Sequences), String> {
    let own = connection.apart().map_err(|error| error.to_string())?;
    let read = tables
        .iter()
        .map(|name| {
            let failed = |error: DbError| format!("could not read {name}: {error}");
            Ok(Table {
                structure: own.structure(schema, name).map_err(failed)?,
                ddl: own.export_ddl(schema, name).map_err(failed)?,
                name: name.clone(),
            })
        })
        .collect::<Result<_, String>>()?;
    let sequences = own
        .sequences(schema, tables)
        .map_err(|error| format!("could not read its sequences: {error}"))?;
    Ok((read, sequences))
}

/// What the file says of itself, every line a comment. A name holding a line
/// break would end the comment and run the rest as SQL, so it reads as a space.
///
/// ponytail: the tables are read one at a time, not from one snapshot, and
/// only tables and the sequences their columns own are written. A snapshot
/// (one read-only transaction on a session of its own) and views are each a
/// follow-up; routines and triggers would come after them.
fn header(schema: &str, request: &SchemaRequest, ordered: &Ordered, sequences: bool) -> String {
    let line = |text: &str| text.replace(['\r', '\n'], " ");
    let names = |tables: &[String]| {
        tables
            .iter()
            .map(|table| line(table))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let count = match ordered.order.len() {
        1 => "1 table".to_string(),
        tables => format!("{tables} tables"),
    };
    let what = match (request.structure, request.rows) {
        (true, true) => "structure and rows",
        (true, false) => "structure only",
        _ => "rows only",
    };
    // Without the structure a sequence is only set, so it has to be there already.
    let written = match (sequences, request.structure) {
        (false, _) => "Only tables are written",
        (true, true) => "Only tables and the sequences their columns own are written",
        (true, false) => {
            "Only tables are written, and the sequences their columns own are set, not created"
        }
    };
    let partitioned = match request.partitioned.as_slice() {
        [] => String::new(),
        tables => format!(
            "partitioned tables and their partitions ({}), ",
            names(tables)
        ),
    };
    let mut header = format!(
        "-- Schema \"{}\" exported by DBDelve: {count}, {what}.\n\
         -- Each table was read on its own, not from one snapshot.\n\
         -- {written}: {partitioned}views, routines, triggers and every other object are left out.\n",
        line(schema)
    );
    if !ordered.missing.is_empty() {
        header += &format!(
            "-- Referenced but not exported: {}.\n",
            names(&ordered.missing)
        );
    }
    // With the structure, the keys come after the rows or the cycle is refused.
    if !request.structure && !ordered.cyclic.is_empty() {
        header += &format!(
            "-- These tables reference each other, so their rows may not run back in this order: {}.\n",
            names(&ordered.cyclic)
        );
    }
    header + "\n"
}

/// What `dependency_order` made of a schema's tables.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Ordered {
    /// Every table: the acyclic ones in dependency order, then the tables in or
    /// behind a cycle, in the order they came.
    pub order: Vec<String>,
    /// Only the members of a cycle, a self-reference included.
    pub cyclic: Vec<String>,
    /// Tables of `schema` some exported table points at that are not exported,
    /// sorted, each once.
    pub missing: Vec<String>,
}

/// The `tables` of `schema`, each after the ones its keys point at. Tables in a
/// cycle, which no order runs back, and the ones waiting behind a cycle go on
/// the end as they came; `cyclic` names the members of the cycles alone. A key
/// to a table this export does not write holds nothing back, since the file
/// cannot create it either way, and is reported in `missing`. Where nothing
/// forces an order, the tables keep the one they came in, so the file reads as
/// the catalog does.
pub(super) fn dependency_order(schema: &str, tables: Vec<(String, Vec<ForeignKey>)>) -> Ordered {
    let names: Vec<String> = tables.iter().map(|(name, _)| name.clone()).collect();
    let mut missing: Vec<String> = tables
        .iter()
        .flat_map(|(_, keys)| keys)
        .filter(|key| key.referenced_schema == schema && !names.contains(&key.referenced_table))
        .map(|key| key.referenced_table.clone())
        .collect();
    missing.sort();
    missing.dedup();

    let mut placed: Vec<String> = Vec::with_capacity(tables.len());
    let mut waiting = tables;
    while !waiting.is_empty() {
        let before = waiting.len();
        waiting.retain(|(name, keys)| {
            let ready = keys.iter().all(|key| {
                key.referenced_schema != schema
                    || !names.contains(&key.referenced_table)
                    || (key.referenced_table != *name && placed.contains(&key.referenced_table))
            });
            if ready {
                placed.push(name.clone());
            }
            !ready
        });
        if waiting.len() == before {
            break;
        }
    }

    // A waiting table is in a cycle when following keys between waiting tables
    // leads back to it; one that only leads into a cycle never does.
    let leads_to = |from: &str| -> Vec<&str> {
        waiting
            .iter()
            .find(|(name, _)| name == from)
            .into_iter()
            .flat_map(|(_, keys)| keys)
            .filter(|key| key.referenced_schema == schema)
            .map(|key| key.referenced_table.as_str())
            .filter(|table| waiting.iter().any(|(name, _)| name == table))
            .collect()
    };
    let cyclic = waiting
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|start| {
            let mut seen: Vec<&str> = Vec::new();
            let mut stack = leads_to(start);
            while let Some(table) = stack.pop() {
                if table == *start {
                    return true;
                }
                if !seen.contains(&table) {
                    seen.push(table);
                    stack.extend(leads_to(table));
                }
            }
            false
        })
        .map(str::to_string)
        .collect();

    placed.extend(waiting.into_iter().map(|(name, _)| name));
    Ordered {
        order: placed,
        cyclic,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ForeignKeyReplay, SchemaExport};

    #[test]
    fn only_an_engine_whose_ddl_replays_as_written_exports_a_schema() {
        // Snowflake's DDL is `create or replace`, and SQLite's carries the
        // triggers that would fire on the rows written after them.
        for engine in [
            Engine::Snowflake,
            Engine::Sqlite,
            Engine::Turso,
            Engine::MongoDb,
            Engine::Redis,
        ] {
            assert_eq!(engine.exports_schema(), None, "{engine:?}");
        }
        let export = |names, foreign_keys| {
            Some(SchemaExport {
                names,
                foreign_keys,
            })
        };
        for engine in [Engine::Postgres, Engine::SqlServer] {
            assert_eq!(
                engine.exports_schema(),
                export(DdlNames::Qualified, ForeignKeyReplay::AddedAfterRows),
                "{engine:?}"
            );
        }
        assert_eq!(
            Engine::ClickHouse.exports_schema(),
            export(DdlNames::Qualified, ForeignKeyReplay::NotEnforced)
        );
        // The pair saves and restores the session's own setting, so a session
        // that had checks off is not left with them on.
        for engine in [Engine::MySql, Engine::MariaDb] {
            assert_eq!(
                engine.exports_schema(),
                export(
                    DdlNames::Unqualified,
                    ForeignKeyReplay::ChecksOff {
                        off: "SET @OLD_FOREIGN_KEY_CHECKS=@@FOREIGN_KEY_CHECKS, FOREIGN_KEY_CHECKS=0;",
                        on: "SET FOREIGN_KEY_CHECKS=@OLD_FOREIGN_KEY_CHECKS;",
                    }
                ),
                "{engine:?}"
            );
        }
        assert_eq!(
            Engine::DuckDb.exports_schema(),
            export(DdlNames::Unqualified, ForeignKeyReplay::RefusesCycles)
        );
    }

    fn key(table: &str) -> ForeignKey {
        ForeignKey {
            column: "ref_id".into(),
            referenced_schema: "s".into(),
            referenced_table: table.into(),
            referenced_column: "id".into(),
        }
    }

    fn names(of: &[&str]) -> Vec<String> {
        of.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_table_comes_after_the_tables_it_references_and_ties_keep_their_order() {
        let tables = vec![
            ("orders".into(), vec![key("customers"), key("products")]),
            ("products".into(), vec![]),
            ("customers".into(), vec![]),
            ("lines".into(), vec![key("orders"), key("products")]),
        ];
        assert_eq!(
            dependency_order("s", tables),
            Ordered {
                order: names(&["products", "customers", "orders", "lines"]),
                cyclic: vec![],
                missing: vec![],
            }
        );
    }

    #[test]
    fn tables_that_reference_each_other_or_themselves_are_named() {
        let pair = vec![
            ("plain".into(), vec![]),
            ("employees".into(), vec![key("departments")]),
            ("departments".into(), vec![key("employees")]),
            ("badge".into(), vec![key("employees")]),
        ];
        assert_eq!(
            dependency_order("s", pair),
            Ordered {
                order: names(&["plain", "employees", "departments", "badge"]),
                cyclic: names(&["employees", "departments"]),
                missing: vec![],
            }
        );
        let own = vec![("employees".into(), vec![key("employees")])];
        let ordered = dependency_order("s", own);
        assert_eq!(ordered.order, names(&["employees"]));
        assert_eq!(ordered.cyclic, names(&["employees"]));
    }

    #[test]
    fn a_key_to_an_unticked_table_is_missing_not_cyclic() {
        let mut elsewhere = key("invoices");
        elsewhere.referenced_schema = "other".into();
        let tables = vec![
            ("orders".into(), vec![key("customers"), elsewhere]),
            ("lines".into(), vec![key("customers")]),
        ];
        assert_eq!(
            dependency_order("s", tables),
            Ordered {
                order: names(&["orders", "lines"]),
                cyclic: vec![],
                missing: names(&["customers"]),
            }
        );
        // In a cycle and pointing outside it: the unticked table is still
        // missing, once, and is no member of the cycle.
        let tables = vec![
            ("a".into(), vec![key("b"), key("gone")]),
            ("b".into(), vec![key("a"), key("gone")]),
        ];
        let ordered = dependency_order("s", tables);
        assert_eq!(ordered.cyclic, names(&["a", "b"]));
        assert_eq!(ordered.missing, names(&["gone"]));
    }

    fn request(structure: bool, rows: bool) -> SchemaRequest {
        SchemaRequest {
            tables: names(&["a", "b"]),
            structure,
            rows,
            partitioned: Vec::new(),
        }
    }

    #[test]
    fn rows_are_refused_with_a_sequence_the_role_may_not_read() {
        let sequences = Sequences {
            unreadable: names(&["\"s\".\"a_id_seq\""]),
            ..Sequences::default()
        };
        assert_eq!(
            sequence_refusal(&request(true, true), &sequences).as_deref(),
            Some(
                "this role may not read where these sequences stand, so the file cannot set \
                 them: \"s\".\"a_id_seq\". Grant it SELECT on them, or export the structure \
                 alone."
            )
        );
        assert!(sequence_refusal(&request(false, true), &sequences).is_some());
        assert_eq!(sequence_refusal(&request(true, false), &sequences), None);
        assert_eq!(
            sequence_refusal(&request(true, true), &Sequences::default()),
            None
        );
    }

    #[test]
    fn the_header_names_the_partitioned_tables_it_leaves_out() {
        let ordered = Ordered {
            order: names(&["a", "b"]),
            cyclic: vec![],
            missing: vec![],
        };
        let partitioned = SchemaRequest {
            partitioned: names(&["events", "odd\nDROP TABLE t; --"]),
            ..request(true, true)
        };
        assert!(
            header("s", &partitioned, &ordered, false).contains(
                "-- Only tables are written: partitioned tables and their partitions (events, odd \
                 DROP TABLE t; --), views, routines, triggers and every other object are left out.\n"
            )
        );
    }

    #[test]
    fn the_header_keeps_a_schema_name_inside_its_comment() {
        let ordered = Ordered {
            order: names(&["a", "b"]),
            cyclic: vec![],
            missing: names(&["gone\r\nDROP TABLE t; --"]),
        };
        let written = header(
            "odd\nDROP TABLE t; --",
            &request(true, true),
            &ordered,
            false,
        );
        assert!(
            written
                .lines()
                .all(|line| line.starts_with("-- ") || line.is_empty()),
            "{written}"
        );
        assert!(written.starts_with(
            "-- Schema \"odd DROP TABLE t; --\" exported by DBDelve: 2 tables, structure and rows.\n"
        ));
        assert!(written.contains("-- Referenced but not exported: gone  DROP TABLE t; --.\n"));
        let one = Ordered {
            order: names(&["a"]),
            cyclic: vec![],
            missing: vec![],
        };
        assert_eq!(
            header("s", &request(true, true), &one, false)
                .lines()
                .next(),
            Some("-- Schema \"s\" exported by DBDelve: 1 table, structure and rows.")
        );
    }

    #[test]
    fn the_header_says_what_was_written_and_what_was_not() {
        let ordered = Ordered {
            order: names(&["plain", "a", "b"]),
            cyclic: names(&["a", "b"]),
            missing: names(&["customers", "gone"]),
        };
        assert_eq!(
            header("s", &request(false, true), &ordered, true),
            "-- Schema \"s\" exported by DBDelve: 3 tables, rows only.\n\
             -- Each table was read on its own, not from one snapshot.\n\
             -- Only tables are written, and the sequences their columns own are set, not created: views, routines, triggers and every other object are left out.\n\
             -- Referenced but not exported: customers, gone.\n\
             -- These tables reference each other, so their rows may not run back in this order: a, b.\n\n"
        );
        // With the structure, the keys come after the rows, so the order holds.
        let plain = Ordered {
            missing: vec![],
            ..ordered
        };
        assert_eq!(
            header("s", &request(true, false), &plain, false),
            "-- Schema \"s\" exported by DBDelve: 3 tables, structure only.\n\
             -- Each table was read on its own, not from one snapshot.\n\
             -- Only tables are written: views, routines, triggers and every other object are left out.\n\n"
        );
        assert!(header("s", &request(true, true), &plain, true).contains(
            "-- Only tables and the sequences their columns own are written: views, routines, \
                 triggers and every other object are left out.\n"
        ));
    }
}
