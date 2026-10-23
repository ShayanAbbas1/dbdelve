//! Opening a data file: a CSV, a Parquet or JSON file, or a spreadsheet,
//! picked from the palette or dropped on the window.
//!
//! There is no connection to think about. The files open in a DuckDB profile
//! held in memory -- one that is found if it exists and made if it does not --
//! each in a query tab of its own whose statement is the one DBDelve wrote to
//! read it, on screen and the user's to change: `SELECT * FROM read_csv(…)`.
//! The file itself is only ever read.

use std::path::{Path, PathBuf};

use gpui::{ExternalPaths, PathPromptOptions};

use super::*;

use crate::connection_form::starting_mode;
use crate::db::DUCKDB_IN_MEMORY;

impl Workspace {
    /// The native file picker, then [`Workspace::open_data_files`].
    pub(crate) fn pick_data_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let picked = picked.await;
            _ = workspace.update_in(cx, |workspace, window, cx| match picked {
                Ok(Ok(Some(paths))) => workspace.open_data_files(paths, window, cx),
                Ok(Err(error)) => workspace.note(error.to_string(), cx),
                Ok(Ok(None)) | Err(_) => {}
            });
        })
        .detach();
    }

    pub(crate) fn drop_data_files(
        &mut self,
        dropped: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_data_files(dropped.paths().to_vec(), window, cx);
    }

    pub(crate) fn open_data_files(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let statements: Vec<String> = paths.iter().filter_map(|path| reading(path)).collect();
        if statements.is_empty() {
            self.note(
                "DBDelve opens .csv, .tsv, .parquet, .json and .xlsx files.".to_string(),
                cx,
            );
            return;
        }

        let index = match self.profiles.iter().position(|profile| {
            matches!(&profile.config, ConnectionConfig::DuckDb { path, .. } if path == DUCKDB_IN_MEMORY)
        }) {
            Some(index) => index,
            None => self.create_profile(
                "Files".to_string(),
                ConnectionConfig::DuckDb {
                    path: DUCKDB_IN_MEMORY.to_string(),
                    statement_timeout: 0,
                },
                None,
                starting_mode(None),
                Origin::Form,
                window,
                cx,
            ),
        };
        if index != self.active {
            self.activate(index, cx);
        } else {
            self.connect_active(cx);
        }
        let Some(profile_id) = self.profile().map(|profile| profile.id.clone()) else {
            return;
        };

        for sql in statements {
            self.new_query(&NewQuery, window, cx);
            let Some(tab) = self
                .profile()
                .and_then(|profile| profile.session.active_query_tab())
            else {
                return;
            };
            let (id, editor) = (tab.id, tab.editor.clone());
            editor.update(cx, |editor, cx| editor.set_value(sql.clone(), window, cx));
            self.runs_on_connect.push((profile_id.clone(), id, sql));
        }
        self.run_waiting_files(cx);
    }

    /// Run what [`Workspace::open_data_files`] left waiting for its profile to
    /// connect, once it has. Anything for a profile no longer in front is
    /// dropped: its statement is still in its tab, a Run away.
    pub(crate) fn run_waiting_files(&mut self, cx: &mut Context<Self>) {
        let Some((id, connected)) = self
            .profile()
            .map(|profile| (profile.id.clone(), profile.connection().is_some()))
        else {
            return;
        };
        if !connected {
            return;
        }
        let waiting = std::mem::take(&mut self.runs_on_connect);
        for (profile, tab, sql) in waiting {
            if profile == id {
                self.execute_sql(sql, Tab::Query(tab), cx);
            }
        }
    }
}

/// The statement that reads a file, by its extension, or `None` for a file
/// this does not know how to read.
fn reading(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let function = match extension.as_str() {
        "csv" | "tsv" | "txt" => "read_csv",
        "parquet" => "read_parquet",
        "json" | "jsonl" | "ndjson" => "read_json",
        "xlsx" | "xlsm" | "xls" | "ods" => "read_xlsx",
        _ => return None,
    };
    let path = Engine::DuckDb.quote_literal(&path.to_string_lossy());
    Some(format!("SELECT * FROM {function}({path});"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_read_by_the_function_its_extension_names() {
        assert_eq!(
            reading(Path::new("/data/o'hara.CSV")).as_deref(),
            Some("SELECT * FROM read_csv('/data/o''hara.CSV');")
        );
        assert_eq!(
            reading(Path::new("/data/facturas.xlsx")).as_deref(),
            Some("SELECT * FROM read_xlsx('/data/facturas.xlsx');")
        );
        assert_eq!(reading(Path::new("/data/notes.md")), None);
        assert_eq!(reading(Path::new("/data/no-extension")), None);
    }
}
