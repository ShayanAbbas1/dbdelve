//! How a checked file becomes statements: the chunks, and the text of each.

use std::ops::{Range, RangeInclusive};
use std::sync::Arc;

use super::{Checked, CheckedRow, OnDuplicate, Options, Transaction, Value};
use crate::db::Engine;

/// Under SQL Server's 1,000-row VALUES cap, the tightest of the engines.
pub(crate) const CHUNK_ROWS: usize = 500;
/// So no statement meets a server's packet or text limit.
pub(crate) const CHUNK_BYTES: usize = 512 * 1024;

fn weight(row: &CheckedRow) -> usize {
    row.values
        .iter()
        .flatten()
        .map(|v| {
            4 + match v {
                Value::Null => 0,
                Value::Bool(b) => {
                    if *b {
                        4
                    } else {
                        5
                    }
                }
                Value::Json(json) => json.to_string().len(),
                Value::Text(text) | Value::Number(text) | Value::Date { text, .. } => text.len(),
            }
        })
        .sum()
}

fn same_present(a: &CheckedRow, b: &CheckedRow) -> bool {
    a.values.len() == b.values.len()
        && a.values
            .iter()
            .zip(&b.values)
            .all(|(x, y)| x.is_some() == y.is_some())
}

/// An oversize row is a chunk alone. With `split_on_present`, a chunk holds
/// rows that share the same present columns, since a SQL `INSERT` names one
/// column list.
pub(crate) fn chunks(
    rows: &[CheckedRow],
    max_rows: usize,
    max_bytes: usize,
    split_on_present: bool,
) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (i, row) in rows.iter().enumerate() {
        let size = weight(row);
        let closes = i > start
            && (i - start == max_rows
                || bytes + size > max_bytes
                || (split_on_present && !same_present(&rows[start], row)));
        if closes {
            out.push(start..i);
            start = i;
            bytes = 0;
        }
        bytes += size;
    }
    if start < rows.len() {
        out.push(start..rows.len());
    }
    out
}

pub(crate) struct Plan {
    pub(crate) engine: Engine,
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) checked: Arc<Checked>,
    pub(crate) chunks: Vec<Range<usize>>,
    pub(crate) skip: bool,
    pub(crate) begin: Option<&'static str>,
}

impl Plan {
    pub(crate) fn new(
        engine: Engine,
        schema: String,
        table: String,
        checked: Arc<Checked>,
        options: &Options,
    ) -> Self {
        let begin = match options.transaction {
            Transaction::AllOrNothing => engine.all_or_nothing().ok(),
            Transaction::CommitAsItGoes => None,
        };
        Self {
            engine,
            schema,
            table,
            chunks: chunks(
                &checked.rows,
                CHUNK_ROWS,
                CHUNK_BYTES,
                !engine.takes_new_fields(),
            ),
            skip: options.on_duplicate == OnDuplicate::Skip && engine.skips_duplicates(),
            begin,
            checked,
        }
    }

    pub(crate) fn statement(&self, chunk: usize) -> Result<String, String> {
        let rows = &self.checked.rows[self.chunks[chunk].clone()];
        // A document engine leaves a `None` out of its document, so every column goes
        // to it; a SQL insert names only the columns the chunk's rows all hold.
        let used: Vec<usize> = (0..self.checked.columns.len())
            .filter(|&k| self.engine.takes_new_fields() || rows[0].values[k].is_some())
            .collect();
        let columns: Vec<(&str, &str)> = used
            .iter()
            .map(|&k| {
                let (name, data_type) = &self.checked.columns[k];
                (name.as_str(), data_type.as_str())
            })
            .collect();
        let values: Vec<Vec<Option<&Value>>> = rows
            .iter()
            .map(|row| used.iter().map(|&k| row.values[k].as_ref()).collect())
            .collect();
        crate::sql::insert_rows(
            self.engine,
            &self.schema,
            &self.table,
            &columns,
            &values,
            self.skip,
        )
    }

    pub(crate) fn lines(&self, chunk: usize) -> RangeInclusive<usize> {
        let range = &self.chunks[chunk];
        self.checked.rows[range.start].line..=self.checked.rows[range.end - 1].line
    }

    pub(crate) fn rows(&self) -> usize {
        self.checked.rows.len()
    }

    pub(crate) fn preview_sql(&self) -> Result<String, String> {
        if self.chunks.is_empty() {
            return Err("There are no rows to insert.".into());
        }
        let mut parts = Vec::new();
        parts.extend(self.begin.map(|begin| format!("{begin};")));
        parts.push(format!("{};", self.statement(0)?));
        if self.begin.is_some() {
            parts.push("COMMIT;".into());
        }
        Ok(parts.join("\n"))
    }

    pub(crate) fn script(&self) -> Result<String, String> {
        let mut parts = Vec::new();
        parts.extend(self.begin.map(|begin| format!("{begin};")));
        for chunk in 0..self.chunks.len() {
            parts.push(format!("{};", self.statement(chunk)?));
        }
        if self.begin.is_some() {
            parts.push("COMMIT;".into());
        }
        Ok(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(present: &[bool], len: usize) -> CheckedRow {
        CheckedRow {
            line: 1,
            values: present
                .iter()
                .map(|&p| p.then(|| Value::Text("x".repeat(len))))
                .collect(),
        }
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)] // one chunk is the answer being asserted
    fn chunks_close_on_rows_bytes_or_a_change_in_present_columns() {
        let same: Vec<_> = (0..5).map(|_| row(&[true, true], 1)).collect();
        assert_eq!(chunks(&same, 2, CHUNK_BYTES, true), [0..2, 2..4, 4..5]);
        let wide: Vec<_> = (0..3).map(|_| row(&[true], 300)).collect();
        assert_eq!(chunks(&wide, 10, 512, true), [0..1, 1..2, 2..3]);
        let mixed = vec![
            row(&[true, true], 1),
            row(&[true, true], 1),
            row(&[true, false], 1),
            row(&[true, false], 1),
            row(&[true, true], 1),
        ];
        assert_eq!(chunks(&mixed, 10, CHUNK_BYTES, true), [0..2, 2..4, 4..5]);
        assert_eq!(chunks(&mixed, 10, CHUNK_BYTES, false), [0..5]);
    }

    fn plan(engine: Engine, rows: Vec<CheckedRow>, chunks: Vec<Range<usize>>) -> Plan {
        let width = rows[0].values.len();
        Plan {
            engine,
            schema: "public".into(),
            table: "t".into(),
            checked: Arc::new(Checked {
                columns: (0..width)
                    .map(|i| (format!("c{i}"), "text".into()))
                    .collect(),
                sources: (0..width).collect(),
                rows,
                problems: Vec::new(),
            }),
            chunks,
            skip: false,
            begin: Some("BEGIN"),
        }
    }

    fn one_chunk(engine: Engine, rows: Vec<CheckedRow>) -> Plan {
        let end = rows.len();
        plan(engine, rows, std::iter::once(0..end).collect())
    }

    #[test]
    fn a_plan_says_which_lines_a_chunk_covers_and_previews_its_brackets() {
        let rows = (2..=5)
            .map(|line| CheckedRow {
                line,
                values: vec![Some(Value::Text("x".into()))],
            })
            .collect();
        let mut plan = plan(Engine::Postgres, rows, vec![0..2, 2..4]);
        plan.checked = Arc::new(Checked {
            columns: vec![("a".into(), "text".into())],
            ..Arc::into_inner(plan.checked).unwrap()
        });
        assert_eq!(plan.lines(1), 4..=5);
        assert_eq!(plan.rows(), 4);
        assert_eq!(
            plan.preview_sql().unwrap(),
            "BEGIN;\nINSERT INTO \"public\".\"t\" (\"a\") VALUES ('x'),\n('x');\nCOMMIT;"
        );
        let script = plan.script().unwrap();
        assert_eq!(script.matches("INSERT INTO").count(), 2);
        assert!(script.starts_with("BEGIN;") && script.ends_with("COMMIT;"));
        plan.begin = None;
        assert!(!plan.preview_sql().unwrap().contains("COMMIT"));
    }

    #[test]
    fn a_sql_chunk_names_its_present_columns_and_mongo_names_them_all() {
        let rows = || vec![row(&[true, false], 1), row(&[true, false], 1)];
        let sql = one_chunk(Engine::Postgres, rows()).statement(0).unwrap();
        assert!(
            sql.contains("(\"c0\") VALUES") && !sql.contains("c1"),
            "{sql}"
        );

        let mixed = vec![row(&[true, false], 1), row(&[false, true], 1)];
        let mongo = one_chunk(Engine::MongoDb, mixed).statement(0).unwrap();
        assert_eq!(
            mongo,
            "db.getCollection(\"t\").insertMany([{\"c0\": \"x\"},\n{\"c1\": \"x\"}])"
        );
    }
}
