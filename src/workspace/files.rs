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

use gpui::{ExternalPaths, PathPromptOptions, PromptButton, PromptLevel};

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
            self.remember_held_file(&path, cx);
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
            self.remember_held_file(&path, cx);
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
        profile.session.place_last(TabKey::File(path.clone()));
        self.install_completions(&profile_id, cx);
        self.activate_tab(Tab::Query(id), window, cx);
        self.remember_file(&profile_id, &path, cx);
        self.remember_profiles(cx);
        Some(id)
    }

    /// `path`, resolved through its links, first among `profile_id`'s recent
    /// files. A saved query's file is not one: the Files group lists it as
    /// the query until it is moved.
    pub(crate) fn remember_file(&mut self, profile_id: &str, path: &Path, cx: &mut Context<Self>) {
        let Some(profile) = self
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
        else {
            return;
        };
        if path.parent() == Some(profile.session.query_directory.as_path()) {
            return;
        }
        profile.recent_files_changed();
        let found = profile.missing_files.remove(path);
        if profile.recent_files.first().map(PathBuf::as_path) == Some(path) {
            if found {
                cx.notify();
            }
            return;
        }
        crate::session::remember_recent(&mut profile.recent_files, path.to_path_buf());
        self.profiles_unsaved = true;
        // Rebuilt only once the event in hand is done: a click on the file's
        // row brings it up, and the tree then selects the row it drew under
        // the pointer by its index, which the reorder has given another file.
        let workspace = cx.entity().downgrade();
        let profile_id = profile_id.to_string();
        cx.defer(move |cx| {
            _ = workspace.update(cx, |workspace, cx| {
                workspace.refresh_query_library(&profile_id, cx)
            });
        });
    }

    /// [`Self::remember_file`] on the connection whose tab holds `path`.
    fn remember_held_file(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some((index, _)) = self.tab_on_file(path) {
            let profile_id = self.profiles[index].id.clone();
            self.remember_file(&profile_id, path, cx);
        }
    }

    /// A row of the Files group: its tab brought up when one is open, else
    /// the file read on the blocking pool, as any SQL file is, into a file
    /// tab whatever its extension, since Save As binds whatever name was
    /// typed. One gone since asks to leave the list.
    pub(crate) fn open_listed_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.import_covers() {
            return;
        }
        if self.bring_up_file(&path, window, cx) {
            self.remember_held_file(&path, cx);
            return;
        }
        let front = self.profile().map(|profile| profile.id.clone());
        let read = spawn_blocking(cx.background_executor(), {
            let path = path.clone();
            move || sql_file::read(&path)
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let read = read.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                let read = match read {
                    Ok(Some(read)) => Ok(read),
                    Err(message) => Err(message),
                    Ok(None) => {
                        if let Some(profile) = workspace
                            .profile_mut()
                            .filter(|profile| Some(&profile.id) == front.as_ref())
                        {
                            profile.missing_files.insert(path.clone());
                            profile.recent_files_changed();
                            profile.session.pending_forget = Some(path);
                            cx.notify();
                        }
                        return;
                    }
                };
                let refused = workspace.open_sql_tabs(front, vec![(path, read)], window, cx);
                workspace.note_refused(refused, cx);
            });
        })
        .detach();
    }

    /// Remove from List: `path` leaves the front connection's recent files.
    /// The file itself is never touched.
    pub(crate) fn forget_recent_file(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.recent_files.retain(|held| held != path);
        profile.missing_files.remove(path);
        profile.recent_files_changed();
        let profile_id = profile.id.clone();
        self.refresh_query_library(&profile_id, cx);
        self.remember_profiles(cx);
    }

    /// Remove, from the question a gone file's row raised.
    pub(crate) fn confirm_forget_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_forget.take())
        else {
            return;
        };
        self.forget_recent_file(&path, cx);
        self.refocus_front();
    }

    pub(crate) fn cancel_forget_file(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_forget.take())
            .is_some();
        if cancelled {
            self.refocus_front();
            cx.notify();
        }
        cancelled
    }

    /// Which of every connection's recent files are gone, for the Files
    /// group to grey: at launch and whenever the window comes forward. On
    /// the blocking pool, since one on a stalled mount waits as long as it
    /// does.
    pub(crate) fn look_at_recent_files(&mut self, cx: &mut Context<Self>) {
        let recent: Vec<(String, u64, Vec<PathBuf>)> = self
            .profiles
            .iter()
            .filter(|profile| !profile.recent_files.is_empty())
            .map(|profile| {
                let generation = profile.recent_generation;
                (profile.id.clone(), generation, profile.recent_files.clone())
            })
            .collect();
        if recent.is_empty() {
            return;
        }
        let looked = spawn_blocking(cx.background_executor(), move || {
            recent
                .into_iter()
                .map(|(id, generation, paths)| {
                    let gone = paths
                        .into_iter()
                        .filter(|path| matches!(path.try_exists(), Ok(false)))
                        .collect::<HashSet<_>>();
                    (id, generation, gone)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |workspace, cx| {
            let looked = looked.await;
            _ = workspace.update(cx, |workspace, cx| {
                for (id, generation, gone) in looked {
                    if let Some(profile) = workspace.profiles.iter_mut().find(|p| p.id == id) {
                        profile.take_missing(generation, gone);
                    }
                }
                cx.notify();
            });
        })
        .detach();
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
            let now = sql_file::stamp(&file.path);
            let on_disk = || {
                let read = sql_file::read(&file.path).ok()??;
                Some((read.text, read.bom))
            };
            match file.overwrites(&now, on_disk) {
                true => asking.push(id),
                false => writing.push((id, file.path.clone())),
            }
        }
        self.save_each(&profile_id, writing, close, window, cx);
        if asking.is_empty() {
            return;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.pending_overwrite = Some(PendingOverwrite {
                tabs: asking,
                close,
            });
        }
        self.ask_to_overwrite(window, cx);
        cx.notify();
    }

    /// Overwrite / Cancel, over the files `pending_overwrite` holds.
    fn ask_to_overwrite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self
            .profile()
            .and_then(|profile| profile.session.pending_overwrite.as_ref())
        else {
            return;
        };
        let message = match pending.tabs.as_slice() {
            [id] => match self.file_name_of(*id) {
                Some(name) => format!("{name} changed on disk. Overwrite it?"),
                None => return,
            },
            tabs => format!("{} files changed on disk. Overwrite them?", tabs.len()),
        };
        let answers = [
            PromptButton::ok("Overwrite"),
            PromptButton::cancel("Cancel"),
        ];
        self.ask(
            &message,
            &answers,
            window,
            cx,
            |workspace, answer, window, cx| match answer {
                0 => workspace.confirm_overwrite(window, cx),
                _ => _ = workspace.cancel_overwrite(cx),
            },
        );
    }

    /// Save / Don't Save / Cancel, over closing the file tabs a
    /// `pending_discard` holds for their unsaved changes.
    pub(crate) fn ask_to_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(discard) = self
            .profile()
            .and_then(|profile| profile.session.pending_discard.as_ref())
        else {
            return;
        };
        let message = match discard.tabs().collect::<Vec<_>>().as_slice() {
            [Tab::Query(id)] => match self.file_name_of(*id) {
                Some(name) => format!("Save changes to {name}?"),
                None => return,
            },
            tabs => format!("Save changes to {} files?", tabs.len()),
        };
        let answers = [
            PromptButton::ok("Save"),
            PromptButton::new("Don't Save"),
            PromptButton::cancel("Cancel"),
        ];
        self.ask(
            &message,
            &answers,
            window,
            cx,
            |workspace, answer, window, cx| match answer {
                0 => workspace.save_and_close(window, cx),
                1 => workspace.confirm_discard_close(window, cx),
                _ => _ = workspace.cancel_discard_close(cx),
            },
        );
    }

    /// The platform's own dialog where it has one (macOS, Windows), and
    /// `crate::prompt`'s where it does not; in either, the `PromptButton::Ok`
    /// takes Return and the `PromptButton::Cancel` Escape.
    fn ask(
        &mut self,
        message: &str,
        answers: &[PromptButton],
        window: &mut Window,
        cx: &mut Context<Self>,
        answered: impl FnOnce(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let answer = window.prompt(PromptLevel::Warning, message, None, answers, cx);
        cx.spawn_in(window, async move |workspace, cx| {
            // Dropped unanswered, the dialog went with its window.
            let Ok(answer) = answer.await else {
                return;
            };
            _ = workspace.update_in(cx, |workspace, window, cx| {
                answered(workspace, answer, window, cx)
            });
        })
        .detach();
    }

    fn file_name_of(&self, id: u64) -> Option<String> {
        let tab = self.profile()?.session.query_tab(id)?;
        Some(file_name(&tab.file.as_ref()?.path))
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
        let tabs = pending
            .tabs
            .into_iter()
            .filter_map(|id| {
                let tab = self.profile()?.session.query_tab(id)?;
                Some((id, tab.file.as_ref()?.path.clone()))
            })
            .collect();
        self.save_each(&profile_id, tabs, pending.close, window, cx);
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
    /// it. The stat, and a reload's read, run on the window's thread. It
    /// trusts the stamp alone, so a same-length edit inside one tick of a
    /// coarse clock, or by a tool that keeps timestamps, is not seen here;
    /// Save compares the text before writing, so nothing is overwritten
    /// unasked, and comparing on every look is the upgrade.
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
        self.profiles_unsaved = true;
        cx.notify();
    }

    /// Reload or Keep Mine, from the tab's bar. The bar goes with the
    /// answer, and a button pressed from the keyboard goes with it, so the
    /// keyboard goes back to the buffer rather than to nothing.
    pub(crate) fn answer_file_bar(
        &mut self,
        id: u64,
        reload: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.take_disk(id, reload, window, cx);
        self.refocus_front();
        cx.notify();
    }

    /// Tab `id`'s file is gone: the tab keeps its text, stays bound and is
    /// dirty, and Save writes the file again.
    pub(super) fn mark_gone(&mut self, id: u64, cx: &mut Context<Self>) {
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
        self.profiles_unsaved = true;
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
                    if let Err(message) = workspace.save_tab(&profile_id, id, path, cx) {
                        workspace.note(message, cx);
                    }
                }
                Ok(Err(error)) => workspace.note(error.to_string(), cx),
                Ok(Ok(None)) | Err(_) => {}
            });
        })
        .detach();
    }

    /// Save each of `tabs`, by id and path, closing each one written when
    /// `close`. The first failure is said last, or the notices of the saves
    /// after it would hide why its tab is still open.
    fn save_each(
        &mut self,
        profile_id: &str,
        tabs: Vec<(u64, PathBuf)>,
        close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut failed = None;
        for (id, path) in tabs {
            match self.save_tab(profile_id, id, path, cx) {
                Ok(()) if close => self.close_now(Tab::Query(id), window, cx),
                Ok(()) => {}
                Err(message) => _ = failed.get_or_insert(message),
            }
        }
        if let Some(message) = failed {
            self.note(message, cx);
        }
    }

    /// Write tab `id` of `profile_id` to `path` exactly as it holds it, and
    /// bind the tab there, or why it was not written. Its scratch file goes
    /// once the file is written: nothing is unsaved any more.
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
    ) -> Result<(), String> {
        if self
            .tab_on_file(&path)
            .is_some_and(|(index, other)| other != id || self.profiles[index].id != profile_id)
        {
            return Err(format!("{} is open in another tab.", file_name(&path)));
        }
        let gone = || format!("{} is not open any more.", file_name(&path));
        let Some(profile) = self
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
        else {
            return Err(gone());
        };
        let Some(tab) = profile.session.query_tab(id) else {
            return Err(gone());
        };
        let text = tab.editor.read(cx).value().to_string();
        let bom = tab.file.as_ref().is_some_and(|file| file.bom);
        let scratch_unread = tab.scratch_unread;
        let from = match &tab.file {
            Some(file) => TabKey::File(file.path.clone()),
            None => TabKey::Unsaved(id),
        };
        let stamp = sql_file::write(&path, &text, bom)?;
        // Save As into the saved queries' folder adds one the sidebar lists.
        let new_saved_query = from != TabKey::File(path.clone())
            && path.parent() == Some(profile.session.query_directory.as_path());
        profile.session.rekey(&from, TabKey::File(path.clone()));
        if let Some(tab) = profile.session.query_tab_mut(id) {
            tab.file = Some(FileBinding {
                path: path.clone(),
                saved: Some(text),
                bom,
                stamp: Some(stamp),
                conflict: None,
            });
            tab.refresh_dirty(cx);
        }
        self.sql_directory = path.parent().map(Path::to_path_buf);
        let deleted = match scratch_unread {
            true => Ok(()),
            false => crate::store::delete_scratch(profile_id, id),
        };
        let note = match deleted {
            Ok(()) => format!("Saved {}.", file_name(&path)),
            Err(message) => message,
        };
        if new_saved_query {
            self.reload_queries_of(profile_id, cx);
        }
        self.remember_file(profile_id, &path, cx);
        self.note(note, cx);
        self.remember_profiles(cx);
        Ok(())
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
            // What a quit writes for the tab (`StoredQueryTab::saved_hash`).
            self.profiles_unsaved = true;
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
