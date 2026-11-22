//! Opening a data file: a CSV, a Parquet or JSON file, or a spreadsheet,
//! picked from the palette or dropped on the window.
//!
//! There is no connection to think about. The files open in a DuckDB profile
//! held in memory -- one that is found if it exists and made if it does not --
//! each in a query tab of its own whose statement is the one DBDelve wrote to
//! read it, on screen and the user's to change: `SELECT * FROM read_csv(…)`.
//! The file itself is only ever read.
//!
//! A SQL file is the other kind: it opens in a query tab bound to it on the
//! connection in front, as the user's buffer, runs only when they run it, and
//! is written only when they save it.

use std::path::{Path, PathBuf};

use gpui::{ExternalPaths, PathPromptOptions};

use super::*;

use crate::actions::{OpenSqlFiles, SaveQueryAs};
use crate::connection_form::starting_mode;
use crate::db::DUCKDB_IN_MEMORY;
use crate::session::{Conflict, FileBinding, PendingOverwrite, Settle};
use crate::sql_file::{self, SqlText, file_name};

impl Workspace {
    /// The native file picker, then [`Workspace::open_data_files`].
    pub(crate) fn pick_data_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pick_files(Self::open_data_files, window, cx);
    }

    /// The native file picker, then [`Workspace::open_sql_files`]. The picker
    /// cannot filter by extension, so anything but a `.sql` file is refused
    /// after it is picked.
    pub(crate) fn pick_sql_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.import_covers() {
            return;
        }
        self.pick_files(
            |workspace, paths, window, cx| workspace.open_sql_files(paths, Vec::new(), window, cx),
            window,
            cx,
        );
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

    pub(crate) fn open_sql_files_action(
        &mut self,
        _: &OpenSqlFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_sql_files(window, cx);
    }

    pub(super) fn open_files(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (sql, data) = paths.into_iter().partition(|path| is_sql_file(path));
        self.open_sql_files(sql, data, window, cx);
    }

    /// `sql` read on the background executor, since a file on a stalled
    /// mount or a link to a pipe would hold the window for as long as it
    /// waits. Then each opens in a file tab, and only after them `data`,
    /// since opening a data file brings the DuckDB profile to the front.
    fn open_sql_files(
        &mut self,
        sql: Vec<PathBuf>,
        data: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let front = self.profile().map(|profile| profile.id.clone());
        let read = spawn_blocking(cx.background_executor(), move || {
            sql.iter()
                .map(|path| (sql_file::canonical(path), read_sql_file(path)))
                .collect()
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let read: Vec<_> = read.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                let mut refused = workspace.open_sql_tabs(front, read, window, cx);
                if !data.is_empty() {
                    refused.extend(workspace.open_data_tabs(data, window, cx));
                }
                workspace.note_refused(refused, cx);
            });
        })
        .detach();
    }

    /// Each file in a tab bound to it on `front`, the connection in front
    /// when they were picked, and what was refused, for the caller to say
    /// once everything has opened: every new tab clears the notice, and a
    /// data file moves the front. A file already open anywhere brings its own
    /// tab up instead, after the rest have opened, since that can move the
    /// front.
    fn open_sql_tabs(
        &mut self,
        front: Option<String>,
        read: Vec<(PathBuf, Result<SqlText, String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        if read.is_empty() {
            return Vec::new();
        }
        // A link and its file, picked together, are one file.
        let mut seen = std::collections::HashSet::new();
        let (open, read): (Vec<_>, Vec<_>) = read
            .into_iter()
            .filter(|(path, _)| seen.insert(path.clone()))
            .partition(|(path, _)| self.tab_on_file(path).is_some());
        let mut refused = Vec::new();
        if !read.is_empty() {
            refused = self.open_new_sql_tabs(front, read, window, cx);
        }
        for (path, _) in open {
            self.bring_up_file(&path, window, cx);
        }
        refused
    }

    fn open_new_sql_tabs(
        &mut self,
        front: Option<String>,
        read: Vec<(PathBuf, Result<SqlText, String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let Some(front) = front else {
            return vec!["Open a connection to open a SQL file in.".to_string()];
        };
        // Only while the user was waiting on a stalled read could another
        // connection have come to the front, and the files are not theirs.
        if self.profile().map(|profile| &profile.id) != Some(&front) {
            return vec![
                "The SQL files were not opened: another connection came to the front while they were read."
                    .to_string(),
            ];
        }
        if self.engine().syntax() != db::Syntax::Sql {
            return vec!["A SQL file opens only on a connection that runs SQL.".to_string()];
        }
        let mut refused = Vec::new();
        for (path, read) in read {
            match read {
                Ok(read) => {
                    // `open_file_tab` has said why, which is the notice to keep.
                    if self.open_file_tab(path, read, window, cx).is_none() {
                        return Vec::new();
                    }
                }
                Err(message) => refused.push(message),
            }
        }
        refused
    }

    /// The connection and tab bound to `path`, wherever it is open: one tab
    /// per file across the whole app, so no two buffers save over each other.
    pub(super) fn tab_on_file(&self, path: &Path) -> Option<(usize, u64)> {
        self.profiles
            .iter()
            .enumerate()
            .find_map(|(index, profile)| Some((index, profile.session.tab_on(path)?)))
    }

    /// Bring the tab bound to `path` to the front, with its connection, as a
    /// click on its chip would. Whether there was one.
    pub(crate) fn bring_up_file(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.tab_on_file(path) {
            Some((index, id)) => {
                self.activate_chip(index, Tab::Query(id), window, cx);
                true
            }
            None => false,
        }
    }

    /// A new tab in front on the connection in front, bound to `path` and
    /// holding `read`, by its id, or `None` when none could be made. A file
    /// open anywhere already brings its own tab up instead.
    pub(crate) fn open_file_tab(
        &mut self,
        path: PathBuf,
        read: SqlText,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let path = sql_file::canonical(&path);
        if let Some((_, id)) = self.tab_on_file(&path) {
            self.bring_up_file(&path, window, cx);
            return Some(id);
        }
        if let Err(message) = self.persist_buffer(cx) {
            self.note(message, cx);
            return None;
        }
        let (engine, row_limit) = (self.engine(), self.settings.row_limit);
        let sorting = Sorting::new(self.settings.client_sort);
        self.sql_directory = path.parent().map(Path::to_path_buf);
        let profile = self.profile_mut()?;
        let id = profile.session.next_query_id;
        profile.session.next_query_id += 1;
        profile.session.notice = None;
        let profile_id = profile.id.clone();
        let file = FileBinding {
            path: path.clone(),
            saved: Some(read.text.clone()),
            bom: read.bom,
            stamp: Some(read.stamp),
            conflict: None,
        };
        let tab = QueryTab::new(
            &profile_id,
            id,
            read.text,
            Some(file),
            engine,
            sorting,
            row_limit,
            window,
            cx,
        );
        let profile = self.profile_mut()?;
        profile.session.queries.push(tab);
        profile.session.place_last(TabKey::File(path));
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), window, cx);
        self.remember_profiles(cx);
        Some(id)
    }

    /// Save the buffer in front to its file, or ask where for an untitled
    /// one.
    pub(crate) fn save_query(
        &mut self,
        _: &SaveQuery,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.import_covers() {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        match &tab.file {
            Some(_) => {
                let id = tab.id;
                self.save_in_place(vec![id], false, window, cx);
            }
            None => self.save_query_as(&SaveQueryAs, window, cx),
        }
    }

    /// Save each of the file tabs `ids` on the connection in front to its
    /// own file, closing it once written when `close`. One whose file
    /// someone else wrote since is left for the Overwrite question.
    pub(crate) fn save_in_place(
        &mut self,
        ids: Vec<u64>,
        close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let profile_id = profile.id.clone();
        let mut asking = Vec::new();
        let mut writing = Vec::new();
        for id in ids {
            let Some(file) = profile
                .session
                .query_tab(id)
                .and_then(|tab| tab.file.as_ref())
            else {
                continue;
            };
            // ponytail: stat on the window's thread, as the write after it
            // is; one call, but a stalled mount holds the window for it.
            // A stat that fails is left for the write to report.
            let now = sql_file::stamp(&file.path).ok().flatten();
            match file.overwrites(now) {
                true => asking.push(id),
                false => writing.push((id, file.path.clone())),
            }
        }
        for (id, path) in writing {
            if self.save_tab(&profile_id, id, path, cx) && close {
                self.close_now(Tab::Query(id), window, cx);
            }
        }
        if asking.is_empty() {
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.pending_overwrite = Some(PendingOverwrite {
                tabs: asking,
                close,
            });
        }
        cx.notify();
    }

    /// Overwrite: write each tab the question was asked about over whatever
    /// its file holds now.
    pub(crate) fn confirm_overwrite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((profile_id, pending)) = self.profile_mut().and_then(|profile| {
            let pending = profile.session.pending_overwrite.take()?;
            Some((profile.id.clone(), pending))
        }) else {
            return;
        };
        for id in pending.tabs {
            let Some(path) = self
                .profile()
                .and_then(|profile| profile.session.query_tab(id))
                .and_then(|tab| Some(tab.file.as_ref()?.path.clone()))
            else {
                continue;
            };
            if self.save_tab(&profile_id, id, path, cx) && pending.close {
                self.close_now(Tab::Query(id), window, cx);
            }
        }
        self.refocus_front();
        cx.notify();
    }

    pub(crate) fn cancel_overwrite(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_overwrite.take())
            .is_some();
        if cancelled {
            self.refocus_front();
            cx.notify();
        }
        cancelled
    }

    /// Look at the front tab's file and settle what changed on disk since
    /// the tab last read or wrote it: on the window coming forward and on
    /// the tab coming to the front.
    ///
    /// ponytail: a look on focus misses an edit made in another editor
    /// while DBDelve keeps focus, so a buffer can be stale until the next
    /// switch; a file watcher (`notify`) is the upgrade if anyone reports
    /// it. The stat, and a reload's read, run on the window's thread.
    pub(crate) fn check_front_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, settle)) = self.profile().and_then(|profile| {
            let tab = profile.session.active_query_tab()?;
            let file = tab.file.as_ref()?;
            let now = sql_file::stamp(&file.path).ok()?;
            Some((tab.id, file.settle(now, tab.dirty)))
        }) else {
            return;
        };
        match settle {
            Settle::Keep => {}
            Settle::Reload => self.take_disk(id, true, window, cx),
            Settle::Ask => {
                if let Some(file) = self.front_file_mut(id) {
                    file.conflict = Some(Conflict::Asking);
                }
                cx.notify();
            }
            Settle::Gone => self.mark_gone(id, cx),
        }
    }

    /// Tab `id` on the connection in front takes its file as it is on disk
    /// now: into the buffer as well when `reload`, or, for Keep Mine, as
    /// only the text the buffer is compared with, the next Save asking.
    pub(crate) fn take_disk(
        &mut self,
        id: u64,
        reload: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.front_file_mut(id).map(|file| file.path.clone()) else {
            return;
        };
        let read = match sql_file::read(&path) {
            Ok(Some(read)) => read,
            Ok(None) => return self.mark_gone(id, cx),
            Err(message) => return self.note(message, cx),
        };
        let Some(tab) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id))
        else {
            return;
        };
        if reload {
            tab.editor.update(cx, |editor, cx| {
                let at = editor.cursor().min(read.text.len());
                editor.set_value(read.text.clone(), window, cx);
                editor.set_selected_range(at..at, cx);
            });
        }
        if let Some(file) = &mut tab.file {
            file.saved = Some(read.text);
            file.bom = read.bom;
            file.stamp = Some(read.stamp);
            file.conflict = (!reload).then_some(Conflict::KeptMine);
        }
        // `set_value` emits no change, so nothing else marks the tab.
        tab.refresh_dirty(cx);
        cx.notify();
    }

    /// Tab `id`'s file is gone: the tab keeps its text, stays bound and is
    /// dirty, and Save writes the file again.
    fn mark_gone(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id))
        else {
            return;
        };
        if let Some(file) = &mut tab.file {
            file.saved = None;
            file.stamp = None;
            file.conflict = None;
        }
        tab.refresh_dirty(cx);
        cx.notify();
    }

    fn front_file_mut(&mut self, id: u64) -> Option<&mut FileBinding> {
        self.profile_mut()?.session.query_tab_mut(id)?.file.as_mut()
    }

    /// Ask where to save the buffer in front, then save it there and bind
    /// its tab to that file. A file tab's old file is left as it was.
    pub(crate) fn save_query_as(
        &mut self,
        _: &SaveQueryAs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.import_covers() {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_query_tab() else {
            return;
        };
        let (profile_id, id) = (profile.id.clone(), tab.id);
        let path = tab.file.as_ref().map(|file| file.path.clone());
        let directory = path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| self.sql_directory.clone())
            .or_else(|| crate::store::home().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let suggested = path
            .as_deref()
            .map_or_else(|| "query.sql".to_string(), file_name);
        let chosen = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn_in(window, async move |workspace, cx| {
            let chosen = chosen.await;
            _ = workspace.update(cx, |workspace, cx| match chosen {
                Ok(Ok(Some(path))) => {
                    let typed_extension = path.extension().is_some();
                    let path = sql_file::canonical(&sql_file::with_sql_extension(path));
                    // The panel asked about replacing the name typed, not this one.
                    if !typed_extension && path.exists() {
                        workspace.note(format!("{} already exists.", file_name(&path)), cx);
                        return;
                    }
                    workspace.save_tab(&profile_id, id, path, cx);
                }
                Ok(Err(error)) => workspace.note(error.to_string(), cx),
                Ok(Ok(None)) | Err(_) => {}
            });
        })
        .detach();
    }

    /// Write tab `id` of `profile_id` to `path` exactly as it holds it, and
    /// bind the tab there; whether it was written. Its scratch file goes once
    /// the file is written: nothing is unsaved any more.
    ///
    /// ponytail: written on the window's thread, so a file on a stalled mount
    /// holds the window until the write gives up; the blocking pool is the
    /// upgrade, with the text written kept as the saved text.
    fn save_tab(
        &mut self,
        profile_id: &str,
        id: u64,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .tab_on_file(&path)
            .is_some_and(|(index, other)| other != id || self.profiles[index].id != profile_id)
        {
            self.note(format!("{} is open in another tab.", file_name(&path)), cx);
            return false;
        }
        let Some(profile) = self
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
        else {
            return false;
        };
        let Some(tab) = profile.session.query_tab(id) else {
            return false;
        };
        let text = tab.editor.read(cx).value().to_string();
        let bom = tab.file.as_ref().is_some_and(|file| file.bom);
        let from = match &tab.file {
            Some(file) => TabKey::File(file.path.clone()),
            None => TabKey::Unsaved(id),
        };
        let stamp = match sql_file::write(&path, &text, bom) {
            Ok(stamp) => stamp,
            Err(message) => {
                self.note(message, cx);
                return false;
            }
        };
        profile.session.rekey(&from, TabKey::File(path.clone()));
        if let Some(tab) = profile.session.query_tab_mut(id) {
            tab.file = Some(FileBinding {
                path: path.clone(),
                saved: Some(text),
                bom,
                stamp: Some(stamp),
                conflict: None,
            });
            tab.scratch_unread = false;
            tab.refresh_dirty(cx);
        }
        self.sql_directory = path.parent().map(Path::to_path_buf);
        let note = match crate::store::delete_scratch(profile_id, id) {
            Ok(()) => format!("Saved {}.", file_name(&path)),
            Err(message) => message,
        };
        self.note(note, cx);
        self.remember_profiles(cx);
        true
    }

    /// Mark tab `id` of `profile_id` dirty or clean as its editor now reads.
    pub(crate) fn buffer_changed(&mut self, profile_id: &str, id: u64, cx: &mut Context<Self>) {
        let changed = self
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
            .and_then(|profile| profile.session.query_tab_mut(id))
            .is_some_and(|tab| tab.refresh_dirty(cx));
        if changed {
            cx.notify();
        }
    }

    fn note_refused(&mut self, refused: Vec<String>, cx: &mut Context<Self>) {
        if !refused.is_empty() {
            self.note(refused.join(" "), cx);
        }
    }

    pub(super) fn open_data_files(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let refused = self.open_data_tabs(paths, window, cx);
        self.note_refused(refused, cx);
    }

    /// Each file in a query tab on the DuckDB profile, and what was refused,
    /// as [`Workspace::open_sql_tabs`] hands it back.
    fn open_data_tabs(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<String> {
        let mut statements = Vec::new();
        let mut refused = Vec::new();
        for path in &paths {
            match reading(path) {
                Some(sql) => statements.push(sql),
                None => refused.push(format!(
                    "{} is not a CSV, TSV, Parquet, JSON or Excel file.",
                    file_name(path)
                )),
            }
        }
        if statements.is_empty() {
            return refused;
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
            return refused;
        };

        for sql in statements {
            let Some(id) = self.new_query_holding(sql.clone(), window, cx) else {
                return Vec::new();
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
        refused
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

/// A SQL file's text, refused when it is not a `.sql` file or is not there.
fn read_sql_file(path: &Path) -> Result<SqlText, String> {
    let name = file_name(path);
    if !is_sql_file(path) {
        return Err(format!("{name} is not a .sql file."));
    }
    sql_file::read(path)?.ok_or_else(|| format!("{name} is not there any more."))
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
    fn only_a_sql_file_opens_and_it_opens_as_written_without_its_byte_order_mark() {
        assert!(is_sql_file(Path::new("/dumps/schema.SQL")));
        assert!(!is_sql_file(Path::new("/dumps/rows.csv")));
        assert_eq!(
            read_sql_file(Path::new("/data/people.csv")),
            Err("people.csv is not a .sql file.".to_string())
        );
        assert_eq!(
            sql_file::decoded(b"\xef\xbb\xbfSELECT 1; -- \xc3\xa9t\xc3\xa9\r\n".to_vec()),
            Some(("SELECT 1; -- \u{e9}t\u{e9}\r\n".to_string(), true))
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
