//! Writing a result to a file: the rows a grid holds, or, when that is not
//! the whole result and the user asks for it, every row of it fetched again.

use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, BufWriter},
    path::Path,
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use gpui_component::ActiveTheme;

use super::editing::Short;
use super::*;
use crate::db::{Column, Feed, Fetch, QueryResult, Row};
use crate::export::{FileAs, InsertTarget, Writer};
use crate::session::{ExportProgress, Exporting, PendingExport, Refetch};
use crate::sql::Destructive;
use crate::ui::code_lines;

/// The rows a feed holds for the writer before the statement waits for it: a
/// few passes' worth, so a fast server is slowed to the disk's pace rather than
/// held in memory, without the statement stalling on every pass.
const HELD_ROWS: usize = 4_096;

/// How many of the rows a statement hands back whole are written between
/// two looks at Cancel.
const FINISH_CHUNK: usize = 1_000;

/// How an export that wrote nothing ended.
#[derive(Debug, PartialEq)]
enum Ended {
    Cancelled,
    Failed(String),
}

impl Workspace {
    /// Write the result set in front of the user to a file they pick.
    ///
    /// The rows on screen, when they are the whole result. When they are not
    /// -- the row limit stopped the statement, a snapshot kept part of it, or
    /// a relation tab shows one page -- a dialog asks first, and fetching the
    /// rest happens only on that explicit ask, in a dialog that says it runs
    /// the statement again: an export that quietly re-fetched would make the
    /// number on the row-limit chip a lie, and run SQL nobody asked for.
    /// Pending edits are not written either way: this is the result set the
    /// server returned, and applying them is a separate, visible act.
    pub(crate) fn export_results(&mut self, format: FileAs, cx: &mut Context<Self>) {
        if !self.has_results(cx) {
            return;
        }
        let Some(short) = self.short(cx).or_else(|| self.relation_page(cx)) else {
            self.export_shown(format, cx);
            return;
        };
        let Some(showing) = short.showing() else {
            self.note(short.refusal("exporting"), cx);
            return;
        };
        let every_row = self.refetch();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        profile.session.pending_export = Some(PendingExport {
            format,
            tab: profile.session.active,
            showing,
            situation: short.situation(),
            every_row,
        });
        cx.notify();
    }

    /// Every row of a relation the sidebar names, as `INSERT`s, whether or not
    /// it is open. There is no page to ask about, so no dialog: the menu item
    /// said what it would do, and the save panel can still back out of it.
    pub(crate) fn export_relation(
        &mut self,
        schema: String,
        relation: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let engine = profile.config.engine();
        let refetch = relation_refetch(engine, &schema, &relation, "", &[], &[])
            .and_then(|refetch| self.cleared(refetch));
        let refetch = match refetch {
            Ok(refetch) => refetch,
            Err(reason) => {
                self.note(format!("All rows were not exported: {reason}"), cx);
                return;
            }
        };
        let target = InsertTarget {
            relation: Some((schema, relation.clone())),
            ..InsertTarget::default()
        };
        self.export_every_row(FileAs::Inserts, refetch, relation, target, cx);
    }

    /// What the rows in front of the user write back to: on a relation tab its
    /// table, even where it could not be edited, a view or a keyless table,
    /// since its `SELECT *` names real columns regardless, and a column read
    /// as text for the driver has no source to go by.
    fn insert_target(&self, cx: &App) -> InsertTarget {
        let Some(profile) = self.profile() else {
            return InsertTarget::default();
        };
        if let Some(tab) = profile.session.active_object()
            && matches!(tab.body, ObjectBody::Relation { .. })
        {
            return InsertTarget {
                relation: Some((tab.schema.clone(), tab.name.clone())),
                ..InsertTarget::default()
            };
        }
        profile
            .session
            .active_results()
            .and_then(|results| {
                let result = results.read(cx).delegate().result();
                result.edit.as_ref().map(InsertTarget::from)
            })
            .unwrap_or_default()
    }

    /// A relation tab's page, when it may not be the whole relation: a later
    /// page, or a first one its row limit filled.
    fn relation_page(&self, cx: &App) -> Option<Short> {
        let tab = self.profile()?.session.active_object()?;
        let ObjectBody::Relation {
            query: QueryState::Complete { rows, .. },
            limit,
            offset,
            results,
            ..
        } = &tab.body
        else {
            return None;
        };
        let whole = *offset == 0 && limit.is_none_or(|limit| *rows < limit);
        (!whole).then(|| Short::Page {
            showing: results.read(cx).delegate().result().rows.len(),
            offset: *offset,
        })
    }

    /// The statement that fetches every row of the grid in front again, or
    /// why there is none to run.
    fn refetch(&self) -> Result<Refetch, &'static str> {
        let profile = self.profile().ok_or("the connection is gone.")?;
        let engine = profile.config.engine();
        let refetch = match profile.session.active {
            Tab::Query(id) => {
                let tab = profile.session.query_tab(id).ok_or("the tab is gone.")?;
                // Which statement Refresh would run: on a queue, the one
                // behind the result the switcher shows.
                let queued = tab.queue.as_ref().and_then(|queue| {
                    let finished = queue.done.get(queue.showing)?;
                    (finished.grid != tab.results).then_some((queue, finished))
                });
                let sql = match queued {
                    Some((queue, finished))
                        if queue.lane(finished.start) != Some(queue.showing) =>
                    {
                        return Err("these rows are one of several sets a batch returned.");
                    }
                    Some((_, finished)) => finished.sql.clone(),
                    None => tab
                        .last_query
                        .clone()
                        .ok_or("the statement behind these rows is not known.")?,
                };
                Refetch {
                    sql,
                    generated: false,
                }
            }
            Tab::Object(_) => {
                let tab = profile.session.active_object().ok_or("the tab is gone.")?;
                let ObjectBody::Relation {
                    sort,
                    filter,
                    structure,
                    ..
                } = &tab.body
                else {
                    return Err("this tab has no rows of a relation.");
                };
                let key = match structure {
                    StructureState::Loaded(structure) => structure.row_key(),
                    _ => Vec::new(),
                };
                relation_refetch(engine, &tab.schema, &tab.name, filter, sort, &key)?
            }
        };
        self.cleared(refetch)
    }

    /// `refetch`, or why this connection may not run it now.
    fn cleared(&self, refetch: Refetch) -> Result<Refetch, &'static str> {
        let profile = self.profile().ok_or("the connection is gone.")?;
        if profile.session.exporting.is_some() {
            return Err("an export of every row is already running on this connection.");
        }
        match refetch_refusal(
            profile.config.engine(),
            &refetch,
            profile.mode,
            &profile.confirmed,
            profile.in_turn().is_some(),
        ) {
            Some(reason) => Err(reason),
            None => Ok(refetch),
        }
    }

    /// The rows the grid in front holds, written to a file the user picks.
    fn export_shown(&mut self, format: FileAs, cx: &mut Context<Self>) {
        let stem = self.export_stem();
        let target = self.insert_target(cx);
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(results) = profile.session.active_results() else {
            return;
        };
        // ponytail: the whole result set is copied here, on the frame thread,
        // before the panel even opens -- a wasted copy if the user cancels. It
        // is taken now rather than after the await because the panel is
        // modeless: what the user was looking at when they asked is the only
        // unambiguous answer to what they asked to export. `ResultGrid` holding
        // its `QueryResult` behind an `Arc` is the upgrade path if a large
        // export is ever seen to stutter.
        let result = results.read(cx).delegate().result().clone();
        if result.columns.is_empty() {
            return;
        }
        let id = profile.id.clone();
        let generation = profile.generation;
        let engine = profile.config.engine();
        let connection = profile.connection();
        let rows = result.rows.len();
        let chosen = choose_path(&stem, format, cx);

        cx.spawn(async move |workspace, cx| {
            let Some(path) = chosen.await else {
                return;
            };
            // Rendering a hundred thousand rows is not work to do on the frame
            // thread, and the write even less so, and a table's columns may
            // have to be asked for over the network first.
            let written = cx
                .background_executor()
                .spawn(on_own_thread(move || {
                    // The extension decides the format, not the row that started
                    // this: someone who typed `.json` over the suggested `.csv`
                    // asked for JSON. Which is why the notice says which one it
                    // wrote -- that rename is the one thing they could have got
                    // wrong, and the file name alone does not read it back.
                    let format = FileAs::for_path(&path, engine);
                    let target = match &connection {
                        Some(connection) => knowing_the_table(connection, &target, format, engine),
                        None => target,
                    };
                    let written = write_shown(&result, format, &target, engine, &path);
                    (path, format, written)
                }))
                .await;
            _ = workspace.update(cx, |workspace, cx| {
                // The panel is modeless, so the profile can have moved under
                // this task while it was open. `note` writes to whichever
                // profile is active now, which would announce an export in a
                // session that never ran the query behind it.
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                match written {
                    (path, format, Ok(())) => {
                        let message = exported(rows, format, &path);
                        profile.session.notice = Some(message.clone());
                        workspace.clear_notice_later(id, generation, message, cx);
                    }
                    (path, _, Err(ended)) => profile.session.notice = Some(ended.notice(&path)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// What the tab calls itself, so the file lands named after the thing the
    /// user was looking at rather than after the statement that built it.
    fn export_stem(&self) -> String {
        let Some(profile) = self.profile() else {
            return "results".to_string();
        };
        match profile.session.active_object() {
            Some(tab) => tab.name.clone(),
            None => profile
                .session
                .open_query()
                .map(str::to_string)
                .unwrap_or_else(|| "results".to_string()),
        }
    }

    /// The dialog's answer: every row, or the rows shown. Either way the
    /// save panel asks where next.
    pub(crate) fn answer_pending_export(&mut self, every_row: bool, cx: &mut Context<Self>) {
        // Unconditional, as `cancel_pending_run`'s is: a dialog answered from
        // the keyboard stays up holding it until something redraws.
        cx.notify();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(pending) = profile.session.pending_export.take() else {
            return;
        };
        if profile.session.active != pending.tab {
            return;
        }
        match (every_row, pending.every_row) {
            (true, Ok(refetch)) => {
                let (stem, target) = (self.export_stem(), self.insert_target(cx));
                self.export_every_row(pending.format, refetch, stem, target, cx);
            }
            _ => self.export_shown(pending.format, cx),
        }
    }

    pub(crate) fn cancel_pending_export(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .is_some_and(|profile| profile.session.pending_export.take().is_some());
        if cancelled {
            cx.notify();
        }
        cancelled
    }

    fn export_every_row(
        &mut self,
        format: FileAs,
        refetch: Refetch,
        stem: String,
        target: InsertTarget,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile() else {
            return;
        };
        let (id, generation) = (profile.id.clone(), profile.generation);
        let chosen = choose_path(&stem, format, cx);
        cx.spawn(async move |workspace, cx| {
            let Some(path) = chosen.await else {
                return;
            };
            _ = workspace.update(cx, |workspace, cx| {
                workspace.start_export(&id, generation, refetch, target, path, cx);
            });
        })
        .detach();
    }

    /// Run `refetch` and stream its rows into `path`, once the save panel has
    /// answered. Asked again here, since the panel is modeless: a `BEGIN` or a
    /// mode change while it was open would change the answer.
    fn start_export(
        &mut self,
        id: &str,
        generation: u64,
        refetch: Refetch,
        target: InsertTarget,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.issued_to(id, generation) else {
            return;
        };
        let engine = profile.config.engine();
        let refusal = match &profile.session.exporting {
            Some(_) => Some("an export of every row is already running on this connection."),
            None => refetch_refusal(
                engine,
                &refetch,
                profile.mode,
                &profile.confirmed,
                profile.in_turn().is_some(),
            ),
        };
        let connection = profile.connection();
        let refusal = refusal.or(connection
            .is_none()
            .then_some("the connection is not open."));
        if let Some(reason) = refusal {
            profile.session.notice = Some(format!("All rows were not exported: {reason}"));
            cx.notify();
            return;
        }
        let Some(connection) = connection else {
            return;
        };
        let read_only = profile.mode == Mode::ReadOnly;
        let progress = Arc::new(ExportProgress::default());
        let started = Instant::now();
        let file = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        profile.session.exporting = Some(Exporting {
            file,
            started,
            progress: progress.clone(),
        });
        cx.notify();

        // The count in the status bar moves on this tick alone: the rows go
        // to the file, never through anything that would redraw.
        cx.spawn({
            let id = id.to_string();
            async move |workspace, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;
                    let running = workspace.update(cx, |workspace, cx| {
                        let running = workspace.export_running(&id, started);
                        if running {
                            cx.notify();
                        }
                        running
                    });
                    if !matches!(running, Ok(true)) {
                        break;
                    }
                }
            }
        })
        .detach();

        let task = cx.background_executor().spawn(on_own_thread({
            let (path, progress) = (path.clone(), progress.clone());
            move || {
                export_every_row(
                    &connection,
                    &refetch,
                    &target,
                    read_only,
                    engine,
                    &path,
                    &progress,
                )
            }
        }));
        let id = id.to_string();
        cx.spawn(async move |workspace, cx| {
            let ended = task.await;
            _ = workspace.update(cx, |workspace, cx| {
                // By the export's own start, not the profile's generation: it
                // runs on a connection of its own, which a reconnect of the
                // profile's does not retire, and its line in the status bar has
                // to come down whenever it ends.
                if !workspace.export_running(&id, started) {
                    return;
                }
                if let Some(profile) = workspace
                    .profiles
                    .iter_mut()
                    .find(|profile| profile.id == id)
                {
                    profile.session.exporting = None;
                }
                cx.notify();
                // What it ended in, though, is news only to the session that
                // asked for it, not to one a reconnect or a database switch
                // has put in its place.
                let Some(profile) = workspace.issued_to(&id, generation) else {
                    return;
                };
                match ended {
                    Ok(rows) => {
                        let message = exported(rows, FileAs::for_path(&path, engine), &path);
                        profile.session.notice = Some(message.clone());
                        workspace.clear_notice_later(id, generation, message, cx);
                    }
                    Err(ended) => profile.session.notice = Some(ended.notice(&path)),
                }
            });
        })
        .detach();
    }

    fn export_running(&self, id: &str, started: Instant) -> bool {
        self.profiles.iter().any(|profile| {
            profile.id == id
                && profile
                    .session
                    .exporting
                    .as_ref()
                    .is_some_and(|exporting| exporting.started == started)
        })
    }

    /// Stop the export of every row. The writer sees the flag on its next
    /// pass, stops writing and stops the statement; what the export ends in is
    /// said when it does.
    pub(crate) fn cancel_export(&mut self, cx: &mut Context<Self>) {
        if let Some(exporting) = self
            .profile()
            .and_then(|profile| profile.session.exporting.as_ref())
        {
            exporting.progress.cancelled.store(true, Ordering::Relaxed);
            cx.notify();
        }
    }

    pub(crate) fn render_pending_export(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let profile = self.profile()?;
        let pending = profile.session.pending_export.as_ref()?;
        let syntax = profile.config.engine().syntax();
        let code = fonts(cx).editor.clone();
        let shown = format!(
            "Export {} {} Shown",
            group_thousands(pending.showing as u64),
            if pending.showing == 1 { "Row" } else { "Rows" }
        );
        let (note, sql) = match &pending.every_row {
            Ok(refetch) => (
                "Export All Rows runs this statement again, without the row limit, on a \
                 connection of its own, and writes each row as it arrives."
                    .to_string(),
                Some(code_lines(
                    &refetch.sql,
                    syntax,
                    &cx.theme().highlight_theme,
                )),
            ),
            Err(reason) => (format!("All rows cannot be exported: {reason}"), None),
        };
        let all = pending.every_row.is_ok();
        let workspace = cx.entity().downgrade();
        let (cancel, choose_shown, choose_all) = (workspace.clone(), workspace.clone(), workspace);
        let muted = |text: String| {
            div()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .text_color(t.text_muted)
                .child(text)
        };

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog(t)
                        .child(section_label(t, "Export"))
                        .child(muted(pending.situation.clone()))
                        .child(muted(note))
                        // The statement itself, as the run prompt shows it:
                        // a dialog offering to run SQL shows the SQL.
                        .children(sql.map(|sql| {
                            div()
                                .id("pending-export-sql")
                                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .p(px(layout::SPACE_SM))
                                .rounded(px(layout::RADIUS_CONTROL))
                                .bg(t.surface)
                                .font_family(code)
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text)
                                .children(sql.into_iter().map(|line| div().child(line)))
                        }))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "pending-export",
                                        if all { 2 } else { 1 },
                                        button(
                                            "cancel-pending-export",
                                            "Cancel",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = cancel.update(cx, |workspace, cx| {
                                                    workspace.cancel_pending_export(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "pending-export",
                                        if all { 1 } else { 0 },
                                        button(
                                            "export-shown",
                                            shown,
                                            if all { Tone::Quiet } else { Tone::Primary },
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = choose_shown.update(cx, |workspace, cx| {
                                                    workspace.answer_pending_export(false, cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .children(all.then(|| {
                                    self.choice(
                                        "pending-export",
                                        0,
                                        button(
                                            "export-all",
                                            "Export All Rows",
                                            Tone::Primary,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = choose_all.update(cx, |workspace, cx| {
                                                    workspace.answer_pending_export(true, cx);
                                                });
                                            },
                                        ),
                                        t,
                                    )
                                })),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// The save panel, opened on the home directory with `stem` named in it.
fn choose_path(
    stem: &str,
    format: FileAs,
    cx: &mut Context<Workspace>,
) -> impl Future<Output = Option<PathBuf>> + use<> {
    let suggested = format!("{stem}.{}", format.extension());
    let directory = crate::store::home().unwrap_or_else(|_| PathBuf::from("/"));
    let chosen = cx.prompt_for_new_path(&directory, Some(&suggested));
    async move { chosen.await.ok()?.ok()? }
}

/// The statement that fetches every row of a relation, through the gates
/// `requery_relation` puts a page through.
fn relation_refetch(
    engine: Engine,
    schema: &str,
    relation: &str,
    filter: &str,
    sort: &[SortKey],
    key: &[String],
) -> Result<Refetch, &'static str> {
    let sql = relation_sql(engine, schema, relation, filter, sort, None, 0);
    let sql = sql::is_generated_select(engine, &sql)
        .then(|| sql::paged(engine, &sql, key))
        .flatten()
        .ok_or("dbdelve cannot read this tab's filter as one query.")?;
    Ok(Refetch {
        sql,
        generated: true,
    })
}

/// Why fetching every row of `refetch` again for an export is refused, or
/// `None` when it may run: on a connection of its own, unasked by anything but
/// the dialog, so only when it cannot change anything and reads there what it
/// read here. Fails closed: anything unsure is a refusal with its reason.
///
/// A relation tab's statement is dbdelve's own and has passed its gates
/// already; a query tab's is the user's, run verbatim, so it has to be a lone
/// (`sql::stoppable`) plain read (`sql::plain_read`).
pub(crate) fn refetch_refusal(
    engine: Engine,
    refetch: &Refetch,
    mode: Mode,
    confirmed: &[Destructive],
    in_turn: bool,
) -> Option<&'static str> {
    let sql = refetch.sql.as_str();
    let verdict = sql::classify(engine, sql);
    if !refetch.generated {
        if verdict.destructive.contains(&Destructive::Unreadable) {
            return Some("dbdelve can't parse the statement, so it can't tell that it only reads.");
        }
        if verdict.mode != Mode::ReadOnly
            || !verdict.destructive.is_empty()
            || !sql::rerunnable(engine, sql)
        {
            return Some("the statement writes, and running it again would write again.");
        }
        if !sql::plain_read(engine, sql) {
            return Some("the statement reads or changes what only its own session holds.");
        }
        // One statement, as both parsers count: a file holds one result set.
        if !sql::stoppable(engine, sql) {
            return Some("more than one statement ran together to produce these rows.");
        }
    }
    if sql::gate(&verdict, mode, confirmed).is_some() {
        return Some("this connection's mode would stop the statement.");
    }
    if in_turn {
        return Some(
            "this connection has a transaction open or changed session settings, which a \
             connection of its own would not see.",
        );
    }
    None
}

fn exported(rows: usize, format: FileAs, path: &Path) -> String {
    format!(
        "Exported {} {} as {} to {}.",
        group_thousands(rows as u64),
        if rows == 1 { "row" } else { "rows" },
        format.extension().to_uppercase(),
        path.display()
    )
}

impl Ended {
    fn notice(&self, path: &Path) -> String {
        match self {
            Ended::Cancelled => format!(
                "The export was cancelled. Nothing was written to {}.",
                path.display()
            ),
            Ended::Failed(error) => error.clone(),
        }
    }
}

fn could_not_write(path: &Path) -> impl Fn(io::Error) -> Ended + '_ {
    move |error| Ended::Failed(format!("Could not write {}: {error}", path.display()))
}

/// Write a file through a hidden sibling, renamed onto `path` only once
/// `write` has written it whole. A failed or cancelled export never leaves a
/// truncated file under the name the user chose, nor clobbers a file already
/// there until its replacement is complete.
fn write_replacing<T>(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<T, Ended>,
) -> Result<T, Ended> {
    let failed = could_not_write(path);
    let (partial, file) = staging(path).map_err(&failed)?;
    let mut out = BufWriter::new(file);
    let written = write(&mut out).and_then(|value| {
        // Closed before the rename, which Windows refuses over an open file.
        out.into_inner()
            .map_err(|error| failed(error.into_error()))?;
        std::fs::rename(&partial, path).map_err(&failed)?;
        Ok(value)
    });
    if written.is_err() {
        _ = std::fs::remove_file(&partial);
    }
    written
}

/// A new file beside `path` that nothing else writes: named apart per export,
/// and created only if no file has that name, so two exports to one path
/// never share an inode, and a `.partial` of anyone else's is never truncated.
fn staging(path: &Path) -> io::Result<(PathBuf, File)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    loop {
        let mut name = OsString::from(".");
        name.push(path.file_name().unwrap_or_default());
        name.push(format!(
            ".{}-{}.partial",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let candidate = path.with_file_name(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn write_shown(
    result: &QueryResult,
    format: FileAs,
    target: &InsertTarget,
    engine: Engine,
    path: &Path,
) -> Result<(), Ended> {
    write_replacing(path, |out| {
        let failed = could_not_write(path);
        let mut writer = format
            .writer(out, engine, target, &result.columns)
            .map_err(&failed)?;
        writer
            .rows(&result.rows, &result.cell_types)
            .map_err(&failed)?;
        writer.finish().map_err(failed).map(drop)
    })
}

/// `target`, for a file of `INSERT`s into a real table, told what the server
/// fills of it. On a session of its own, as the user's may be busy. A read that
/// fails leaves the target as it was, whose file names every column: the
/// server says so when it is run, and that is no reason to lose the export.
fn knowing_the_table(
    connection: &Connection,
    target: &InsertTarget,
    format: FileAs,
    engine: Engine,
) -> InsertTarget {
    let structure = target
        .relation
        .as_ref()
        .filter(|_| format == FileAs::Inserts)
        .and_then(|(schema, table)| connection.apart().ok()?.structure(schema, table).ok());
    match structure {
        Some(structure) => target.clone().knowing(engine, &structure.columns),
        None => target.clone(),
    }
}

/// Run `refetch` and write every row it returns to `path`, as they arrive.
fn export_every_row(
    connection: &Connection,
    refetch: &Refetch,
    target: &InsertTarget,
    read_only: bool,
    engine: Engine,
    path: &Path,
    progress: &ExportProgress,
) -> Result<usize, Ended> {
    let format = FileAs::for_path(path, engine);
    let target = &knowing_the_table(connection, target, format, engine);
    write_replacing(path, |out| {
        let sink = Sink {
            format,
            engine,
            target,
            path,
            out: Some(out),
            writer: None,
            progress,
        };
        let written = stream(connection, refetch, read_only, sink, progress)?;
        // A Cancel clicked while the last rows were written still means
        // nothing lands under the user's name.
        match progress.cancelled.load(Ordering::Relaxed) {
            true => Err(Ended::Cancelled),
            false => Ok(written),
        }
    })
}

/// One thread runs the statement, and this one takes what it has fed and
/// writes it, so what is held at any moment is what arrived since the last
/// pass rather than the result.
fn stream(
    connection: &Connection,
    refetch: &Refetch,
    read_only: bool,
    mut sink: Sink,
    progress: &ExportProgress,
) -> Result<usize, Ended> {
    let feed = Feed::bounded(HELD_ROWS);
    // A user's statement goes out on a connection `query_alongside` opens and
    // stops under the run's token; a generated one on a session of its own,
    // stopped as that session, once it is open.
    let (cancel, stops) = match refetch.generated {
        true => (CancelToken::default(), OnceLock::new()),
        false => (CancelToken::alongside(), OnceLock::from(connection.clone())),
    };
    std::thread::scope(|scope| {
        let query = scope.spawn(|| {
            if !refetch.generated {
                return connection.query_alongside(
                    &refetch.sql,
                    &cancel,
                    Fetch {
                        limit: None,
                        feed: Some(&feed),
                        reads_only: true,
                        held: None,
                    },
                    read_only,
                );
            }
            let own = connection.apart()?;
            if read_only {
                own.set_read_only(true)?;
            }
            let own = stops.get_or_init(|| own);
            if progress.cancelled.load(Ordering::Relaxed) {
                return Err(DbError {
                    message: "Cancelled before it was sent.".into(),
                    position: None,
                });
            }
            own.generated(&refetch.sql, &cancel, Some(&feed), None)
        });

        let mut failure = None;
        let mut stopped_at: Option<Instant> = None;
        let mut stopper: Option<std::thread::ScopedJoinHandle<()>> = None;
        // Taken from until the statement is over, whatever happens, as the
        // bounded feed needs: a push it holds waits for this loop alone.
        while !query.is_finished() {
            let fed = feed.take();
            let arrived = !fed.rows.is_empty();
            let stopping = failure.is_some() || progress.cancelled.load(Ordering::Relaxed);
            // Taken and dropped while stopping, so what arrives before the
            // stop lands is not held either.
            if !stopping && let Err(ended) = sink.fed(fed) {
                failure = Some(ended);
            }
            // Sent again each second it runs on: a cancel can land before
            // the statement has started, and stop nothing. On a thread of its
            // own, since a cancel can wait for the statement to end -- a
            // ClickHouse `KILL … SYNC` does -- and the statement can be
            // waiting on this loop to take.
            if (failure.is_some() || progress.cancelled.load(Ordering::Relaxed))
                && stopped_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
                && stopper.as_ref().is_none_or(|stopper| stopper.is_finished())
            {
                if let Some(target) = stops.get() {
                    let cancel = &cancel;
                    stopper = Some(scope.spawn(move || _ = target.cancel(cancel)));
                }
                stopped_at = Some(Instant::now());
            }
            if !arrived {
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        let result = query
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        if failure.is_none() && !progress.cancelled.load(Ordering::Relaxed) {
            failure = sink.fed(feed.take()).err();
        }
        if let Some(ended) = failure {
            return Err(ended);
        }
        if progress.cancelled.load(Ordering::Relaxed) {
            return Err(Ended::Cancelled);
        }
        let result = result.map_err(|error| Ended::Failed(error.message))?;
        if !result.rest.is_empty() {
            return Err(several_sets());
        }
        if result.stopped.is_some() {
            return Err(Ended::Failed(
                "The statement was stopped before it finished, so nothing was written.".into(),
            ));
        }
        // ponytail: MongoDB and Redis push nothing to a feed, so their rows
        // come back here, the whole result held in memory before the first is
        // written. Feeding their cursors as the SQL drivers do is the upgrade
        // path.
        sink.finish(result)
    })
}

fn several_sets() -> Ended {
    Ended::Failed(
        "The statement returned more than one result set, and a file holds one, so nothing was \
         written."
            .into(),
    )
}

/// Where an export's rows go: the file, behind a writer made once the first
/// rows say what the columns are.
struct Sink<'a> {
    format: FileAs,
    engine: Engine,
    target: &'a InsertTarget,
    path: &'a Path,
    out: Option<&'a mut BufWriter<File>>,
    writer: Option<Writer<&'a mut BufWriter<File>>>,
    progress: &'a ExportProgress,
}

impl Sink<'_> {
    fn fed(&mut self, fed: db::Fed) -> Result<(), Ended> {
        match fed.set {
            0 => Ok(()),
            1 if fed.rows.is_empty() => Ok(()),
            1 => self.write(&fed.columns, fed.rows, &[]),
            _ => Err(several_sets()),
        }
    }

    fn write(
        &mut self,
        columns: &[Column],
        rows: Vec<Row>,
        cell_types: &[Vec<&str>],
    ) -> Result<(), Ended> {
        let rows = rendered(self.engine, columns, rows);
        let failed = could_not_write(self.path);
        if self.writer.is_none() {
            let out = self
                .out
                .take()
                .expect("a sink without a writer has its file");
            let writer = self.format.writer(out, self.engine, self.target, columns);
            self.writer = Some(writer.map_err(&failed)?);
        }
        let writer = self.writer.as_mut().expect("made above");
        writer.rows(&rows, cell_types).map_err(failed)?;
        self.progress.rows.fetch_add(rows.len(), Ordering::Relaxed);
        Ok(())
    }

    /// The rows the statement handed back rather than fed, then the end of
    /// the file. The number written, all told.
    ///
    /// Written a chunk at a time, Cancel asked between chunks: these can be
    /// the whole result, and a Cancel that waited for all of it to be written
    /// would come too late to stop anything.
    fn finish(mut self, result: QueryResult) -> Result<usize, Ended> {
        if self.writer.is_none() && result.rows.is_empty() {
            self.write(&result.columns, Vec::new(), &[])?;
        }
        let mut rows = result.rows.into_iter();
        let mut start = 0;
        loop {
            let chunk: Vec<Row> = rows.by_ref().take(FINISH_CHUNK).collect();
            if chunk.is_empty() {
                break;
            }
            if self.progress.cancelled.load(Ordering::Relaxed) {
                return Err(Ended::Cancelled);
            }
            let end = start + chunk.len();
            let cell_types = result.cell_types.get(start..end).unwrap_or(&[]);
            self.write(&result.columns, chunk, cell_types)?;
            start = end;
        }
        let writer = self.writer.take().expect("written above");
        writer.finish().map_err(could_not_write(self.path))?;
        Ok(self.progress.rows.load(Ordering::Relaxed))
    }
}

/// `rows` with the cells a fed result handed over before their column's type
/// was known rendered by it, as `render_aside` does for the grid, so the file
/// shows what the grid would. A chunk taken before the types arrived stays as
/// it came: only the Postgres describe can be late, and only past its patience.
///
/// ponytail: the grid restyles such rows once the types come; a file cannot,
/// so a describe that answers past the feed's patience leaves the batches
/// written before it untyped, their geometry as raw EWKB. Holding the first
/// batch on the export's path until the describe answers or the statement
/// ends is the upgrade path, if a late describe is ever seen on an export.
fn rendered(engine: Engine, columns: &[Column], rows: Vec<Row>) -> Vec<Row> {
    let mut chunk = QueryResult {
        columns: columns.to_vec(),
        rows,
        ..QueryResult::default()
    };
    for (row, column, _, value) in db::render(db::unrendered(engine, &chunk)) {
        chunk.rows[row].replace(&[(column, Some(value.as_str()))]);
    }
    chunk.rows
}

#[cfg(test)]
mod round_trip;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ConnectionConfig;
    use crate::export::Format;

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("dbdelve-export-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// The staging files `write_replacing` left in `directory`.
    fn staged(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_string_lossy();
                name.starts_with('.') && name.ends_with(".partial")
            })
            .collect()
    }

    fn query(sql: &str) -> Refetch {
        Refetch {
            sql: sql.into(),
            generated: false,
        }
    }

    #[test]
    fn every_row_is_fetched_again_only_for_a_lone_plain_read_on_a_fresh_session() {
        let refused = |sql: &str, mode: Mode, in_turn: bool| {
            refetch_refusal(Engine::Postgres, &query(sql), mode, &[], in_turn)
        };
        assert_eq!(refused("SELECT * FROM t", Mode::ReadOnly, false), None);
        assert_eq!(refused("SELECT * FROM t", Mode::Full, false), None);
        assert!(
            refused("SELECT * FROM t", Mode::ReadOnly, true)
                .unwrap()
                .contains("transaction")
        );
        assert!(
            refused("INSERT INTO t VALUES (1) RETURNING *", Mode::Full, false)
                .unwrap()
                .contains("writes")
        );
        assert!(refused("SELECT nextval('s')", Mode::Full, false).is_some());
        assert!(
            refused("selec * from t", Mode::Full, false)
                .unwrap()
                .contains("parse")
        );
        assert!(
            refetch_refusal(
                Engine::SqlServer,
                &query("SELECT 1 AS a; SELECT 2 AS b"),
                Mode::Full,
                &[],
                false
            )
            .unwrap()
            .contains("more than one")
        );
        assert!(
            refused("SELECT 1; SELECT 2", Mode::Full, false)
                .unwrap()
                .contains("more than one")
        );
        let relation = Refetch {
            sql: relation_sql(Engine::Postgres, "public", "t", "", &[], None, 0),
            generated: true,
        };
        assert_eq!(
            refetch_refusal(Engine::Postgres, &relation, Mode::ReadOnly, &[], false),
            None
        );
        assert!(refetch_refusal(Engine::Postgres, &relation, Mode::ReadOnly, &[], true).is_some());
    }

    #[test]
    fn a_failed_write_leaves_neither_the_file_nor_its_partial() {
        let directory = scratch("replace");
        let path = directory.join("out.csv");
        std::fs::write(&path, "kept").unwrap();
        let theirs = directory.join("out.csv.partial");
        std::fs::write(&theirs, "theirs").unwrap();

        let failed = write_replacing(&path, |out| {
            io::Write::write_all(out, b"half").unwrap();
            Err::<(), _>(Ended::Cancelled)
        });
        assert_eq!(failed, Err(Ended::Cancelled));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept");
        assert_eq!(staged(&directory), Vec::<PathBuf>::new());

        // One export still writing while another to the same path completes.
        write_replacing(&path, |first| {
            io::Write::write_all(first, b"first").unwrap();
            write_replacing(&path, |second| {
                io::Write::write_all(second, b"second").map_err(could_not_write(&path))
            })
            .unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs");
        assert_eq!(staged(&directory), Vec::<PathBuf>::new());
        _ = std::fs::remove_dir_all(&directory);
    }

    /// As MongoDB and Redis hand theirs over: all at once, once the statement
    /// is done.
    #[test]
    fn rows_handed_back_whole_are_not_written_past_a_cancel() {
        let directory = scratch("finish");
        let path = directory.join("rows.csv");
        let finish = |progress: &ExportProgress| {
            let result = QueryResult {
                columns: vec![Column {
                    name: "n".into(),
                    data_type: None,
                }],
                rows: (0..2_500)
                    .map(|n| Row::new([Some(n.to_string())]).unwrap())
                    .collect(),
                ..QueryResult::default()
            };
            write_replacing(&path, |out| {
                let sink = Sink {
                    format: FileAs::Rows(Format::Csv),
                    engine: Engine::MongoDb,
                    target: &InsertTarget::default(),
                    path: &path,
                    out: Some(out),
                    writer: None,
                    progress,
                };
                sink.finish(result)
            })
        };

        assert_eq!(finish(&ExportProgress::default()), Ok(2_500));
        let progress = ExportProgress::default();
        progress.cancelled.store(true, Ordering::Relaxed);
        assert_eq!(finish(&progress), Err(Ended::Cancelled));
        assert_eq!(progress.rows.load(Ordering::Relaxed), 0);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2_501);
        assert_eq!(staged(&directory), Vec::<PathBuf>::new());
        _ = std::fs::remove_dir_all(&directory);
    }

    /// Past every row limit there is, through the feed, and cancelled before
    /// it starts.
    #[test]
    fn every_row_streams_to_the_file_and_a_cancel_writes_nothing() {
        let directory = scratch("stream");
        let database = directory.join("rows.db");
        std::fs::write(&database, b"").unwrap();
        let connection = Connection::open(ConnectionConfig::Sqlite {
            path: database.display().to_string(),
            statement_timeout: 0,
        })
        .unwrap();
        let rows = 150_000;
        let refetch = query(&format!(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < {rows}) \
             SELECT x, 'row ' || x AS label FROM n"
        ));

        let path = directory.join("rows.csv");
        let progress = ExportProgress::default();
        let written = export_every_row(
            &connection,
            &refetch,
            &InsertTarget::default(),
            true,
            Engine::Sqlite,
            &path,
            &progress,
        );
        assert_eq!(written, Ok(rows));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), rows + 1);
        assert!(text.starts_with("x,label\n1,row 1\n"));
        assert!(text.ends_with(&format!("{rows},row {rows}\n")));

        let path = directory.join("cancelled.json");
        let progress = ExportProgress::default();
        progress.cancelled.store(true, Ordering::Relaxed);
        let written = export_every_row(
            &connection,
            &refetch,
            &InsertTarget::default(),
            true,
            Engine::Sqlite,
            &path,
            &progress,
        );
        assert_eq!(written, Err(Ended::Cancelled));
        assert!(!path.exists());
        assert_eq!(staged(&directory), Vec::<PathBuf>::new());
        _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn only_a_file_of_inserts_into_a_table_that_can_be_read_is_told_about_it() {
        let directory = scratch("knowing");
        let database = directory.join("rows.db");
        std::fs::write(&database, b"").unwrap();
        let connection = Connection::open(ConnectionConfig::Sqlite {
            path: database.display().to_string(),
            statement_timeout: 0,
        })
        .unwrap();
        let knows_nothing = |target: &InsertTarget, format| {
            let known = knowing_the_table(&connection, target, format, Engine::Sqlite);
            assert_eq!(&known, target);
        };
        let gone = InsertTarget {
            relation: Some(("main".into(), "gone".into())),
            ..InsertTarget::default()
        };
        knows_nothing(&gone, FileAs::Inserts);
        knows_nothing(&gone, FileAs::Rows(Format::Csv));
        knows_nothing(&InsertTarget::default(), FileAs::Inserts);

        _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_relation_exported_as_sql_runs_back_into_a_copy_of_it() {
        let directory = scratch("inserts");
        let database = directory.join("rows.db");
        std::fs::write(&database, b"").unwrap();
        let connection = Connection::open(ConnectionConfig::Sqlite {
            path: database.display().to_string(),
            statement_timeout: 0,
        })
        .unwrap();
        let cancel = CancelToken::default();
        let run = |sql: &str| connection.query(sql, &cancel, Fetch::default()).unwrap();
        run("CREATE TABLE notes (id INTEGER, note TEXT, ratio REAL, data BLOB)");
        run("INSERT INTO notes VALUES (1, 'it''s', 1.5, x'ABCD'), (2, NULL, NULL, NULL)");

        let path = directory.join("notes.sql");
        let refetch = relation_refetch(Engine::Sqlite, "main", "notes", "", &[], &[]).unwrap();
        let target = InsertTarget {
            relation: Some(("main".into(), "notes".into())),
            ..InsertTarget::default()
        };
        let written = export_every_row(
            &connection,
            &refetch,
            &target,
            true,
            Engine::Sqlite,
            &path,
            &ExportProgress::default(),
        );
        assert_eq!(written, Ok(2));

        let before = run("SELECT * FROM notes ORDER BY id").rows;
        run("DELETE FROM notes");
        for statement in std::fs::read_to_string(&path).unwrap().lines() {
            run(statement);
        }
        assert_eq!(run("SELECT * FROM notes ORDER BY id").rows, before);
        _ = std::fs::remove_dir_all(&directory);
    }
}
