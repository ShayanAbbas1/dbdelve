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
                ConnectionConfig::duckdb(DUCKDB_IN_MEMORY.to_string(), 0),
                None,
                starting_mode(None),
                Origin::Form,
                window,
                cx,
            ),
        };
        // Even when it is in front already: a profile made here for the first
        // file ever opened is index 0, in front behind the New connection
        // form, which would cover the tabs this opens.
        self.activate(index, cx);
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
        // Otherwise its connect is on its way, and lands in `run_waiting_files`.
        if self
            .profile()
            .is_some_and(|profile| profile.connection().is_some())
        {
            self.run_waiting_files(&profile_id, cx);
        }
    }

    /// Run what [`Workspace::open_data_files`] left waiting for `profile`,
    /// once its connect has landed: if it connected and is still in front.
    /// Otherwise it is dropped, its statement still in its tab, a Run away --
    /// a later connect must not run it over whatever the tab holds by then.
    pub(crate) fn run_waiting_files(&mut self, profile: &str, cx: &mut Context<Self>) {
        let waiting = waiting_for(&mut self.runs_on_connect, profile);
        if !self
            .profile()
            .is_some_and(|front| front.id == profile && front.connection().is_some())
        {
            return;
        }
        for (tab, sql) in waiting {
            self.execute_sql(sql, Tab::Query(tab), cx);
        }
    }
}

/// `profile`'s waiting runs, taken out of `waiting`: tab and statement.
fn waiting_for(waiting: &mut Vec<(String, u64, String)>, profile: &str) -> Vec<(u64, String)> {
    let (taken, kept) = std::mem::take(waiting)
        .into_iter()
        .partition::<Vec<_>, _>(|(id, ..)| id == profile);
    *waiting = kept;
    taken.into_iter().map(|(_, tab, sql)| (tab, sql)).collect()
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
    let path = path.to_string_lossy();
    let path = match function {
        "read_xlsx" => path.into_owned(),
        _ => unglobbed(&path),
    };
    let path = Engine::DuckDb.quote_literal(&path);
    Some(format!("SELECT * FROM {function}({path});"))
}

/// `path` as a pattern DuckDB's file readers match to itself alone. They read
/// every path as a glob, so `br[a].csv` would read `bra.csv` and `what?.csv`
/// every file it could stand for; a character class holding one character
/// matches only that character, and is the only escape the glob has.
fn unglobbed(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        match character {
            '*' | '?' | '[' => {
                escaped.push('[');
                escaped.push(character);
                escaped.push(']');
            }
            other => escaped.push(other),
        }
    }
    escaped
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

    #[test]
    fn a_connect_landing_settles_its_own_profiles_waiting_runs_and_no_others() {
        let mut waiting = vec![
            ("files".to_string(), 1, "a".to_string()),
            ("other".to_string(), 2, "b".to_string()),
            ("files".to_string(), 3, "c".to_string()),
        ];
        assert_eq!(
            waiting_for(&mut waiting, "files"),
            [(1, "a".to_string()), (3, "c".to_string())]
        );
        assert_eq!(waiting, [("other".to_string(), 2, "b".to_string())]);
        assert_eq!(waiting_for(&mut waiting, "files"), []);
    }

    #[test]
    fn a_file_whose_name_reads_as_a_pattern_is_read_alone() {
        use crate::db::{CancelToken, Connection, Fetch};
        let dir = std::env::temp_dir().join(format!("dbdelve-files-glob-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut files = vec![
            ("br[a].csv", "bra.csv"),
            ("report [2024].csv", "report 2.csv"),
        ];
        // Windows refuses `?` and `*` in a file's name.
        if cfg!(unix) {
            files.extend([("what?.csv", "what!.csv"), ("star*.csv", "star2.csv")]);
        }
        for (n, (named, near)) in files.iter().enumerate() {
            std::fs::write(dir.join(named), format!("n\n{n}\n")).unwrap();
            std::fs::write(dir.join(near), "n\n-1\n").unwrap();
        }
        let connection =
            Connection::open(ConnectionConfig::duckdb(DUCKDB_IN_MEMORY.to_string(), 0)).unwrap();
        for (n, (named, _)) in files.iter().enumerate() {
            let sql = reading(&dir.join(named)).unwrap();
            let result = connection
                .query(&sql, &CancelToken::default(), Fetch::default())
                .unwrap();
            let read: Vec<Option<&str>> = result.rows.iter().map(|row| row.get(0)).collect();
            assert_eq!(read, [Some(n.to_string().as_str())], "{sql}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
