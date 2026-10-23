//! `read_xlsx`, a DuckDB table function over a spreadsheet.
//!
//! DuckDB reads CSV, Parquet and JSON itself; its own excel extension is not
//! one the bundled build can link in, so this is the spreadsheet reader, in
//! Rust, over `calamine`. Registered on every DuckDB connection `db::duckdb`
//! opens, so a sheet is queried like any other file:
//!
//! ```sql
//! SELECT * FROM read_xlsx('/path/facturas.xlsx');
//! SELECT * FROM read_xlsx('/path/facturas.xlsx', sheet := 'Enero', header := false);
//! ```
//!
//! The first row names the columns unless `header := false`. A column whose
//! every filled cell is a number comes back `DOUBLE`; anything else, dates
//! included, comes back as text the way the sheet shows it, for a `CAST` to
//! make into what the user means. Guessing a date's type from a spreadsheet is
//! how a reader gets it wrong quietly.

use std::error::Error;
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};

use calamine::{Data, Reader, open_workbook_auto};
use duckdb::core::{DataChunkHandle, Inserter, LogicalTypeHandle, LogicalTypeId};
use duckdb::vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab};

pub(super) struct ReadXlsx;

pub(super) struct Sheet {
    numeric: Vec<bool>,
    rows: Vec<Vec<Data>>,
}

pub(super) struct Cursor {
    next: AtomicUsize,
}

impl VTab for ReadXlsx {
    type InitData = Cursor;
    type BindData = Sheet;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        let path = bind.get_parameter(0).to_string();
        let header = bind
            .get_named_parameter("header")
            .is_none_or(|header| header.to_string() == "true");
        let mut workbook = open_workbook_auto(&path)?;
        let sheet = match bind.get_named_parameter("sheet") {
            Some(sheet) => sheet.to_string(),
            None => workbook
                .sheet_names()
                .first()
                .cloned()
                .ok_or_else(|| format!("{path} has no sheets"))?,
        };
        let range = workbook.worksheet_range(&sheet)?;

        let mut rows: Vec<Vec<Data>> = range.rows().map(<[Data]>::to_vec).collect();
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        let names: Vec<String> = match header && !rows.is_empty() {
            true => rows.remove(0).iter().map(|cell| cell.to_string()).collect(),
            false => Vec::new(),
        };

        let numeric: Vec<bool> = (0..width)
            .map(|col| {
                let mut filled = rows
                    .iter()
                    .filter_map(|row| row.get(col))
                    .filter(|cell| !matches!(cell, Data::Empty))
                    .peekable();
                filled.peek().is_some()
                    && filled.all(|cell| matches!(cell, Data::Int(_) | Data::Float(_)))
            })
            .collect();

        let mut taken = Vec::with_capacity(width);
        for (col, &numeric) in numeric.iter().enumerate() {
            let name = names
                .get(col)
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("column{}", col + 1));
            // DuckDB refuses two columns of one name, and a sheet's header row
            // repeats itself as often as anyone typed it.
            let mut unique = name.clone();
            let mut suffix = 2;
            while taken.contains(&unique) {
                unique = format!("{name}_{suffix}");
                suffix += 1;
            }
            let kind = match numeric {
                true => LogicalTypeId::Double,
                false => LogicalTypeId::Varchar,
            };
            bind.add_result_column(&unique, LogicalTypeHandle::from(kind));
            taken.push(unique);
        }
        Ok(Sheet { numeric, rows })
    }

    fn init(_: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        Ok(Cursor {
            next: AtomicUsize::new(0),
        })
    }

    fn func(
        func: &TableFunctionInfo<Self>,
        output: &mut DataChunkHandle,
    ) -> Result<(), Box<dyn Error>> {
        let sheet = func.get_bind_data();
        let cursor = func.get_init_data();
        let columns = sheet.numeric.len();
        if columns == 0 {
            output.set_len(0);
            return Ok(());
        }
        let capacity = output.flat_vector(0).capacity();
        let start = cursor.next.fetch_add(capacity, Ordering::Relaxed);
        let rows = sheet.rows.get(start..).unwrap_or_default();
        let rows = &rows[..rows.len().min(capacity)];

        for (col, &numeric) in sheet.numeric.iter().enumerate() {
            let mut vector = output.flat_vector(col);
            for (at, row) in rows.iter().enumerate() {
                match (row.get(col), numeric) {
                    (None | Some(Data::Empty), _) => vector.set_null(at),
                    (Some(Data::Int(value)), true) => unsafe {
                        vector.as_mut_slice::<f64>()[at] = *value as f64;
                    },
                    (Some(Data::Float(value)), true) => unsafe {
                        vector.as_mut_slice::<f64>()[at] = *value;
                    },
                    (Some(cell), _) => vector.insert(at, CString::new(text(cell))?),
                }
            }
        }
        output.set_len(rows.len());
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![LogicalTypeHandle::from(LogicalTypeId::Varchar)])
    }

    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![
            (
                "sheet".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ),
            (
                "header".to_string(),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
            ),
        ])
    }
}

/// A cell as the sheet shows it. A date as ISO 8601, which `CAST` reads; a
/// whole float without its `.0`, the way a spreadsheet prints one.
fn text(cell: &Data) -> String {
    match cell {
        Data::DateTime(value) => match value.as_datetime() {
            Some(moment) if moment.time().to_string() == "00:00:00" => moment.date().to_string(),
            Some(moment) => moment.to_string(),
            None => value.to_string(),
        },
        // NUL ends a C string, and DuckDB takes text as one.
        other => other.to_string().replace('\0', ""),
    }
}
