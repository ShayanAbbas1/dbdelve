//! Writing a whole schema to one file of SQL: each table's `CREATE`, then each
//! table's rows, in an order the file runs back in.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::Write;

use super::*;
use crate::db::{ExportDdl, ForeignKey, ForeignKeyReplay, Sequences, Structure, Target};

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
        // The schema is the connection's own database's: the explorer offers
        // the export on no other.
        let routed = profile.route(None);
        let refusal = refusal.or((!routed.is_available()).then_some("the connection is not open."));
        if let Some(reason) = refusal {
            profile.session.notice = Some(format!("The schema was not exported: {reason}"));
            cx.notify();
            return;
        }
        self.run_export(
            id,
            generation,
            routed,
            path,
            FileAs::Inserts,
            request.rows,
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
    let export = Target::built_in(engine)
        .exports_schema()
        .ok_or_else(|| refused("this engine's structure does not run back as written.".into()))?;
    let (asking, named, names) = (
        connection.clone(),
        schema.to_string(),
        request.tables.clone(),
    );
    // On a thread of its own, as `knowing_the_table` is: a read behind a
    // schema lock must not hold a Cancel up.
    // `own` is the export's session for what it reads of the catalog, apart
    // from the one a query tab's transaction may be open on.
    let (read, sequences, own) =
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
    let mut ordered = export_order(schema, keys, request.rows);
    if export.foreign_keys == ForeignKeyReplay::RefusesCycles
        && request.structure
        && !ordered.cyclic.is_empty()
    {
        return Err(refused(cycle_refusal(&ordered.cyclic)));
    }
    // With the checks off or none enforced, no order of the rows fails.
    if matches!(
        export.foreign_keys,
        ForeignKeyReplay::ChecksOff | ForeignKeyReplay::NotEnforced
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
            unqualified: !export.names.qualifies(schema),
            ..InsertTarget::default()
        }
        .knowing(engine, &table.structure.columns);
        planned.push((table, refetch, target));
    }

    // An `OWNED BY` ties a sequence to a column the structure creates, so it
    // goes with the structure; a `setval` sets one where the rows left it.
    let (before_tables, owned_by): (&[String], &[String]) = match request.structure {
        true => (&sequences.before_tables, &sequences.owned_by),
        false => (&[], &[]),
    };
    let checks = match export.foreign_keys {
        ForeignKeyReplay::ChecksOff => Some(
            crate::db::checks_off(engine)
                .ok_or_else(|| refused("this engine has no way to turn its checks off.".into()))?,
        ),
        _ => None,
    };
    let sequenced = !before_tables.is_empty()
        || (request.structure && !sequences.identity.is_empty())
        || (request.rows && !sequences.values.is_empty());

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
            for statement in &sequences.identity {
                write!(out, "{statement}\n\n").map_err(&failed)?;
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
        if request.rows && !sequences.values.is_empty() {
            // Read again now the rows are, so a row another session added
            // meanwhile is behind where the file sets its sequence.
            let again = own
                .export_sequences(schema, &request.tables)
                .map_err(|error| refused(format!("could not read its sequences: {error}")))?;
            if let Some(reason) = sequence_refusal(request, &again).or_else(|| {
                (request.structure && redefined(&sequences, &again)).then(|| {
                    "its sequences were changed while it was written, so the file would \
                     create them as they were and set them as they are. Export it again."
                        .to_string()
                })
            }) {
                return Err(refused(reason));
            }
            for statement in &again.values {
                write!(out, "{statement}\n\n").map_err(&failed)?;
            }
        }
        for statement in owned_by {
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

/// Why an engine that refuses cycles (`ForeignKeyReplay::RefusesCycles`)
/// writes no structure for `cyclic`.
pub(super) fn cycle_refusal(cyclic: &[String]) -> String {
    format!(
        "{} reference each other or themselves, so no order of them runs back.",
        cyclic.join(", ")
    )
}

/// Whether `again`, read after the rows, names other sequences than `first`,
/// which the file created them from, or defines one otherwise: a sequence a
/// default took up meanwhile would be set and never created, one dropped
/// created and never set, and a `MAXVALUE` raised and passed would be set
/// beyond the one created. Only the values may have moved.
fn redefined(first: &Sequences, again: &Sequences) -> bool {
    first.before_tables != again.before_tables
        || first.identity != again.identity
        || first.owned_by != again.owned_by
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
) -> Result<(Vec<Table>, Sequences, Connection), String> {
    let own = connection
        .apart_unpathed()
        .map_err(|error| error.to_string())?;
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
        .export_sequences(schema, tables)
        .map_err(|error| format!("could not read its sequences: {error}"))?;
    Ok((read, sequences, own))
}

/// What the file says of itself, every line a comment. A name holding a line
/// break would end the comment and run the rest as SQL, so it reads as a space.
///
/// ponytail: the tables are read one at a time, not from one snapshot, and
/// only tables and the sequences their columns use are written. A snapshot
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
        (true, true) => "Only tables and the sequences their columns use are written",
        (true, false) => {
            "Only tables are written, and the sequences their columns use are set, not created"
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

/// [`dependency_order`] for a file that writes `rows` or not. Without rows, a
/// table's key to itself is created with it and holds nothing back.
pub(super) fn export_order(
    schema: &str,
    mut tables: Vec<(String, Vec<ForeignKey>)>,
    rows: bool,
) -> Ordered {
    if !rows {
        for (name, keys) in &mut tables {
            keys.retain(|key| key.referenced_schema != schema || key.referenced_table != *name);
        }
    }
    dependency_order(schema, tables)
}

/// The `tables` of `schema`, each after the ones its keys point at. Tables in a
/// cycle, which no order runs back, go together, after what the cycle points
/// at and before what points at it; `cyclic` names the members of the cycles,
/// a table referencing itself included. A key to a table this export does not
/// write holds nothing back, since the file cannot create it either way, and
/// is reported in `missing`. Where nothing forces an order, the tables keep
/// the one they came in, so the file reads as the catalog does.
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

    // The keys between the tables written, by position.
    let points_at: Vec<Vec<usize>> = tables
        .iter()
        .map(|(_, keys)| {
            keys.iter()
                .filter(|key| key.referenced_schema == schema)
                .filter_map(|key| names.iter().position(|name| *name == key.referenced_table))
                .collect()
        })
        .collect();
    // `reaches[from][to]`: following keys from `from` leads to `to`, which
    // for `to == from` means `from` is in a cycle.
    let reaches: Vec<Vec<bool>> = (0..names.len())
        .map(|from| {
            let mut seen = vec![false; names.len()];
            let mut stack = points_at[from].clone();
            while let Some(table) = stack.pop() {
                if !seen[table] {
                    seen[table] = true;
                    stack.extend(&points_at[table]);
                }
            }
            seen
        })
        .collect();
    // Each table's cycle, named by its first member: the tables it leads to
    // that lead back to it, or itself alone.
    let component: Vec<usize> = (0..names.len())
        .map(|table| {
            (0..table)
                .find(|&other| reaches[table][other] && reaches[other][table])
                .unwrap_or(table)
        })
        .collect();

    // Each cycle taken as one, named by its first member: its members in the
    // order they came, how many other cycles it points at that are not placed
    // yet, and the cycles that point at it.
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); names.len()];
    for table in 0..names.len() {
        members[component[table]].push(table);
    }
    let mut waiting = vec![0usize; names.len()];
    let mut pointed_at_by: Vec<Vec<usize>> = vec![Vec::new(); names.len()];
    for from in (0..names.len()).filter(|&table| component[table] == table) {
        let mut targets: Vec<usize> = members[from]
            .iter()
            .flat_map(|&member| &points_at[member])
            .map(|&to| component[to])
            .filter(|&to| to != from)
            .collect();
        targets.sort_unstable();
        targets.dedup();
        waiting[from] = targets.len();
        for to in targets {
            pointed_at_by[to].push(from);
        }
    }
    // The first table, in the order they came, whose cycle points only at
    // tables already placed or in it, is the first member of the ready cycle
    // with the lowest name. One is always ready: the cycles, each taken as
    // one, point at one another in no cycle.
    let mut ready: BinaryHeap<Reverse<usize>> = (0..names.len())
        .filter(|&table| component[table] == table && waiting[table] == 0)
        .map(Reverse)
        .collect();
    let mut order = Vec::with_capacity(names.len());
    while let Some(Reverse(cycle)) = ready.pop() {
        order.extend(members[cycle].iter().map(|&member| names[member].clone()));
        for &from in &pointed_at_by[cycle] {
            waiting[from] -= 1;
            if waiting[from] == 0 {
                ready.push(Reverse(from));
            }
        }
    }
    debug_assert_eq!(order.len(), names.len(), "the cycles form no cycle");

    let cyclic = (0..names.len())
        .filter(|&table| reaches[table][table])
        .map(|table| names[table].clone())
        .collect();
    Ordered {
        order,
        cyclic,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{DdlNames, ForeignKeyReplay, SchemaExport};

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
            assert_eq!(
                Target::built_in(engine).exports_schema(),
                None,
                "{engine:?}"
            );
        }
        let export = |names, foreign_keys| {
            Some(SchemaExport {
                names,
                foreign_keys,
            })
        };
        for engine in [Engine::Postgres, Engine::SqlServer] {
            assert_eq!(
                Target::built_in(engine).exports_schema(),
                export(DdlNames::Qualified, ForeignKeyReplay::AddedAfterRows),
                "{engine:?}"
            );
        }
        assert_eq!(
            Target::built_in(Engine::ClickHouse).exports_schema(),
            export(DdlNames::Qualified, ForeignKeyReplay::NotEnforced)
        );
        for engine in [Engine::MySql, Engine::MariaDb] {
            assert_eq!(
                Target::built_in(engine).exports_schema(),
                export(DdlNames::Unqualified, ForeignKeyReplay::ChecksOff),
                "{engine:?}"
            );
        }
        // Every engine that turns its checks off has the statements to.
        for engine in Engine::ALL {
            let turns_off = Target::built_in(engine)
                .exports_schema()
                .is_some_and(|export| export.foreign_keys == ForeignKeyReplay::ChecksOff);
            assert_eq!(
                crate::db::checks_off(engine).is_some(),
                turns_off,
                "{engine:?}"
            );
        }
        assert_eq!(
            Target::built_in(Engine::DuckDb).exports_schema(),
            export(
                DdlNames::QualifiedOutside("main"),
                ForeignKeyReplay::RefusesCycles
            )
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
    fn sequences_redefined_between_the_reads_are_told_from_moved_ones() {
        let first = Sequences {
            before_tables: names(&["CREATE SEQUENCE s MAXVALUE 10;"]),
            owned_by: names(&["ALTER SEQUENCE s OWNED BY t.id;"]),
            values: names(&["SELECT setval('s', 5, true);"]),
            ..Sequences::default()
        };
        let moved = Sequences {
            values: names(&["SELECT setval('s', 9, true);"]),
            ..first.clone()
        };
        assert!(!redefined(&first, &moved));
        let raised = Sequences {
            before_tables: names(&["CREATE SEQUENCE s MAXVALUE 100;"]),
            values: names(&["SELECT setval('s', 50, true);"]),
            ..first.clone()
        };
        assert!(redefined(&first, &raised));
        let taken_up = Sequences {
            before_tables: names(&["CREATE SEQUENCE s MAXVALUE 10;", "CREATE SEQUENCE u;"]),
            ..first.clone()
        };
        assert!(redefined(&first, &taken_up));
        let dropped = Sequences::default();
        assert!(redefined(&first, &dropped));
    }

    #[test]
    fn a_long_chain_listed_backwards_orders_without_rescanning() {
        // Each table points at the next, so every table but the last waits:
        // a planner that looked for the first ready table afresh each round
        // would take cubic time here.
        let count = 2_000;
        let name = |index: usize| format!("t{index:04}");
        let tables = (0..count)
            .map(|index| {
                let keys = match index + 1 < count {
                    true => vec![key(&name(index + 1))],
                    false => vec![],
                };
                (name(index), keys)
            })
            .collect();
        let started = std::time::Instant::now();
        let ordered = dependency_order("s", tables);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(
            ordered.order,
            (0..count).rev().map(name).collect::<Vec<_>>()
        );
        assert!(ordered.cyclic.is_empty());
    }

    #[test]
    fn a_table_that_points_at_a_cycle_comes_after_it() {
        // `z_orders` references itself, and `a_items` comes first in the
        // catalog: its rows still need `z_orders`'s in first.
        let tables = vec![
            ("a_items".into(), vec![key("z_orders")]),
            ("z_orders".into(), vec![key("z_orders")]),
        ];
        assert_eq!(
            dependency_order("s", tables),
            Ordered {
                order: names(&["z_orders", "a_items"]),
                cyclic: names(&["z_orders"]),
                missing: vec![],
            }
        );
        // A cycle goes after what it points at, its members together.
        let tables = vec![
            ("lines".into(), vec![key("b")]),
            ("a".into(), vec![key("b"), key("base")]),
            ("b".into(), vec![key("a")]),
            ("base".into(), vec![]),
        ];
        assert_eq!(
            dependency_order("s", tables),
            Ordered {
                order: names(&["base", "a", "b", "lines"]),
                cyclic: names(&["a", "b"]),
                missing: vec![],
            }
        );
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
             -- Only tables are written, and the sequences their columns use are set, not created: views, routines, triggers and every other object are left out.\n\
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
            "-- Only tables and the sequences their columns use are written: views, routines, \
                 triggers and every other object are left out.\n"
        ));
    }
}
