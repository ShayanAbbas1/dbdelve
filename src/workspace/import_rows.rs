//! Import Rows: the dialog's life on the workspace, from the file prompt
//! through the background checks of the file to the run that sends it.
//!
//! Everything here finds its dialog by profile id rather than through the
//! profile in front: a check or a chunk lands whenever it lands, and the
//! profile in front by then may be another one.

use std::time::{Duration, Instant};

use gpui::PathPromptOptions;

use super::import_dialog::{import_blocked, new_preview, show_preview, show_preview_row};
use super::*;
use crate::import_rows::{
    Delimiter, Options, Outcome, Plan, Run, Spellings, Step, TargetColumn, prepare, send,
};
use crate::session::{ImportDialog, ImportInputs, ImportPhase};

/// How long the options sit still before the file is read again, so a word
/// typed into a field re-reads once rather than per keystroke.
const RECHECK_DELAY: Duration = Duration::from_millis(150);

impl Workspace {
    /// The palette's and File menu's way in: into the table the object tab in
    /// front shows.
    pub(crate) fn import_rows(
        &mut self,
        _: &ImportRows,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let target = self
            .profile()
            .and_then(|profile| profile.session.active_object())
            .map(|tab| {
                (
                    tab.schema.clone(),
                    tab.name.clone(),
                    tab.takes_imports(engine),
                )
            });
        match target {
            None => self.note("Open a table to import rows into.".into(), cx),
            Some((_, _, false)) => self.note("This view takes no imported rows.".into(), cx),
            Some((schema, table, true)) => self.import_rows_into(schema, table, window, cx),
        }
    }

    /// Offer the file prompt for an import into `schema.table`, opening
    /// nothing else: the dialog reads the table's columns itself. Silent
    /// when a dialog is already open, which is in front of the user.
    pub(crate) fn import_rows_into(
        &mut self,
        schema: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.require(Mode::ReadWrite, cx) {
            return;
        }
        self.clear_notice();
        let Some(profile) = self.profile() else {
            return;
        };
        // A session holds one dialog, and replacing a running one would drop
        // its connection mid-import.
        if let Some(running) = self.running_import_into(&profile.id) {
            self.note(format!("An import into {running} is running."), cx);
        } else if profile.session.import_rows.is_none() {
            self.prompt_import_file(schema, table, window, cx);
        }
    }

    /// Whether the import dialog covers the tab in front, where whatever acts
    /// on that tab's buffer or grid would act out of sight, or open under it.
    pub(crate) fn import_covers(&self) -> bool {
        self.profile()
            .is_some_and(|profile| profile.session.import_rows.is_some())
    }

    /// `` `schema.table` ``, while `profile` has an import running. What would
    /// end the run's session or the dialog it reports to is refused meanwhile:
    /// a reconnect, a database switch, a second import.
    pub(crate) fn running_import_into(&self, profile: &str) -> Option<String> {
        let found = self.profiles.iter().find(|found| found.id == profile)?;
        let dialog = found.session.import_rows.as_ref()?;
        matches!(dialog.phase, ImportPhase::Running { .. })
            .then(|| format!("`{}.{}`", dialog.schema, dialog.table))
    }

    /// Ask for a file to import into `schema.table`. A dismissed picker does
    /// nothing.
    pub(crate) fn prompt_import_file(
        &mut self,
        schema: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile().map(|profile| profile.id.clone()) else {
            return;
        };
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let chosen = chosen.await;
            _ = workspace.update_in(cx, |workspace, window, cx| match chosen {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        workspace.open_import_file(profile, schema, table, path, window, cx);
                    }
                }
                Ok(Err(error)) => workspace.note(error.to_string(), cx),
                Ok(Ok(None)) | Err(_) => {}
            });
        })
        .detach();
    }

    /// Read `path` and open `profile`'s dialog on it, in place of whatever
    /// file the dialog held, then read the table's columns for it.
    fn open_import_file(
        &mut self,
        profile: String,
        schema: String,
        table: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // ponytail: whole file in memory; stream rows if multi-GB imports matter
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move { std::fs::read(&path) }
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let read = read.await;
            _ = workspace.update_in(cx, |workspace, window, cx| {
                let Some(found) = workspace.profiles.iter_mut().find(|p| p.id == profile) else {
                    return;
                };
                let bytes = match read {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        found.session.notice =
                            Some(format!("Could not read {}: {error}", path.display()));
                        cx.notify();
                        return;
                    }
                };
                let engine = found.config.engine();
                let session = &mut found.session;
                if session
                    .import_rows
                    .as_ref()
                    .is_some_and(|dialog| matches!(dialog.phase, ImportPhase::Running { .. }))
                {
                    return;
                }
                let options = Options::detected(&path, &bytes);
                session.editor_needs_focus = true;
                session.import_rows = Some(ImportDialog {
                    schema,
                    table,
                    structure: StructureState::Loading,
                    inputs: import_inputs(&profile, &options, window, cx),
                    options,
                    path,
                    bytes: bytes.into(),
                    mappings: Vec::new(),
                    prepared: None,
                    plan: None,
                    checks: 0,
                    checking: false,
                    phase: ImportPhase::Editing,
                    error: None,
                    committed: 0,
                    preview: new_preview(window, cx),
                    sql: cx.new(|cx| {
                        EditorState::new(window, cx)
                            .language(engine.syntax().highlighter())
                            .soft_wrap(false)
                    }),
                    sql_stale: false,
                    showing_sql: false,
                    preview_start: 0,
                    problem_at: 0,
                    focused_mapping: None,
                    mapping_scroll: gpui::ScrollHandle::new(),
                });
                workspace.load_import_columns(&profile, cx);
            });
        })
        .detach();
    }

    /// Read the columns of the table `profile`'s dialog imports into, with the
    /// same catalog call a relation tab's structure makes, and check the file
    /// against them once they land. Again after a reconnect, which may have
    /// moved the profile to another server or database.
    pub(crate) fn load_import_columns(&mut self, profile: &str, cx: &mut Context<Self>) {
        let Some(found) = self.profiles.iter_mut().find(|found| found.id == profile) else {
            return;
        };
        let connection = found.connection();
        let connecting = matches!(found.state, ProfileState::Connecting);
        let Some(dialog) = found
            .session
            .import_rows
            .as_mut()
            .filter(|dialog| matches!(dialog.phase, ImportPhase::Editing))
        else {
            return;
        };
        let Some(connection) = connection else {
            // The connect landing loads them.
            if !connecting {
                dialog.structure = StructureState::Failed("The connection is not open.".into());
                self.recheck_import(profile, cx);
            }
            return;
        };
        let (schema, table, file) = (
            dialog.schema.clone(),
            dialog.table.clone(),
            dialog.bytes.clone(),
        );
        let structure = cx
            .background_executor()
            .spawn(async move { connection.structure(&schema, &table) });
        let profile = profile.to_string();
        cx.spawn(async move |workspace, cx| {
            let structure = structure.await;
            _ = workspace.update(cx, |workspace, cx| {
                // The file a dialog holds is its identity.
                let Some(dialog) = workspace
                    .import_dialog_mut(&profile)
                    .filter(|dialog| Arc::ptr_eq(&dialog.bytes, &file))
                else {
                    return;
                };
                dialog.structure = match structure {
                    Ok(structure) => StructureState::Loaded(structure),
                    Err(error) => StructureState::Failed(error.message),
                };
                workspace.recheck_import(&profile, cx);
            });
        })
        .detach();
    }

    fn import_dialog_mut(&mut self, profile: &str) -> Option<&mut ImportDialog> {
        self.profiles
            .iter_mut()
            .find(|found| found.id == profile)?
            .session
            .import_rows
            .as_mut()
    }

    /// Read the file again under the dialog's options and mappings. The last
    /// answer stays on screen until this one lands.
    pub(crate) fn recheck_import(&mut self, profile: &str, cx: &mut Context<Self>) {
        let Some(found) = self.profiles.iter_mut().find(|found| found.id == profile) else {
            return;
        };
        let engine = found.config.engine();
        let Some(dialog) = found.session.import_rows.as_mut() else {
            return;
        };
        dialog.checks += 1;
        dialog.checking = true;
        cx.notify();
        let targets = match &dialog.structure {
            StructureState::Loaded(structure) => TargetColumn::from_structure(structure, engine),
            StructureState::Failed(message) => {
                dialog.prepared = Some(Err(message.clone()));
                dialog.plan = None;
                dialog.checking = false;
                return;
            }
            // `load_import_columns` checks again when the columns land.
            StructureState::Loading => return,
        };
        let (check, bytes, options) = (dialog.checks, dialog.bytes.clone(), dialog.options.clone());
        let previous = match &dialog.prepared {
            Some(Ok(prepared)) => Some((prepared.columns.clone(), dialog.mappings.clone())),
            _ => None,
        };
        let (schema, table) = (dialog.schema.clone(), dialog.table.clone());
        let profile = profile.to_string();
        cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(RECHECK_DELAY).await;
            // The file a dialog holds is its identity: a dialog reopened
            // counts its checks from zero again.
            let file = bytes.clone();
            let newest = |dialog: &&mut ImportDialog| {
                dialog.checks == check && Arc::ptr_eq(&dialog.bytes, &file)
            };
            let current = workspace.update(cx, |workspace, _| {
                workspace
                    .import_dialog_mut(&profile)
                    .filter(newest)
                    .is_some()
            });
            if !matches!(current, Ok(true)) {
                return;
            }
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let previous = previous
                        .as_ref()
                        .map(|(columns, mappings)| (columns.as_slice(), mappings.as_slice()));
                    let prepared = prepare(&bytes, &options, &targets, previous, engine)?;
                    let checked = prepared.checked.clone();
                    let plan = Plan::new(engine, schema, table, checked, &options);
                    Ok::<_, String>((prepared, plan))
                })
                .await;
            _ = workspace.update(cx, |workspace, cx| {
                let Some(dialog) = workspace.import_dialog_mut(&profile).filter(newest) else {
                    return;
                };
                match prepared {
                    Ok((prepared, plan)) => {
                        dialog.mappings = prepared.mappings.clone();
                        dialog.plan = Some(Arc::new(plan));
                        dialog.prepared = Some(Ok(Arc::new(prepared)));
                        dialog.problem_at = 0;
                        show_preview(dialog, cx);
                    }
                    Err(message) => {
                        dialog.prepared = Some(Err(message));
                        dialog.plan = None;
                    }
                }
                dialog.checking = false;
                dialog.sql_stale = true;
                cx.notify();
            });
        })
        .detach();
    }

    /// Change `profile`'s dialog and read the file again under the change.
    /// `edit` says whether it changed anything; nothing changes while the
    /// import runs.
    pub(crate) fn edit_import(
        &mut self,
        profile: &str,
        edit: impl FnOnce(&mut ImportDialog) -> bool,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.import_dialog_mut(profile) else {
            return;
        };
        if matches!(dialog.phase, ImportPhase::Editing) && edit(dialog) {
            self.recheck_import(profile, cx);
        }
    }

    pub(crate) fn show_import_sql(&mut self, profile: &str, sql: bool, cx: &mut Context<Self>) {
        if let Some(dialog) = self.import_dialog_mut(profile) {
            dialog.showing_sql = sql;
            cx.notify();
        }
    }

    /// Point at a file column's mapping and scroll it into view.
    pub(crate) fn focus_import_mapping(
        &mut self,
        profile: &str,
        column: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = self.import_dialog_mut(profile) {
            dialog.focused_mapping = Some(column);
            dialog.mapping_scroll.scroll_to_item(column);
            cx.notify();
        }
    }

    /// Show the dialog's problem `index` where it is: its row in the preview,
    /// or the mapping of the file column it belongs to.
    pub(crate) fn jump_to_problem(&mut self, profile: &str, index: usize, cx: &mut Context<Self>) {
        let Some(dialog) = self.import_dialog_mut(profile) else {
            return;
        };
        let Some(Ok(prepared)) = &dialog.prepared else {
            return;
        };
        let Some(problem) = prepared.checked.problems.get(index) else {
            return;
        };
        let (row, column) = (problem.row, problem.column);
        dialog.problem_at = index;
        match (row, column) {
            (Some(row), _) => {
                dialog.showing_sql = false;
                show_preview_row(dialog, row, cx);
                cx.notify();
            }
            (None, Some(column)) => self.focus_import_mapping(profile, column, cx),
            (None, None) => cx.notify(),
        }
    }

    /// Every statement the import would send, onto the clipboard. Written off
    /// the main thread: it is the whole file again, as SQL.
    pub(crate) fn copy_import_sql(&mut self, profile: &str, cx: &mut Context<Self>) {
        let Some(plan) = self
            .import_dialog_mut(profile)
            .and_then(|dialog| dialog.plan.clone())
        else {
            return;
        };
        let script = cx.background_executor().spawn(async move { plan.script() });
        let profile = profile.to_string();
        cx.spawn(async move |workspace, cx| {
            let script = script.await;
            _ = workspace.update(cx, |workspace, cx| match script {
                Ok(sql) => cx.write_to_clipboard(ClipboardItem::new_string(sql)),
                Err(message) => {
                    if let Some(dialog) = workspace.import_dialog_mut(&profile) {
                        dialog.error = Some(message);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Write the newest check's first statement into the SQL view, which a
    /// check landing without a window cannot. Only while the view is shown:
    /// the statement can be a whole chunk, and a check lands per keystroke.
    pub(crate) fn sync_import_sql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self
            .profile_mut()
            .and_then(|profile| profile.session.import_rows.as_mut())
            .filter(|dialog| dialog.sql_stale && dialog.showing_sql)
        else {
            return;
        };
        dialog.sql_stale = false;
        // A plan with no statement to show is a file whose problems the footer
        // already names.
        let sql = dialog
            .plan
            .as_ref()
            .and_then(|plan| plan.preview_sql().ok())
            .unwrap_or_default();
        dialog
            .sql
            .update(cx, |editor, cx| editor.set_value(sql, window, cx));
    }

    /// Run the plan on screen. Does nothing unless the last check found no
    /// problems and is still the newest.
    pub(crate) fn start_import(&mut self, cx: &mut Context<Self>) {
        let ready = self
            .profile()
            .and_then(|profile| profile.session.import_rows.as_ref())
            .is_some_and(|dialog| {
                matches!(dialog.phase, ImportPhase::Editing)
                    && dialog.plan.is_some()
                    && import_blocked(dialog).is_none()
            });
        if !ready || !self.require(Mode::ReadWrite, cx) {
            return;
        }
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let (id, connection) = (profile.id.clone(), profile.connection());
        let Some(dialog) = profile.session.import_rows.as_mut() else {
            return;
        };
        let Some(connection) = connection else {
            dialog.error = Some("The connection is not open.".into());
            cx.notify();
            return;
        };
        let Some(plan) = dialog.plan.clone() else {
            return;
        };
        let started = Instant::now();
        dialog.error = None;
        dialog.phase = ImportPhase::Running {
            run: Run::new(
                plan.chunks.iter().map(|chunk| chunk.len()).collect(),
                plan.begin.is_some(),
            ),
            plan,
            connection: None,
            cancel: CancelToken::default(),
            started,
            cancelling: false,
        };
        cx.notify();

        let apart = cx
            .background_executor()
            .spawn(on_own_thread(move || connection.apart()));
        cx.spawn({
            let id = id.clone();
            async move |workspace, cx| {
                let apart = apart.await;
                _ = workspace.update(cx, |workspace, cx| {
                    let Some(dialog) = workspace.running_import(&id, started) else {
                        return;
                    };
                    match apart {
                        Ok(own) => {
                            if let ImportPhase::Running { connection, .. } = &mut dialog.phase {
                                *connection = Some(own);
                            }
                            workspace.import_step(id, started, cx);
                        }
                        Err(error) => {
                            dialog.phase = ImportPhase::Editing;
                            dialog.error = Some(error.message);
                            cx.notify();
                        }
                    }
                });
            }
        })
        .detach();

        // The same tick as `execute_unchecked`'s: nothing else redraws the
        // elapsed time while a chunk is out.
        cx.spawn(async move |workspace, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let running = workspace.update(cx, |workspace, cx| {
                    let running = workspace.running_import(&id, started).is_some();
                    if running {
                        cx.notify();
                    }
                    running
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// The import `started` began, found by its start rather than by the
    /// profile's generation: it runs on a session of its own, and a reconnect
    /// of the profile's is refused until it ends.
    fn running_import(&mut self, profile: &str, started: Instant) -> Option<&mut ImportDialog> {
        self.import_dialog_mut(profile).filter(|dialog| {
            matches!(dialog.phase, ImportPhase::Running { started: at, .. } if at == started)
        })
    }

    /// Send the run's next submission, or end the run when there is none.
    fn import_step(&mut self, profile: String, started: Instant, cx: &mut Context<Self>) {
        let Some((mode, confirmed)) = self
            .profiles
            .iter()
            .find(|found| found.id == profile)
            .map(|found| (found.mode, found.confirmed.clone()))
        else {
            return;
        };
        let Some(dialog) = self.running_import(&profile, started) else {
            return;
        };
        let ImportPhase::Running {
            plan,
            run,
            connection,
            cancel,
            ..
        } = &dialog.phase
        else {
            return;
        };
        let Some(step) = run.next() else {
            self.finish_import(profile, cx);
            return;
        };
        let Some(connection) = connection.clone() else {
            return;
        };
        let (plan, cancel) = (plan.clone(), cancel.clone());
        let sent = cx.background_executor().spawn(on_own_thread(move || {
            let sql = plan.sql(step).map_err(|message| DbError {
                message,
                position: None,
            })?;
            send(&connection, plan.engine, &sql, mode, &confirmed, &cancel)
        }));
        cx.spawn(async move |workspace, cx| {
            let landed = sent.await;
            _ = workspace.update(cx, |workspace, cx| {
                // Gone with its profile, and its session with it, which
                // rolls back whatever transaction it held open.
                let Some(dialog) = workspace.running_import(&profile, started) else {
                    return;
                };
                if let ImportPhase::Running { run, .. } = &mut dialog.phase {
                    run.landed(step, landed);
                }
                cx.notify();
                workspace.import_step(profile, started, cx);
            });
        })
        .detach();
    }

    pub(crate) fn cancel_import(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = self
            .profile_mut()
            .and_then(|profile| profile.session.import_rows.as_mut())
        {
            stop_import(dialog, cx);
        }
    }

    /// Escape's step: closes the dialog, except while it runs, when only
    /// Cancel stops it.
    pub(crate) fn close_import(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let session = &mut profile.session;
        let Some(dialog) = &session.import_rows else {
            return false;
        };
        if matches!(dialog.phase, ImportPhase::Editing) {
            session.import_rows = None;
            self.refocus_front();
            cx.notify();
        }
        true
    }

    /// Drop `profile`'s dialog, stopping what it is running, and say what a
    /// stopped run left behind. Dropping its session rolls back an open
    /// transaction.
    pub(crate) fn abandon_import(
        &mut self,
        profile: &str,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let found = self.profiles.iter_mut().find(|found| found.id == profile)?;
        let mut dialog = found.session.import_rows.take()?;
        cx.notify();
        let ImportPhase::Running { plan, run, .. } = &dialog.phase else {
            return None;
        };
        let stopped = stopped_note(
            &format!("`{}.{}`", dialog.schema, dialog.table),
            plan.begin.is_some(),
            run.inserted(),
            plan.chunks.iter().map(|chunk| chunk.len()).sum(),
        );
        stop_import(&mut dialog, cx);
        Some(stopped)
    }

    /// Say how the run ended. Only a failure keeps the dialog, on the error.
    fn finish_import(&mut self, profile: String, cx: &mut Context<Self>) {
        let in_front = self.profile().is_some_and(|active| active.id == profile);
        let Some(found) = self.profiles.iter_mut().find(|found| found.id == profile) else {
            return;
        };
        let session = &mut found.session;
        let Some(dialog) = session.import_rows.as_mut() else {
            return;
        };
        let ImportPhase::Running { plan, run, .. } = &dialog.phase else {
            return;
        };
        let Some(outcome) = run.outcome().cloned() else {
            return;
        };
        let summary = outcome.summary(plan);
        let (schema, table) = (dialog.schema.clone(), dialog.table.clone());
        let (changed, closes) = match outcome {
            Outcome::Imported { .. } => (true, true),
            Outcome::Cancelled { committed } => (committed > 0, true),
            Outcome::Failed { committed, .. } => {
                dialog.committed = committed;
                (committed > 0, false)
            }
        };
        if closes {
            session.import_rows = None;
        } else {
            dialog.phase = ImportPhase::Editing;
            dialog.error = Some(summary.clone());
        }
        let mut shown = None;
        if changed {
            let active = session.active;
            for object in &mut session.objects {
                let ObjectBody::Relation { stale, count, .. } = &mut object.body else {
                    continue;
                };
                if object.schema != schema || object.name != table {
                    continue;
                }
                if in_front && active == Tab::Object(object.id) {
                    shown = Some(object.id);
                } else {
                    // Read again when it is next looked at, as after a
                    // reconnect, and counted again as `refresh_relation`
                    // would.
                    *stale = true;
                    if let RowCount::Counted(..) = count {
                        *count = RowCount::Unasked;
                    }
                }
            }
        }
        if let Some(tab) = shown {
            // Before the notice, which a refresh takes down.
            self.refresh_relation(tab, cx);
        }
        if closes {
            if let Some(found) = self.profiles.iter_mut().find(|found| found.id == profile) {
                found.session.notice = Some(summary);
            }
            if in_front {
                self.refocus_front();
            }
        }
        cx.notify();
    }
}

/// The typed options' fields, each writing its option back as it is typed.
fn import_inputs(
    profile: &str,
    options: &Options,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> ImportInputs {
    let inputs = ImportInputs {
        delimiter: cx
            .new(|cx| InputState::new(window, cx).validate(|text, _| text.chars().count() <= 1)),
        skip_lines: cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("0")
                .validate(|text, _| text.chars().all(|c| c.is_ascii_digit()))
        }),
        null_word: cx.new(|cx| InputState::new(window, cx).placeholder("e.g. NULL or \\N")),
        booleans: cx.new(|cx| InputState::new(window, cx).default_value(options.booleans.text())),
    };
    // `false` where the text is not an option yet: an emptied delimiter,
    // spellings that do not parse. The last good one stays.
    writes_option(profile, &inputs.delimiter, cx, |options, text| {
        let Some(c) = text.chars().next() else {
            return false;
        };
        options.delimiter = Delimiter::Custom(c);
        true
    });
    writes_option(profile, &inputs.skip_lines, cx, |options, text| {
        let Ok(lines) = (if text.is_empty() { Ok(0) } else { text.parse() }) else {
            return false;
        };
        options.skip_lines = lines;
        true
    });
    writes_option(profile, &inputs.null_word, cx, |options, text| {
        options.null_word = text.to_string();
        true
    });
    writes_option(profile, &inputs.booleans, cx, |options, text| {
        let Ok(spellings) = Spellings::parse(text) else {
            return false;
        };
        options.booleans = spellings;
        true
    });
    inputs
}

fn writes_option(
    profile: &str,
    input: &Entity<InputState>,
    cx: &mut Context<Workspace>,
    write: fn(&mut Options, &str) -> bool,
) {
    let profile = profile.to_string();
    cx.subscribe(input, move |workspace, input, event: &InputEvent, cx| {
        if matches!(event, InputEvent::Change) {
            let text = input.read(cx).value();
            workspace.edit_import(&profile, |dialog| write(&mut dialog.options, &text), cx);
        }
    })
    .detach();
}

/// What an import stopped from outside its dialog left in `target`, after
/// `landed` of its `rows` were sent. A transaction rolls back with its
/// session, unless its `COMMIT` is already out, which a stop does not reach;
/// commit as it goes keeps the chunks that landed, and the one out when it
/// stopped may land too.
fn stopped_note(target: &str, atomic: bool, landed: usize, rows: usize) -> String {
    let committed = match (atomic, landed) {
        (true, _) if landed == rows => {
            return format!(
                "The import into {target} was stopped as it committed, so its rows may have \
                 been imported."
            );
        }
        (true, _) => {
            return format!("The import into {target} was stopped, and nothing was imported.");
        }
        (false, 0) => "no rows".to_string(),
        (false, 1) => "1 row".to_string(),
        (false, n) => format!("{} rows", group_thousands(n as u64)),
    };
    format!(
        "The import into {target} was stopped with {committed} committed, and perhaps the chunk \
         it was sending."
    )
}

/// Stop asking for more of the run, and stop the statement it has out on the
/// server. A failed stop leaves that statement to finish, after which the run
/// sends nothing more but its `ROLLBACK`.
fn stop_import(dialog: &mut ImportDialog, cx: &mut Context<Workspace>) {
    let ImportPhase::Running {
        run,
        connection,
        cancel,
        cancelling,
        ..
    } = &mut dialog.phase
    else {
        return;
    };
    // What is out on the server is the step the run last asked for. Not a
    // `COMMIT`, which a cancel would leave unknown, nor a `ROLLBACK`.
    let out = run.next();
    run.cancel();
    *cancelling = true;
    cx.notify();
    if let Some(connection) = connection.clone()
        && matches!(out, Some(Step::Begin | Step::Chunk(_)))
    {
        let cancel = cancel.clone();
        cx.background_executor()
            .spawn(on_own_thread(move || connection.cancel(&cancel)))
            .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::stopped_note;

    #[test]
    fn a_stopped_import_says_what_it_left_committed() {
        assert_eq!(
            stopped_note("`public.users`", true, 4, 10),
            "The import into `public.users` was stopped, and nothing was imported."
        );
        assert_eq!(
            stopped_note("`public.users`", true, 10, 10),
            "The import into `public.users` was stopped as it committed, so its rows may have \
             been imported."
        );
        assert_eq!(
            stopped_note("`public.users`", false, 1, 10),
            "The import into `public.users` was stopped with 1 row committed, and perhaps the \
             chunk it was sending."
        );
        assert_eq!(
            stopped_note("`public.users`", false, 12_000, 20_000),
            "The import into `public.users` was stopped with 12,000 rows committed, and perhaps \
             the chunk it was sending."
        );
    }
}
