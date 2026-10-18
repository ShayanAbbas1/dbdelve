//! Import Rows: the dialog's life on the workspace, from the file prompt
//! through the background checks of the file to the run that sends it.
//!
//! Everything here finds its dialog by profile id rather than through the
//! profile in front: a check or a chunk lands whenever it lands, and the
//! profile in front by then may be another one.

use std::time::{Duration, Instant};

use gpui::PathPromptOptions;

use super::*;
use crate::import_rows::{Options, Outcome, Plan, Run, Step, TargetColumn, prepare, send};
use crate::session::{ImportDialog, ImportPhase};

/// How long the options sit still before the file is read again, so a word
/// typed into a field re-reads once rather than per keystroke.
const RECHECK_DELAY: Duration = Duration::from_millis(150);

impl Workspace {
    pub(crate) fn import_rows(
        &mut self,
        _: &ImportRows,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.import_target(cx) {
            self.prompt_import_file(tab, window, cx);
        }
    }

    /// The object tab in front, when an import may go into it, saying why
    /// when it may not. Silent when that tab's dialog is already open.
    pub(crate) fn import_target(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        if self
            .profile()
            .and_then(|profile| profile.session.active_object())
            .is_some_and(|tab| matches!(tab.body, ObjectBody::Structure { .. }))
        {
            self.note("Switch to Data to import rows.".into(), cx);
            return None;
        }
        if !self.require(Mode::ReadWrite, cx) {
            return None;
        }
        self.clear_notice();
        let engine = self.engine();
        let session = &self.profile()?.session;
        let Some(tab) = session.active_object() else {
            self.note("Open a table to import rows into.".into(), cx);
            return None;
        };
        if !tab.takes_inserts(engine) {
            self.note("This view takes no inserts.".into(), cx);
            return None;
        }
        let id = tab.id;
        match &session.import_rows {
            Some(dialog) if dialog.tab == id => None,
            // A session holds one dialog, and replacing a running one would
            // drop its connection mid-import.
            Some(ImportDialog {
                phase: ImportPhase::Running { .. },
                schema,
                table,
                ..
            }) => {
                let message = format!("An import into `{schema}.{table}` is running.");
                self.note(message, cx);
                None
            }
            _ => Some(id),
        }
    }

    /// Ask for a file to import into `tab`. A dismissed picker does nothing.
    pub(crate) fn prompt_import_file(
        &mut self,
        tab: u64,
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
            _ = workspace.update_in(cx, |workspace, window, cx| {
                // Tab ids count up per session, so the same id in another
                // profile is another table.
                if workspace
                    .profile()
                    .is_none_or(|active| active.id != profile)
                {
                    return;
                }
                match chosen {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            workspace.open_import_file(tab, path, window, cx);
                        }
                    }
                    Ok(Err(error)) => workspace.note(error.to_string(), cx),
                    Ok(Ok(None)) | Err(_) => {}
                }
            });
        })
        .detach();
    }

    /// Read `path` and open the dialog on it over `tab`, in place of whatever
    /// file the dialog held.
    pub(crate) fn open_import_file(
        &mut self,
        tab: u64,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile().map(|profile| profile.id.clone()) else {
            return;
        };
        // ponytail: whole file in memory; stream rows if multi-GB imports matter
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move { std::fs::read(&path) }
        });
        cx.spawn_in(window, async move |workspace, cx| {
            let read = read.await;
            _ = workspace.update_in(cx, |workspace, _, cx| {
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
                let session = &mut found.session;
                let Some(object) = session.objects.iter().find(|object| object.id == tab) else {
                    return;
                };
                if session
                    .import_rows
                    .as_ref()
                    .is_some_and(|dialog| matches!(dialog.phase, ImportPhase::Running { .. }))
                {
                    return;
                }
                session.import_rows = Some(ImportDialog {
                    tab,
                    schema: object.schema.clone(),
                    table: object.name.clone(),
                    options: Options::detected(&path, &bytes),
                    path,
                    bytes: bytes.into(),
                    mappings: Vec::new(),
                    prepared: None,
                    plan: None,
                    checks: 0,
                    checking: false,
                    phase: ImportPhase::Editing,
                    error: None,
                });
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
        let session = &mut found.session;
        let Some(dialog) = session.import_rows.as_mut() else {
            return;
        };
        dialog.checks += 1;
        dialog.checking = true;
        cx.notify();
        let structure = session
            .objects
            .iter()
            .find(|tab| tab.id == dialog.tab)
            .and_then(|tab| match &tab.body {
                ObjectBody::Relation { structure, .. } => Some(structure),
                ObjectBody::Structure { .. } | ObjectBody::Routine(_) => None,
            });
        let targets = match structure {
            Some(StructureState::Loaded(structure)) => {
                TargetColumn::from_structure(structure, engine)
            }
            Some(StructureState::Failed(message)) => {
                dialog.prepared = Some(Err(message.clone()));
                dialog.plan = None;
                dialog.checking = false;
                return;
            }
            // `load_structure` checks again when the columns land.
            Some(StructureState::Loading) | None => return,
        };
        let (check, tab, bytes, options) = (
            dialog.checks,
            dialog.tab,
            dialog.bytes.clone(),
            dialog.options.clone(),
        );
        let previous = match &dialog.prepared {
            Some(Ok(prepared)) => Some((prepared.columns.clone(), dialog.mappings.clone())),
            _ => None,
        };
        let (schema, table) = (dialog.schema.clone(), dialog.table.clone());
        let profile = profile.to_string();
        cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(RECHECK_DELAY).await;
            // The file a dialog holds is its identity: a dialog reopened on
            // the same tab counts its checks from zero again.
            let file = bytes.clone();
            let newest = |dialog: &&mut ImportDialog| {
                dialog.tab == tab && dialog.checks == check && Arc::ptr_eq(&dialog.bytes, &file)
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
                    }
                    Err(message) => {
                        dialog.prepared = Some(Err(message));
                        dialog.plan = None;
                    }
                }
                dialog.checking = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Run the plan on screen. Does nothing unless the last check found no
    /// problems and is still the newest.
    pub(crate) fn start_import(&mut self, cx: &mut Context<Self>) {
        let ready = self
            .profile()
            .and_then(|profile| profile.session.import_rows.as_ref())
            .is_some_and(|dialog| {
                matches!(dialog.phase, ImportPhase::Editing)
                    && !dialog.checking
                    && dialog.plan.is_some()
                    && matches!(&dialog.prepared, Some(Ok(prepared)) if prepared.checked.problems.is_empty())
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
    /// profile's generation: it runs on a session of its own, which a
    /// reconnect of the profile's does not end.
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
                // Gone with its tab or profile, and its session with it, which
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

    /// Escape's step: closes the dialog over the tab in front, except while it
    /// runs, when only Cancel stops it.
    pub(crate) fn close_import(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(profile) = self.profile_mut() else {
            return false;
        };
        let session = &mut profile.session;
        let Some(dialog) = &session.import_rows else {
            return false;
        };
        if Tab::Object(dialog.tab) != session.active {
            return false;
        }
        if matches!(dialog.phase, ImportPhase::Editing) {
            session.import_rows = None;
            self.refocus_front();
            cx.notify();
        }
        true
    }

    /// Drop `profile`'s dialog, over `tab` or over any, stopping what it is
    /// running. Dropping its session rolls back an open transaction.
    pub(crate) fn abandon_import(
        &mut self,
        profile: &str,
        tab: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let Some(found) = self.profiles.iter_mut().find(|found| found.id == profile) else {
            return;
        };
        if let Some(mut dialog) = found
            .session
            .import_rows
            .take_if(|dialog| tab.is_none_or(|tab| dialog.tab == tab))
        {
            stop_import(&mut dialog, cx);
            cx.notify();
        }
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
        let tab = dialog.tab;
        let (changed, closes) = match outcome {
            Outcome::Imported { .. } => (true, true),
            Outcome::Cancelled { committed } => (committed > 0, true),
            Outcome::Failed { committed, .. } => (committed > 0, false),
        };
        if closes {
            session.import_rows = None;
        } else {
            dialog.phase = ImportPhase::Editing;
            dialog.error = Some(summary.clone());
        }
        if changed && !in_front {
            // Read again when it is next looked at, as after a reconnect.
            if let Some(ObjectBody::Relation { stale, .. }) = session
                .objects
                .iter_mut()
                .find(|object| object.id == tab)
                .map(|object| &mut object.body)
            {
                *stale = true;
            }
        }
        if changed && in_front {
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
