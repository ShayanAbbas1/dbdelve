//! Opening a data file: a CSV, a Parquet or JSON file, or a spreadsheet,
//! picked from the palette or dropped on the window.
//!
//! There is no connection to think about. The files open in a DuckDB profile
//! held in memory -- one that is found if it exists and made if it does not --
//! each in a query tab of its own whose statement is the one DBDelve wrote to
//! read it, on screen and the user's to change: `SELECT * FROM read_csv(…)`.
//! The file itself is only ever read.
//!
//! A SQL file is the other kind: its text opens in a new query tab on the
//! connection in front, as the user's buffer, and runs only when they run it.

use std::io::Read;
use std::path::{Path, PathBuf};

use gpui::{ExternalPaths, PathPromptOptions};

use super::*;

use crate::connection_form::starting_mode;
use crate::db::DUCKDB_IN_MEMORY;

impl Workspace {
    /// The native file picker, then [`Workspace::open_files`].
    pub(crate) fn pick_data_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_files(Self::open_files, window, cx);
    }

    /// The native file picker, then [`Workspace::open_sql_files`]: whatever
    /// is picked opens as SQL, whatever its extension.
    pub(crate) fn pick_sql_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_files(Self::open_sql_files, window, cx);
    }

    fn pick_files(
        &mut self,
        open: fn(&mut Self, Vec<PathBuf>, &mut Window, &mut Context<Self>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let picked = picked.await;
            _ = workspace.update_in(cx, |workspace, window, cx| match picked {
                Ok(Ok(Some(paths))) => open(workspace, paths, window, cx),
                Ok(Err(error)) => workspace.note(error.to_string(), cx),
                Ok(Ok(None)) | Err(_) => {}
            });
        })
        .detach();
    }

    pub(crate) fn drop_files(
        &mut self,
        dropped: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_files(dropped.paths().to_vec(), window, cx);
    }

    /// SQL files into the connection in front first, since opening a data
    /// file brings the DuckDB profile to the front.
    fn open_files(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let (sql, data): (Vec<_>, Vec<_>) = paths.into_iter().partition(|path| is_sql_file(path));
        let refused = match sql.is_empty() {
            true => Vec::new(),
            false => self.open_sql_tabs(sql, window, cx),
        };
        if !data.is_empty() {
            self.open_data_files(data, window, cx);
        }
        self.note_refused(refused, cx);
    }

    pub(crate) fn open_sql_files(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let refused = self.open_sql_tabs(paths, window, cx);
        self.note_refused(refused, cx);
    }

    /// Each file's text in a new query tab on the connection in front, and
    /// what was refused, for the caller to say once everything has opened:
    /// every new tab clears the notice, and a data file moves the front.
    fn open_sql_tabs(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        if self.profile().is_none() {
            return vec!["Open a connection to open a SQL file in.".to_string()];
        }
        let mut refused = Vec::new();
        for path in paths {
            match read_sql_file(&path) {
                Ok(text) => {
                    // `new_query` has said why, which is the notice to keep.
                    if self.new_query_holding(text, window, cx).is_none() {
                        return Vec::new();
                    }
                }
                Err(message) => refused.push(message),
            }
        }
        refused
    }

    fn note_refused(&mut self, refused: Vec<String>, cx: &mut Context<Self>) {
        if !refused.is_empty() {
            self.note(refused.join(" "), cx);
        }
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
                "DBDelve opens .sql, .csv, .tsv, .parquet, .json and .xlsx files.".to_string(),
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
            let Some(id) = self.new_query_holding(sql.clone(), window, cx) else {
                return;
            };
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

    /// A new query tab in front holding `text`, by its id, or `None` when none
    /// could be made. Checked by id because a `new_query` that refuses leaves
    /// the user's own tab in front, and `text` would land over their buffer.
    fn new_query_holding(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let id = self.profile()?.session.next_query_id;
        self.new_query(&NewQuery, window, cx);
        let editor = self
            .profile()?
            .session
            .active_query_tab()
            .filter(|tab| tab.id == id)?
            .editor
            .clone();
        editor.update(cx, |editor, cx| editor.set_value(text, window, cx));
        Some(id)
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

fn is_sql_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
}

// ponytail: the editor holds the whole file, so a dump past this is refused
// rather than opened into a buffer that crawls; running a file that size
// without loading it is the upgrade.
const MAX_SQL_FILE_BYTES: u64 = 4 << 20;

/// A SQL file's text, exactly as written but for a UTF-8 byte order mark,
/// which is the file's encoding and not the user's SQL. Text that is not
/// UTF-8 is refused rather than read lossily, which would change statements.
fn read_sql_file(path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    // Bounded while reading, not by the size up front, which a file still
    // being written or a link to a device does not keep to.
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_SQL_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("Could not open {name}: {error}"))?;
    if bytes.len() as u64 > MAX_SQL_FILE_BYTES {
        return Err(format!(
            "{name} is more than the {} MB the editor opens.",
            MAX_SQL_FILE_BYTES >> 20
        ));
    }
    sql_text(bytes).ok_or_else(|| format!("{name} is not UTF-8 text."))
}

fn sql_text(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    Some(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
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
    fn a_sql_file_opens_as_written_without_its_byte_order_mark() {
        assert!(is_sql_file(Path::new("/dumps/schema.SQL")));
        assert!(!is_sql_file(Path::new("/dumps/rows.csv")));
        assert_eq!(
            sql_text(b"\xef\xbb\xbfSELECT 1; -- \xc3\xa9t\xc3\xa9\r\n".to_vec()).as_deref(),
            Some("SELECT 1; -- \u{e9}t\u{e9}\r\n")
        );
        assert_eq!(sql_text(b"SELECT '\xe9';".to_vec()), None);
    }

    #[test]
    fn a_sql_file_past_the_cap_is_refused_and_one_at_it_opens() {
        let dir = std::env::temp_dir().join(format!("dbdelve-sql-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (at, past) = (dir.join("at.sql"), dir.join("past.sql"));
        std::fs::write(&at, vec![b' '; MAX_SQL_FILE_BYTES as usize]).unwrap();
        std::fs::write(&past, vec![b' '; MAX_SQL_FILE_BYTES as usize + 1]).unwrap();
        let (at, past) = (read_sql_file(&at), read_sql_file(&past));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(at.is_ok());
        assert_eq!(
            past,
            Err("past.sql is more than the 4 MB the editor opens.".to_string())
        );
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
