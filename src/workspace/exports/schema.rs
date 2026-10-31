//! Writing a whole schema to one file of SQL: each table's `CREATE`, then each
//! table's rows, in an order the file runs back in.

use std::io::Write;

use super::*;
use crate::db::{ForeignKey, RelationKind, Structure};

impl Workspace {
    /// Every plain table of `schema`, written as one file of SQL the user
    /// picks. A view, a partition or anything else the catalog lists is left
    /// out, as the file's header says.
    ///
    /// ponytail: every plain table, with no picker and no palette command,
    /// until #188 settles where this is asked from.
    pub(crate) fn export_schema(&mut self, schema: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let (id, generation) = (profile.id.clone(), profile.generation);
        let engine = profile.config.engine();
        let tables: Vec<String> = self
            .catalog()
            .and_then(|catalog| catalog.schemas.iter().find(|each| each.name == schema))
            .map(|found| {
                found
                    .relations
                    .iter()
                    .filter(|relation| {
                        matches!(relation.kind, RelationKind::Table)
                            && relation.partition_of.is_none()
                    })
                    .map(|relation| relation.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        if tables.is_empty() {
            self.note("The schema was not exported: it has no tables.".into(), cx);
            return;
        }
        let refusal = tables.iter().find_map(|table| {
            relation_refetch(engine, &schema, table, "", &[], &[])
                .and_then(|refetch| self.cleared(refetch))
                .err()
        });
        if let Some(reason) = refusal {
            self.note(format!("The schema was not exported: {reason}"), cx);
            return;
        }
        let chosen = choose_path(&schema, FileAs::Inserts, cx);
        cx.spawn(async move |workspace, cx| {
            let Some(path) = chosen.await else {
                return;
            };
            _ = workspace.update(cx, |workspace, cx| {
                workspace.start_schema_export(&id, generation, schema, tables, path, cx);
            });
        })
        .detach();
    }

    /// Asked again once the save panel has answered, as `start_export` is.
    fn start_schema_export(
        &mut self,
        id: &str,
        generation: u64,
        schema: String,
        tables: Vec<String>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.issued_to(id, generation) else {
            return;
        };
        let engine = profile.config.engine();
        let refusal = match &profile.session.exporting {
            Some(_) => Some("an export of every row is already running on this connection."),
            None => tables.iter().find_map(|table| {
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
                    connection, &schema, &tables, read_only, engine, path, progress,
                )
            },
            cx,
        );
    }
}

/// One table as the file needs it, read before anything is written.
struct Table {
    name: String,
    ddl: String,
    structure: Structure,
}

/// Every table of `tables` in `schema`, written to `path` as their `CREATE`
/// statements and then their rows, a table always after the ones it points at.
/// The rows written, all tables told.
///
/// Everything that can refuse is asked before the file is opened, so a refusal
/// leaves whatever was at `path` as it was.
pub(super) fn export_schema(
    connection: &Connection,
    schema: &str,
    tables: &[String],
    read_only: bool,
    engine: Engine,
    path: &Path,
    progress: &ExportProgress,
) -> Result<usize, Ended> {
    let refused = |reason: String| Ended::Failed(format!("The schema was not exported: {reason}"));
    let (asking, named, names) = (connection.clone(), schema.to_string(), tables.to_vec());
    // On a thread of its own, as `knowing_the_table` is: a read behind a
    // schema lock must not hold a Cancel up.
    let read = unless_cancelled(progress, move || read_tables(&asking, &named, &names))?
        .ok_or_else(|| refused("the read of its tables stopped.".into()))?
        .map_err(refused)?;
    let keys = read
        .iter()
        .map(|table| (table.name.clone(), table.structure.foreign_keys.clone()))
        .collect();
    let order = dependency_order(schema, keys).map_err(|caught| {
        refused(format!(
            "{} reference each other or themselves, so no order of them runs back.",
            caught.join(", ")
        ))
    })?;
    let mut planned = Vec::with_capacity(order.len());
    for name in &order {
        let table = read
            .iter()
            .find(|table| &table.name == name)
            .expect("ordered from what was read");
        let refetch = relation_refetch(engine, schema, name, "", &[], &[])
            .map_err(|reason| refused(reason.into()))?;
        let target = InsertTarget {
            relation: Some((schema.to_string(), name.clone())),
            ..InsertTarget::default()
        }
        .knowing(engine, &table.structure.columns);
        planned.push((table, refetch, target));
    }

    write_replacing(path, |out| {
        let failed = could_not_write(path);
        out.write_all(header(schema, planned.len()).as_bytes())
            .map_err(&failed)?;
        for (table, _, _) in &planned {
            write!(out, "{}\n\n", table.ddl).map_err(&failed)?;
        }
        let mut written = 0;
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
        match progress.cancelled.load(Ordering::Relaxed) {
            true => Err(Ended::Cancelled),
            false => Ok(written),
        }
    })
}

fn read_tables(
    connection: &Connection,
    schema: &str,
    tables: &[String],
) -> Result<Vec<Table>, String> {
    let own = connection.apart().map_err(|error| error.to_string())?;
    tables
        .iter()
        .map(|name| {
            let failed = |error: DbError| format!("could not read {name}: {error}");
            Ok(Table {
                structure: own.structure(schema, name).map_err(failed)?,
                ddl: own.ddl(schema, name, RelationKind::Table).map_err(failed)?,
                name: name.clone(),
            })
        })
        .collect()
}

/// What the file says of itself, every line a comment. A name holding a line
/// break would end the comment and run the rest as SQL, so it reads as a space.
///
/// ponytail: the tables are read one at a time and only tables are written,
/// until #188 settles a snapshot and the rest of a schema's objects.
fn header(schema: &str, tables: usize) -> String {
    let schema = schema.replace(['\r', '\n'], " ");
    let count = match tables {
        1 => "1 table".to_string(),
        tables => format!("{tables} tables"),
    };
    format!(
        "-- Schema \"{schema}\" exported by DBDelve: {count}.\n\
         -- Each table was read on its own, not from one snapshot.\n\
         -- Only tables are written: views, sequences and every other object are left out.\n\n"
    )
}

/// The `tables` of `schema`, each after the ones its keys point at, or the ones
/// caught in a cycle, which no order runs back. A key to anything this export
/// does not write holds nothing back: the file cannot create it either way.
/// Where nothing forces an order, the tables keep the one they came in, so the
/// file reads as the catalog does.
///
/// ponytail: a cycle, a table pointing at itself among them, is refused until
/// #188 settles how a file replays one (its keys added after the rows, or the
/// checks turned off inside the file).
pub(super) fn dependency_order(
    schema: &str,
    tables: Vec<(String, Vec<ForeignKey>)>,
) -> Result<Vec<String>, Vec<String>> {
    let names: Vec<String> = tables.iter().map(|(name, _)| name.clone()).collect();
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
            return Err(waiting.into_iter().map(|(name, _)| name).collect());
        }
    }
    Ok(placed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(table: &str) -> ForeignKey {
        ForeignKey {
            column: "ref_id".into(),
            referenced_schema: "s".into(),
            referenced_table: table.into(),
            referenced_column: "id".into(),
        }
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
            Ok(vec![
                "products".into(),
                "customers".into(),
                "orders".into(),
                "lines".into()
            ])
        );
    }

    #[test]
    fn tables_that_reference_each_other_or_themselves_are_named_and_refused() {
        let pair = vec![
            ("plain".into(), vec![]),
            ("employees".into(), vec![key("departments")]),
            ("departments".into(), vec![key("employees")]),
        ];
        assert_eq!(
            dependency_order("s", pair),
            Err(vec!["employees".into(), "departments".into()])
        );
        let own = vec![("employees".into(), vec![key("employees")])];
        assert_eq!(dependency_order("s", own), Err(vec!["employees".into()]));
    }

    #[test]
    fn the_header_keeps_a_schema_name_inside_its_comment() {
        let written = header("odd\nDROP TABLE t; --", 2);
        assert!(
            written
                .lines()
                .all(|line| line.starts_with("-- ") || line.is_empty()),
            "{written}"
        );
        assert!(
            written
                .starts_with("-- Schema \"odd DROP TABLE t; --\" exported by DBDelve: 2 tables.\n")
        );
        assert!(written.contains("not from one snapshot"));
        assert_eq!(
            header("s", 1).lines().next(),
            Some("-- Schema \"s\" exported by DBDelve: 1 table.")
        );
    }

    #[test]
    fn a_key_to_anything_outside_the_export_does_not_hold_a_table_back() {
        let mut elsewhere = key("customers");
        elsewhere.referenced_schema = "other".into();
        let tables = vec![("orders".into(), vec![elsewhere, key("a_view")])];
        assert_eq!(dependency_order("s", tables), Ok(vec!["orders".into()]));
    }
}
