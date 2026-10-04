//! Opening what the explorer lists, and keeping an opened object current.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;

use crate::db::ColumnDefinition;
use crate::session::{Finished, Queue, TabKey};

impl Workspace {
    pub(crate) fn open_explorer_target(
        &mut self,
        target: ExplorerTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(catalog) = self.catalog() else {
            return;
        };

        let opened = match target {
            ExplorerTarget::Relation {
                schema_index,
                relation_index,
            } => catalog.schemas.get(schema_index).and_then(|schema| {
                let relation = schema.relations.get(relation_index)?;
                Some(OpenedObject::Relation {
                    schema: schema.name.clone(),
                    name: relation.name.clone(),
                    kind: relation.kind,
                    // A click in the explorer opens the whole relation.
                    filter: String::new(),
                    filters: Vec::new(),
                })
            }),
            ExplorerTarget::Routine {
                schema_index,
                routine_index,
            } => catalog.schemas.get(schema_index).and_then(|schema| {
                let routine = schema.routines.get(routine_index)?;
                Some(OpenedObject::Routine {
                    schema: schema.name.clone(),
                    routine: routine.clone(),
                })
            }),
        };

        if let Some(opened) = opened
            && let Some(id) = self.open_object(opened, window, cx)
        {
            self.activate_tab(Tab::Object(id), window, cx);
            self.remember_profiles(cx);
        }
    }

    /// Give an object a tab, reusing the one it already has. Opening does not
    /// show it — the caller decides that, so restoring a session can rebuild
    /// six tabs without running six queries.
    pub(crate) fn open_object(
        &mut self,
        opened: OpenedObject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let (schema, name, kind) = (opened.schema().to_string(), opened.name(), opened.kind());
        let preview_rows = self.settings.preview_rows;
        let engine = self.engine();
        let profile = self.profile_mut()?;
        let existing = matching_tab(
            profile
                .session
                .objects
                .iter()
                .map(|tab| (tab.id, tab.schema.as_str(), tab.name.as_str(), tab.filter())),
            &schema,
            &name,
            opened.filter(),
        );

        if let Some(id) = existing {
            return Some(id);
        }

        let id = profile.session.next_object_id;
        profile.session.next_object_id += 1;
        let body = match opened {
            OpenedObject::Routine { routine, .. } => ObjectBody::Routine(routine),
            OpenedObject::Relation {
                filter, filters, ..
            } => {
                let filters = filters
                    .into_iter()
                    .map(|bar| filter_row(engine, id, bar, window, cx))
                    .collect();
                ObjectBody::Relation {
                    showing_structure: false,
                    structure: StructureState::Loading,
                    results: result_grid::new_grid(window, cx),
                    query: QueryState::Idle,
                    sort: Vec::new(),
                    filter,
                    filters,
                    next_join: Conjunction::default(),
                    limit: preview_rows,
                    offset: 0,
                    stale: false,
                    hydrated: false,
                    row_panel_folded: false,
                    row_panel_split: cx.new(|_| ResizableState::default()),
                    count: RowCount::Unasked,
                }
            }
        };
        let profile = self.profile_mut()?;
        profile.session.objects.push(ObjectTab {
            id,
            schema,
            name,
            kind,
            body,
        });
        // See `new_query`'s own `place_last`. A session restored at launch
        // comes through here too, one object at a time and in its stored
        // order, so its objects still follow its queries.
        profile.session.place_last(TabKey::Object(id));
        Some(id)
    }

    /// Run a relation's `SELECT` and load its structure, once. Reaching a tab
    /// again must not re-query — the rows it already holds are why the tab is
    /// worth keeping open — but a failed run is not a result, so that one
    /// is allowed to be tried again.
    pub(crate) fn load_relation(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.objects.iter().find(|tab| tab.id == id))
        else {
            return;
        };
        let ObjectBody::Relation { query, stale, .. } = &tab.body else {
            return;
        };
        // A tab restored from a snapshot is `Complete` over rows nothing has
        // checked against the server, so it gets exactly one run -- and its
        // structure, which no snapshot keeps.
        if !*stale && !matches!(query, QueryState::Idle | QueryState::Failed(_)) {
            return;
        }

        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        self.load_structure(id, schema, relation, cx);
        self.requery_relation(id, |_, _, _, _| true, cx);
    }

    /// Run a relation tab's statement again, after `change` has had its say
    /// about the tab's filter, sort, row limit and page offset. `false` from
    /// `change` means nothing moved, and nothing runs.
    ///
    /// Every path that re-queries a relation comes through here. The statement
    /// is dbdelve's own, so it is regenerated from whatever the tab is now set to
    /// rather than edited — the row limit and the quoting cannot drift out of
    /// one place — and `QueryState::Idle` is what makes a preview willing to run
    /// again, so no caller can forget it.
    pub(crate) fn requery_relation(
        &mut self,
        id: u64,
        change: impl FnOnce(&mut String, &mut Vec<SortKey>, &mut usize, &mut usize) -> bool,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let connected = profile.connection().is_some();
        let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        let ObjectBody::Relation {
            sort,
            filter,
            query,
            limit,
            offset,
            stale,
            structure,
            count,
            ..
        } = &mut tab.body
        else {
            return;
        };
        if !change(filter, sort, limit, offset) {
            return;
        }
        // Discarded, not cancelled: on most engines a cancel stops whatever
        // the connection is running, which need not be this count.
        if !count.answers(filter) {
            *count = RowCount::Unasked;
        }
        let key = match structure {
            StructureState::Loaded(structure) => structure.row_key(),
            // A first page in one order and a second in another is how rows
            // repeat and go missing, so the page waits for the key. The
            // structure's arrival runs it. Not without a connection, though:
            // then no structure is coming (`load_structure` gave up, or a
            // reconnect is dropping its answer) and the run is what says the
            // connection is not open. A reconnect reloads the tab in front,
            // and any other on its next visit. `stale` is left for the run the
            // structure's arrival makes, which is this one, deferred.
            StructureState::Loading if engine.pages_by_key() && connected => return,
            _ => Vec::new(),
        };

        let sql = relation_sql(engine, &schema, &relation, filter, sort, *limit, *offset);
        // Checked before anything leaves the machine: a refused filter leaves
        // the rows on screen and the bars as they stand, so it can be corrected
        // rather than retyped.
        let paged = sql::is_generated_select(engine, &sql)
            .then(|| sql::paged(engine, &sql, &key))
            .flatten();
        let Some(sql) = paged else {
            // Spent all the same. Left standing, it would have `load_relation`
            // load the structure and try again on every visit to the tab, for
            // a filter refused every time.
            *stale = false;
            self.note(
                "dbdelve will not run a filter it cannot read as one query.".into(),
                cx,
            );
            return;
        };

        // The one run that must not blank the grid first: a restored tab's rows
        // are the rows it was showing, and clearing them to fetch the same
        // thing again is a flash of nothing. Taken here rather than tested,
        // because every later run is replacing rows the server sent and has to
        // clear them.
        let keep_rows = std::mem::take(stale);
        // A preview only re-queries when it is asked to, and this is the ask.
        // Not for the refresh, whose state still describes the rows it keeps:
        // a cancel puts that state back.
        if !keep_rows {
            *query = QueryState::Idle;
        }
        self.execute_and_then(sql, Tab::Object(id), None, keep_rows, None, cx);
    }

    /// Count the rows under a relation tab's filter, because the user asked.
    ///
    /// Never run unasked: on a large table it is a full scan, and on every
    /// engine but Snowflake it holds the profile's connection while it runs.
    /// So it goes through what a user's run does -- the same two gates the
    /// preview of this filter passed, the history, and a cancel handle the
    /// normal Cancel reaches -- but answers into the status bar rather than
    /// the grid. A statement the mode check would stop is refused rather than
    /// prompted for: the prompt resumes into the tab's grid, which is not where
    /// a count goes.
    pub(crate) fn count_rows(&mut self, id: u64, cx: &mut Context<Self>) {
        self.clear_notice();
        let engine = self.engine();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(connection) = profile.connection() else {
            self.note("The connection is not open.".into(), cx);
            return;
        };
        let (profile_id, generation, mode) = (profile.id.clone(), profile.generation, profile.mode);
        let confirmed = profile.confirmed.clone();
        let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id) else {
            return;
        };
        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        let ObjectBody::Relation { filter, count, .. } = &mut tab.body else {
            return;
        };
        if matches!(count, RowCount::Counting { .. }) {
            return;
        }
        let sql = explorer::count_sql(engine, &schema, &relation, filter);
        if !sql::is_generated_select(engine, &sql)
            || sql::gate(&sql::classify(engine, &sql), mode, &confirmed).is_some()
        {
            self.note(
                "dbdelve will not count rows under a filter it cannot run unprompted.".into(),
                cx,
            );
            return;
        }
        let asked = filter.clone();
        let started = std::time::Instant::now();
        let cancel = CancelToken::default();
        *count = RowCount::Counting {
            filter: asked.clone(),
            started,
            cancel: cancel.clone(),
            cancelling: false,
        };
        let _ = store::append_history(&profile_id, &sql);
        remember_statement(&mut profile.session.history, &sql);
        cx.notify();

        let task = cx
            .background_executor()
            .spawn(async move { connection.generated(&sql, &cancel) });
        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            _ = workspace.update(cx, |workspace, cx| {
                // By id alone, so a reconnect that retired this run still
                // takes its "Counting…" down rather than leaving it forever.
                let Some(profile) = workspace
                    .profiles
                    .iter_mut()
                    .find(|profile| profile.id == profile_id)
                else {
                    return;
                };
                let current = profile.generation == generation;
                let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id) else {
                    return;
                };
                let ObjectBody::Relation { count, .. } = &mut tab.body else {
                    return;
                };
                // A newer count, or a filter that moved since, owns the state.
                if !matches!(count, RowCount::Counting { started: at, .. } if *at == started) {
                    return;
                }
                let rows = result.map(|result| {
                    result
                        .rows
                        .first()
                        .and_then(|row| row.first()?.as_deref()?.trim().parse::<u64>().ok())
                });
                *count = match rows {
                    Ok(Some(rows)) if current => RowCount::Counted(asked, rows),
                    _ => RowCount::Unasked,
                };
                if current {
                    match rows {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            profile.session.notice =
                                Some("The count came back without a number.".into());
                        }
                        Err(error) => {
                            profile.session.notice =
                                Some(format!("The count failed: {}", error.message));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Stop a relation tab's count. Like Cancel on a run, it records only that
    /// the request went out; the count's own answer says what happened.
    pub(crate) fn cancel_count(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(connection) = self.profile().and_then(Profile::connection) else {
            return;
        };
        let Some(ObjectBody::Relation {
            count: RowCount::Counting {
                cancel, cancelling, ..
            },
            ..
        }) = self
            .profile_mut()
            .and_then(|profile| profile.session.objects.iter_mut().find(|tab| tab.id == id))
            .map(|tab| &mut tab.body)
        else {
            return;
        };
        if std::mem::replace(cancelling, true) {
            return;
        }
        let cancel = cancel.clone();
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { connection.cancel(&cancel) });
        cx.spawn(async move |workspace, cx| {
            if let Err(error) = task.await {
                _ = workspace.update(cx, |workspace, cx| workspace.note(error.message, cx));
            }
        })
        .detach();
    }

    /// A header click on a relation tab: move that column through the sort and
    /// ask the server again.
    pub(crate) fn relation_sort(&mut self, id: u64, column: usize, cx: &mut Context<Self>) {
        let engine = self.engine();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some((_, results)) = profile.session.slot(Tab::Object(id)) else {
            return;
        };
        let Some(expression) =
            sort_expression(engine, results.read(cx).delegate().columns(), column)
        else {
            return;
        };
        self.requery_relation(
            id,
            move |_, sort, _, offset| {
                cycle(sort, &expression);
                // A new ordering makes the old window meaningless: page five of
                // one sort is not page five of another.
                *offset = 0;
                true
            },
            cx,
        );
    }

    pub(crate) fn load_structure(
        &mut self,
        id: u64,
        schema: String,
        relation: String,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some(connection) = profile.connection() else {
            return;
        };
        let profile_id = profile.id.clone();
        let generation = profile.generation;
        let request = {
            let issued = profile.session.structure_requests.entry(id).or_default();
            *issued += 1;
            *issued
        };
        let key = (schema.clone(), relation.clone());
        let structure_task = cx.background_executor().spawn(async move {
            let mut structure = connection.structure(&schema, &relation)?;
            // Only an arrow hangs off this, so a failed lookup is no arrows
            // and not a failed structure.
            structure.referenced_by = connection
                .references(&schema, &relation)
                .unwrap_or_default();
            Ok::<_, DbError>(structure)
        });

        cx.spawn(async move |workspace, cx| {
            let result = structure_task.await;
            workspace
                .update(cx, |workspace, cx| {
                    let Some(profile) = workspace.issued_to(&profile_id, generation) else {
                        return;
                    };
                    // A refresh started after this one has already asked for the
                    // same definition, and its answer is the newer one.
                    if profile.session.structure_requests.get(&id) != Some(&request) {
                        return;
                    }
                    // The same call completion makes, so completion should not
                    // make it again for this relation.
                    if let Ok(structure) = &result {
                        profile.session.completion_columns.borrow_mut().insert(
                            key,
                            completion::ColumnState::Loaded(
                                structure
                                    .columns
                                    .iter()
                                    .map(|column| column.name.clone())
                                    .collect(),
                            ),
                        );
                    }
                    // Addressed by tab, so a second object opened while this was
                    // in flight cannot end up wearing this one's columns.
                    let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                    else {
                        return;
                    };
                    let mut waited = false;
                    if let ObjectBody::Relation { structure, .. } = &mut tab.body {
                        waited = matches!(structure, StructureState::Loading);
                        *structure = match result {
                            Ok(loaded) => StructureState::Loaded(loaded),
                            Err(error) => StructureState::Failed(error.message),
                        };
                        cx.notify();
                    }
                    // The rows and the structure are two requests and either can
                    // land last, so both sides mark.
                    workspace.mark_columns(id, cx);
                    // The page `requery_relation` held back for this key.
                    if waited && workspace.engine().pages_by_key() {
                        workspace.requery_relation(id, |_, _, _, _| true, cx);
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Tell a relation tab's grid what its structure says about its columns:
    /// which carry a foreign key, which the server declared `NOT NULL`, and
    /// which have a default.
    ///
    /// Called from both the structure's arrival and the rows', because they are
    /// two requests and either can land last. A completed run replaces the whole
    /// delegate, so the marks are not meant to survive a re-query.
    pub(crate) fn mark_columns(&mut self, id: u64, cx: &mut Context<Self>) {
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.objects.iter().find(|tab| tab.id == id) else {
            return;
        };
        let ObjectBody::Relation {
            structure: StructureState::Loaded(structure),
            results,
            ..
        } = &tab.body
        else {
            return;
        };
        let foreign_keys: Vec<String> = structure
            .foreign_keys
            .iter()
            .map(|key| key.column.clone())
            .collect();
        let referenced_by = structure.referenced_by.clone();
        let primary_key = structure.primary_key();
        let schema = tab.schema.clone();
        let not_nullable = columns_not_nullable(&structure.columns);
        let has_default = columns_with_defaults(engine, &structure.columns);
        let results = results.clone();
        results.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            grid.mark_foreign_keys(&foreign_keys);
            grid.mark_references(&referenced_by, &schema);
            grid.mark_primary_key(&primary_key);
            grid.mark_columns(&not_nullable, &has_default);
            cx.notify();
        });
    }

    /// Open the row the active cell references (spec §6.2): a preview of the
    /// referenced relation, filtered to the value the cell holds.
    ///
    /// Outbound only — from the row holding the key to the row it references;
    /// [`Workspace::open_reference`] goes the other way.
    /// Nothing runs for a NULL, which references nothing.
    pub(crate) fn follow_foreign_key(
        &mut self,
        _: &FollowForeignKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_object() else {
            return;
        };
        let ObjectBody::Relation {
            structure: StructureState::Loaded(structure),
            results,
            ..
        } = &tab.body
        else {
            return;
        };
        let grid = results.read(cx);
        let grid = grid.delegate();
        let Some((_, col)) = grid.active() else {
            return;
        };
        // The column's own name: the preview is dbdelve's `SELECT *`, so the
        // header is the server's word for the column rather than an alias.
        let Some(name) = grid.columns().get(col).map(|column| column.name.clone()) else {
            return;
        };
        let Some(key) = structure
            .foreign_keys
            .iter()
            .find(|key| key.column == name)
            .cloned()
        else {
            return;
        };
        let Some(bar) = foreign_key_filter(&key, grid.active_value()) else {
            return;
        };
        // The referencing column's type stands in for the referenced one's,
        // which SQL Server requires to match and whose structure is not loaded.
        let columns: Vec<ColumnDefinition> = structure
            .columns
            .iter()
            .filter(|column| column.name == key.column)
            .map(|column| ColumnDefinition {
                name: key.referenced_column.clone(),
                ..column.clone()
            })
            .collect();
        let filters = vec![bar];
        let filter = derived_filter(engine, &filters, &columns);
        // The catalog is the only authority on what the referenced relation is;
        // a default is what a tab opened before it loaded would have worn too.
        let kind = match &profile.catalog {
            CatalogState::Loaded(catalog, _) => {
                relation_kind(catalog, &key.referenced_schema, &key.referenced_table)
                    .unwrap_or_default()
            }
            _ => RelationKind::default(),
        };
        let opened = OpenedObject::Relation {
            schema: key.referenced_schema,
            name: key.referenced_table,
            kind,
            filter,
            filters,
        };

        if let Some(id) = self.open_object(opened, window, cx) {
            self.activate_tab(Tab::Object(id), window, cx);
            self.remember_profiles(cx);
        }
    }

    /// The arrow on a key cell. Which relations reference the key is not what
    /// the structure says -- that is every relation that *could* -- but which
    /// hold a row for this value, so each candidate is asked for one row and
    /// only the ones that answer are listed, beside any whose check failed.
    pub(crate) fn show_references(
        &mut self,
        _: &ShowReferences,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(connection) = profile.connection() else {
            return;
        };
        let (profile_id, generation) = (profile.id.clone(), profile.generation);
        let Some(tab) = profile.session.active_object() else {
            return;
        };
        let ObjectBody::Relation {
            structure: StructureState::Loaded(structure),
            results,
            ..
        } = &tab.body
        else {
            return;
        };
        let grid = results.read(cx);
        let grid = grid.delegate();
        let Some((_, col)) = grid.active() else {
            return;
        };
        let Some(name) = grid.columns().get(col).map(|column| column.name.clone()) else {
            return;
        };
        let (Some(value), Some(at)) = (grid.active_value(), grid.reference_anchor()) else {
            return;
        };
        let candidates: Vec<(usize, gpui::SharedString, Option<String>)> = grid
            .reference_choices(col)
            .into_iter()
            .filter_map(|(index, label)| {
                let reference = structure.referenced_by.get(index)?;
                let bar = reference_filter(reference, Some(value))?;
                let columns: Vec<ColumnDefinition> = structure
                    .columns
                    .iter()
                    .filter(|column| column.name == name)
                    .map(|column| ColumnDefinition {
                        name: reference.column.clone(),
                        ..column.clone()
                    })
                    .collect();
                let filter = derived_filter(engine, &[bar], &columns);
                let sql = explorer::probe_sql(engine, &reference.schema, &reference.table, &filter);
                let sql = sql::is_generated_select(engine, &sql)
                    .then(|| sql::paged(engine, &sql, &[]))
                    .flatten();
                Some((index, label, sql))
            })
            .collect();

        self.reference_checks += 1;
        let check = self.reference_checks;
        self.reference_popup = Some(ReferencePopup {
            at,
            choices: None,
            check,
        });
        cx.notify();

        let task = cx.background_executor().spawn(async move {
            reference_answers(
                candidates
                    .into_iter()
                    .map(|(index, label, sql)| {
                        let answer = match sql {
                            Some(sql) => connection
                                .generated(&sql, &CancelToken::default())
                                .map(|result| !result.rows.is_empty())
                                .map_err(|error| error.message),
                            None => Err(
                                "dbdelve will not run a check it cannot read as a single read."
                                    .into(),
                            ),
                        };
                        (index, label, answer)
                    })
                    .collect(),
            )
        });
        cx.spawn(async move |workspace, cx| {
            let found = task.await;
            _ = workspace.update(cx, |workspace, cx| {
                if workspace.issued_to(&profile_id, generation).is_none() {
                    return;
                }
                if let Some(popup) = &mut workspace.reference_popup
                    && popup.check == check
                {
                    popup.choices = Some(found);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Open a relation that references the key in the active cell, filtered to
    /// the rows that hold it: the inbound half of [`Workspace::follow_foreign_key`].
    /// Nothing runs for a NULL, which no row references.
    pub(crate) fn open_reference(
        &mut self,
        action: &OpenReference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_object() else {
            return;
        };
        let ObjectBody::Relation {
            structure: StructureState::Loaded(structure),
            results,
            ..
        } = &tab.body
        else {
            return;
        };
        let grid = results.read(cx);
        let grid = grid.delegate();
        let Some((_, col)) = grid.active() else {
            return;
        };
        let Some(name) = grid.columns().get(col).map(|column| column.name.clone()) else {
            return;
        };
        let Some(reference) = structure.referenced_by.get(action.index).cloned() else {
            return;
        };
        if reference.referenced_column != name {
            return;
        }
        let Some(bar) = reference_filter(&reference, grid.active_value()) else {
            return;
        };
        // The key's own type stands in for the referencing column's, which
        // SQL Server requires to match and whose structure is not loaded.
        let columns: Vec<ColumnDefinition> = structure
            .columns
            .iter()
            .filter(|column| column.name == name)
            .map(|column| ColumnDefinition {
                name: reference.column.clone(),
                ..column.clone()
            })
            .collect();
        let filters = vec![bar];
        let filter = derived_filter(engine, &filters, &columns);
        let kind = match &profile.catalog {
            CatalogState::Loaded(catalog, _) => {
                relation_kind(catalog, &reference.schema, &reference.table).unwrap_or_default()
            }
            _ => RelationKind::default(),
        };
        let opened = OpenedObject::Relation {
            schema: reference.schema,
            name: reference.table,
            kind,
            filter,
            filters,
        };

        if let Some(id) = self.open_object(opened, window, cx) {
            self.activate_tab(Tab::Object(id), window, cx);
            self.remember_profiles(cx);
        }
    }

    pub(crate) fn show_structure(&mut self, showing_structure: bool, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Tab::Object(id) = profile.session.active else {
            return;
        };
        if let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
            && let ObjectBody::Relation {
                showing_structure: showing,
                ..
            } = &mut tab.body
        {
            *showing = showing_structure;
            cx.notify();
        }
    }

    /// Put a tab's snapshot on screen, the first time the tab is looked at.
    ///
    /// Startup used to parse every stored tab's snapshot, which is a megabyte
    /// of JSON per handful of tabs decoded inside `Render` for grids nobody has
    /// asked to see. A tab that is never reached now touches the disk not at
    /// all.
    ///
    /// Nothing here can land on a live result. `hydrated` is set on the first
    /// attempt whether or not a snapshot was found, so the read happens at most
    /// once per tab; and a tab whose state is anything but `Idle` has a run of
    /// its own -- in flight, finished or failed -- so it is left alone even on
    /// that one attempt.
    pub(crate) fn hydrate_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let profile_id = profile.id.clone();
        let mut queued_results = 0;
        let key = match tab {
            Tab::Query(id) => {
                let Some(query_tab) = profile.session.query_tab_mut(id) else {
                    return;
                };
                if std::mem::replace(&mut query_tab.hydrated, true)
                    || !matches!(query_tab.query, QueryState::Idle)
                {
                    return;
                }
                queued_results = std::mem::take(&mut query_tab.queued_results);
                store::query_grid_key(id)
            }
            Tab::Object(id) => {
                let Some(object) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                else {
                    return;
                };
                let key = store::object_grid_key(&object.schema, &object.name, object.filter());
                let ObjectBody::Relation {
                    query, hydrated, ..
                } = &mut object.body
                else {
                    return;
                };
                if std::mem::replace(hydrated, true) || !matches!(query, QueryState::Idle) {
                    return;
                }
                key
            }
        };

        // Before the tab's own snapshot, and not behind it: a queue whose last
        // statement returned no columns -- a `DELETE`, an `ALTER` -- wrote no
        // `q-{id}` for its results to hang off.
        if let Tab::Query(id) = tab {
            self.restore_queue(id, queued_results, window, cx);
        }

        let Some(snapshot) = store::read_grid(&profile_id, &key) else {
            return;
        };
        let Some((results, mode)) = self
            .profile()
            .and_then(|profile| Some((profile.session.results(tab)?.clone(), profile.mode)))
        else {
            return;
        };
        show_snapshot(&results, &snapshot, mode, self.engine(), cx);
        let preview_rows = self.settings.preview_rows;
        let Some(profile) = self.profile_mut() else {
            return;
        };
        match tab {
            // A query tab's rows are all a snapshot restores: the statement
            // behind them is arbitrary SQL the user wrote, so nothing re-runs
            // it until they ask. It could be an `UPDATE ... RETURNING`.
            Tab::Query(id) => {
                if let Some(query_tab) = profile.session.query_tab_mut(id) {
                    query_tab.query = restored_state(&snapshot);
                    query_tab.last_query = snapshot.last_query;
                }
            }
            Tab::Object(id) => {
                if let Some(object) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                    && let ObjectBody::Relation {
                        showing_structure,
                        query,
                        sort,
                        filter,
                        limit,
                        stale,
                        ..
                    } = &mut object.body
                {
                    *showing_structure = snapshot.showing_structure;
                    *query = restored_state(&snapshot);
                    // The sort the snapshot's rows are actually in, so the
                    // refresh asks for the same order rather than whatever the
                    // server hands back unordered.
                    *sort = snapshot
                        .order_by
                        .iter()
                        .map(|(expression, ascending)| SortKey::new(expression.clone(), *ascending))
                        .collect();
                    // The filter the snapshot's rows were read under, so the
                    // refresh asks the same question rather than the whole
                    // table's. The bars behind it came back with the tab, which
                    // is where they are stored: the snapshot is keyed by this
                    // expression, so the two cannot disagree.
                    *filter = snapshot.filter.clone();
                    *limit = snapshot.limit.unwrap_or(preview_rows);
                    *stale = true;
                }
            }
        }
    }

    /// Rebuild a query tab's result strip from the snapshots its queue left
    /// behind, one grid per result.
    ///
    /// The queue comes back inert: nothing is left in `remaining` and nothing
    /// is `awaiting`, so no statement re-runs and a later ordinary run on this
    /// tab replaces the strip rather than being mistaken for one of its
    /// statements. The statements themselves are not kept -- the buffer is the
    /// user's and may say something else entirely by now -- so `Queue::sql` is
    /// empty and each result carries its own statement, which is all a chip
    /// needs.
    fn restore_queue(
        &mut self,
        id: u64,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((profile_id, mode)) = self
            .profile()
            .map(|profile| (profile.id.clone(), profile.mode))
        else {
            return;
        };
        let engine = self.engine();
        let mut done = Vec::new();
        for index in 0..count {
            let key = store::queued_grid_key(id, index);
            // A result that wrote no snapshot -- no columns, so nothing to keep
            // -- leaves a gap rather than ending the strip: the results after
            // it are still on disk and still worth showing.
            let Some(snapshot) = store::read_grid(&profile_id, &key) else {
                continue;
            };
            let grid = crate::result_grid::new_grid(window, cx);
            show_snapshot(&grid, &snapshot, mode, engine, cx);
            done.push(Finished {
                // ponytail: a restored result has no buffer offset to point
                // at, and `start` only feeds the line number in the failure
                // dialog, which a restored result never raises -- it is
                // `Complete` or nothing. Keep the offset in the snapshot if a
                // failed one is ever restored too.
                start: 0,
                sql: snapshot.last_query.clone().unwrap_or_default(),
                state: restored_state(&snapshot),
                grid,
            });
        }
        if done.is_empty() {
            return;
        }
        let Some(query_tab) = self
            .profile_mut()
            .and_then(|profile| profile.session.query_tab_mut(id))
        else {
            return;
        };
        query_tab.queue = Some(Queue {
            sql: String::new(),
            remaining: Vec::new(),
            showing: done.len() - 1,
            done,
            awaiting: false,
            spare: Vec::new(),
        });
    }

    pub(crate) fn activate_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.active = tab;
            profile.session.clear_prompts();
            profile.session.editor_needs_focus = true;
        }
        // Before `load_relation`, which decides whether to re-query from the
        // state the snapshot leaves the tab in: hydrating afterwards would
        // arrive over a run already in flight and be refused.
        self.hydrate_tab(tab, window, cx);
        if let Tab::Object(id) = tab {
            self.load_relation(id, cx);
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// What comes to the front once the tab in front has closed: the tab
    /// `Session::fallback` named, or with none left, the window. Never
    /// nothing: the editor that had focus has just unmounted, and focus left
    /// to fall would land outside every binding the workspace listens for.
    pub(crate) fn front_after_close(
        &mut self,
        fallback: Option<Tab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match fallback {
            Some(next) => self.activate_tab(next, window, cx),
            None => {
                if let Some(profile) = self.profile_mut() {
                    profile.session.clear_prompts();
                    profile.session.editor_needs_focus = true;
                }
            }
        }
    }

    pub(crate) fn close_object(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_run(Tab::Object(id), cx);
        // A count outliving its tab would hold the connection for an answer
        // nothing is left to show.
        self.cancel_count(id, cx);
        let mut in_front = None;
        if let Some(profile) = self.profile_mut() {
            if profile.session.active == Tab::Object(id) {
                in_front = Some(profile.session.fallback(Tab::Object(id)));
            }
            // Read before the tab goes, because the key is made of its schema,
            // name and filter, and there is nothing left to make it from
            // afterwards.
            let snapshot = profile
                .session
                .objects
                .iter()
                .find(|tab| tab.id == id)
                .map(|tab| store::object_grid_key(&tab.schema, &tab.name, tab.filter()));
            let profile_id = profile.id.clone();
            profile.session.objects.retain(|tab| tab.id != id);
            profile.session.structure_requests.remove(&id);
            // Nothing else clears a form once its tab is gone: left in place,
            // it would hold its input fields alive for nothing and `new_row`
            // reopening this id, impossible since ids never recur, is the
            // only other thing that would have found it again.
            if profile
                .session
                .insert_form
                .as_ref()
                .is_some_and(|form| form.tab == Tab::Object(id))
            {
                profile.session.insert_form = None;
            }
            if let Some(key) = snapshot {
                let _ = store::remove_grid(&profile_id, &key);
            }
        }
        if let Some(fallback) = in_front {
            self.front_after_close(fallback, window, cx);
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Turn the object tabs read back from disk into live ones. A relation is
    /// opened as soon as there is a connection to query, because everything its
    /// tab needs is on disk; only a routine, whose body the tab renders, waits
    /// for the catalog's routines. Anything the database no longer has simply does not
    /// come back -- a relation it has dropped comes back as a tab whose query
    /// fails, which says so where silence did not.
    pub(crate) fn restore_objects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let pending = profile.session.pending_objects.len();
        if pending == 0 {
            return;
        }
        let (opened, still_pending) = restorable(
            profile.config.engine(),
            profile.connection().is_some(),
            &profile.catalog,
            &profile.session.pending_objects,
        );
        // Called on every frame, so a pass that could do nothing has to change
        // nothing: rewriting the profile here would write to disk per frame.
        if opened.is_empty() && still_pending.len() == pending {
            return;
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.pending_objects = still_pending;
        }
        let mut restored_active = None;
        for (opened, active) in opened {
            let id = self.open_object(opened, window, cx);
            if active {
                restored_active = id;
            }
        }
        match restored_active {
            Some(id) => self.activate_tab(Tab::Object(id), window, cx),
            // Nothing to activate, but the pending list was drained, so what is
            // on disk has to be rewritten from the tabs that actually resolved.
            None => self.remember_profiles(cx),
        }
    }

    pub(crate) fn catalog(&self) -> Option<&Catalog> {
        match self.profile().map(|profile| &profile.catalog) {
            Some(CatalogState::Loaded(catalog, _)) => Some(catalog),
            _ => None,
        }
    }
}

/// Split the stored tabs into those that can open now and those still waiting.
///
/// A routine waits for the routines, not just the catalog: resolved against
/// relations alone it would find nothing and be dropped for good. One whose
/// routines failed to load keeps waiting, because a failed listing says nothing
/// about whether the routine still exists.
fn restorable(
    engine: Engine,
    connected: bool,
    catalog: &CatalogState,
    pending: &[store::StoredObject],
) -> (Vec<(OpenedObject, bool)>, Vec<store::StoredObject>) {
    let (catalog, routines) = match catalog {
        CatalogState::Loaded(catalog, routines) => (Some(catalog), *routines),
        _ => (None, Routines::Loading),
    };

    let mut opened = Vec::new();
    let mut still_pending = Vec::new();
    for stored in pending {
        if stored.routine {
            match catalog {
                Some(catalog) if routines == Routines::Loaded => opened.extend(
                    OpenedObject::resolve(engine, catalog, stored)
                        .map(|object| (object, stored.active)),
                ),
                _ => still_pending.push(stored.clone()),
            }
        } else if connected {
            let (filter, filters) = restored_filter(engine, stored);
            opened.push((
                OpenedObject::Relation {
                    schema: stored.schema.clone(),
                    name: stored.name.clone(),
                    // The catalog when it is here, because `stored.kind` can
                    // be the default a build that did not keep one wrote --
                    // which draws every view with a table's icon.
                    kind: catalog
                        .and_then(|catalog| relation_kind(catalog, &stored.schema, &stored.name))
                        .unwrap_or(stored.kind),
                    filter,
                    filters,
                },
                stored.active,
            ));
        } else {
            still_pending.push(stored.clone());
        }
    }
    (opened, still_pending)
}

/// The relations to list under a reference arrow: each one a row was found in,
/// and each one whose check failed, carrying what went wrong. A failed check
/// says nothing about whether a row is there, so it is never folded into the
/// relations that hold none.
fn reference_answers(
    answers: Vec<(usize, gpui::SharedString, Result<bool, String>)>,
) -> Vec<ReferenceAnswer> {
    answers
        .into_iter()
        .filter_map(|(index, label, answer)| {
            Some((
                index,
                label,
                answer.map(|found| found.then_some(())).transpose()?,
            ))
        })
        .collect()
}

/// The columns a `NULL` cannot be written into.
fn columns_not_nullable(columns: &[ColumnDefinition]) -> Vec<String> {
    columns
        .iter()
        .filter(|column| !column.nullable)
        .map(|column| column.name.clone())
        .collect()
}

/// The columns `SET x = DEFAULT` is a statement the server will accept.
///
/// Empty where the engine has no such assignment, which the engine answers
/// rather than this (`AGENTS.md` rule 4). The grid is told nothing instead: the
/// entry is absent there because the fact behind it is absent, which is how
/// every other unknown column already behaves.
///
/// `default` is read as a presence and never as an expression. It comes back as
/// a synthetic marker — `AUTO_INCREMENT`, `GENERATED BY DEFAULT AS IDENTITY` —
/// as readily as as SQL, and splicing one of those in would write the marker.
fn columns_with_defaults(engine: Engine, columns: &[ColumnDefinition]) -> Vec<String> {
    if !engine.assigns_default() {
        return Vec::new();
    }
    columns
        .iter()
        .filter(|column| column.default.is_some())
        .map(|column| column.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::{Routine, RoutineKind, Schema};

    fn stored(name: &str, routine: bool) -> store::StoredObject {
        store::StoredObject {
            schema: "public".into(),
            name: name.into(),
            routine,
            kind: RelationKind::Table,
            filter: String::new(),
            filter_engine: None,
            filters: Vec::new(),
            active: false,
            bars: Vec::new(),
        }
    }

    fn catalog_with(routines: Routines) -> CatalogState {
        let digest = Routine {
            name: "digest".into(),
            kind: RoutineKind::Function,
            identity_arguments: String::new(),
            result_type: String::new(),
            language: String::new(),
            definition: String::new(),
        };
        CatalogState::Loaded(
            Catalog {
                schemas: vec![Schema {
                    name: "public".into(),
                    relations: Vec::new(),
                    routines: match routines {
                        Routines::Loaded => vec![digest],
                        _ => Vec::new(),
                    },
                }],
            },
            routines,
        )
    }

    #[test]
    fn a_stored_routine_waits_for_the_routines_and_survives_their_failure() {
        let pending = [stored("accounts", false), stored("digest()", true)];

        // Relations are in, routines are not: the relation opens, and the
        // routine is neither resolved against a catalog without it nor dropped.
        for routines in [Routines::Loading, Routines::Failed] {
            let (opened, waiting) =
                restorable(Engine::Postgres, true, &catalog_with(routines), &pending);
            assert_eq!(opened.len(), 1);
            assert_eq!(opened[0].0.name(), "accounts");
            assert_eq!(waiting, [stored("digest()", true)]);
        }

        let (opened, waiting) = restorable(
            Engine::Postgres,
            true,
            &catalog_with(Routines::Loaded),
            &pending,
        );
        assert_eq!(opened.len(), 2);
        assert!(waiting.is_empty());
    }

    #[test]
    fn a_reference_check_that_failed_is_listed_with_its_error_not_as_no_rows() {
        let answers = reference_answers(vec![
            (0, "orders.account_id".into(), Ok(true)),
            (1, "invoices.account_id".into(), Ok(false)),
            (
                2,
                "audit.account_id".into(),
                Err("permission denied".into()),
            ),
        ]);

        assert_eq!(
            answers,
            [
                (0, "orders.account_id".into(), Ok(())),
                (
                    2,
                    "audit.account_id".into(),
                    Err("permission denied".into())
                ),
            ]
        );
        assert!(reference_answers(vec![(1, "invoices.account_id".into(), Ok(false))]).is_empty());
    }

    fn definition(name: &str, nullable: bool, default: Option<&str>) -> ColumnDefinition {
        ColumnDefinition {
            name: name.to_string(),
            data_type: "text".to_string(),
            nullable,
            default: default.map(str::to_string),
        }
    }

    #[test]
    fn a_sqlite_connection_reports_no_column_as_having_a_default() {
        // SQLite has no `DEFAULT` on the right of an `UPDATE` assignment, so
        // the entry must never be offered there -- and the grid is the wrong
        // place to decide that, since nothing above `src/db/` may branch on
        // the engine (`AGENTS.md` rule 4). It is decided here by withholding
        // the fact, which leaves the column looking like every other one
        // nothing has been said about.
        let columns = [
            definition("id", false, Some("AUTO_INCREMENT")),
            definition("note", true, Some("'unset'")),
            definition("depth", true, None),
        ];

        assert!(columns_with_defaults(Engine::Sqlite, &columns).is_empty());
        for engine in [Engine::Postgres, Engine::MySql] {
            assert_eq!(columns_with_defaults(engine, &columns), ["id", "note"]);
        }
        // Nullability is the server's answer on every engine.
        assert_eq!(columns_not_nullable(&columns), ["id"]);
    }
}
