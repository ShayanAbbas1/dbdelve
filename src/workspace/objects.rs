//! Opening what the explorer lists, and keeping an opened object current.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;

use super::sidebar::new_query_binding;
use crate::db::{ColumnDefinition, Structure};
use crate::explorer::Pin;
use crate::session::{
    AfterEdits, Binding, BoundSession, Explained, Finished, Queue, Routed, TabKey,
};
use crate::sidebar::{
    NodeAction, NodeKind, SelectedNode, SelectionAction, schema_row_is_a_database,
};

/// A definition being read: the connection it was asked of, by id and
/// generation, and the read itself.
type DdlRead = (String, u64, gpui::Task<Result<String, DbError>>);

/// An object's title in the Show DDL dialog, with its database when that is
/// not the connection's own: the same `main.birds` can be in both.
fn ddl_title(kind: ObjectKind, database: Option<&str>, schema: &str, name: &str) -> String {
    let kind = crate::palette::kind_label(kind);
    let named = match database {
        Some(database) => format!("{database}.{schema}.{name}"),
        None => format!("{schema}.{name}"),
    };
    format!("{}{} {named}", kind[..1].to_uppercase(), &kind[1..])
}

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

        if let Some(opened) = opened_from(catalog, target)
            && let Some(id) = self.open_object(opened, None, window, cx)
        {
            self.activate_tab(Tab::Object(id), window, cx);
            self.remember_profiles(cx);
        }
    }

    /// Open the object `target` names in the catalog read for `database`,
    /// another database of `profile`'s server, as a tab bound to it: the
    /// profile comes to the front first, and a database whose catalog has not
    /// loaded opens nothing. `target` indexes that catalog, not the
    /// profile's own, which could hold a `main.t` of its own to open instead
    /// of this one's. The connection's own database opens as it always has.
    pub(crate) fn open_object_on(
        &mut self,
        profile: &str,
        database: String,
        target: ExplorerTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.profiles.iter().position(|found| found.id == profile) else {
            return;
        };
        if self.active != Some(index) {
            self.activate(index, cx);
        }
        let found = &self.profiles[index];
        if found.active_database().as_deref() == Some(database.as_str()) {
            self.open_explorer_target(target, window, cx);
            return;
        }
        let Some(CatalogState::Loaded(catalog, _)) = found.database_catalogs.get(&database) else {
            return;
        };
        if let Some(opened) = opened_from(catalog, target)
            && let Some(id) = self.open_object(opened, Some(database), window, cx)
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
        database: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let (schema, name, kind) = (opened.schema().to_string(), opened.name(), opened.kind());
        let row_limit = self.settings.row_limit;
        let engine = self.engine();
        let target = self.target();
        let sorting = Sorting::new(self.settings.client_sort).on(engine);
        let edit_mode = self.settings.edit_mode.on(target);
        let structure = matches!(opened, OpenedObject::Structure { .. });
        let profile = self.profile_mut()?;
        let existing = matching_tab(
            profile
                .session
                .objects
                .iter()
                .filter(|tab| matches!(tab.body, ObjectBody::Structure { .. }) == structure)
                .map(|tab| {
                    (
                        tab.id,
                        tab.database.as_deref(),
                        tab.schema.as_str(),
                        tab.name.as_str(),
                        tab.filter(),
                    )
                }),
            database.as_deref(),
            &schema,
            &name,
            opened.filter(),
        );

        if let Some(id) = existing {
            return Some(id);
        }

        let id = profile.session.next_object_id;
        profile.session.next_object_id += 1;
        let completions = profile.session.filter_completions(engine, &schema, &name);
        // A data tab for the relation has already read this definition, and
        // reading it again is several statements -- on Snowflake, a warehouse
        // woken -- for the same answer. Recorded as asked, so `load_relation`
        // takes it as this tab's own; Refresh still asks, and a reconnect
        // forgets it.
        let known = structure
            .then(|| {
                profile
                    .session
                    .objects
                    .iter()
                    .find_map(|tab| match &tab.body {
                        ObjectBody::Relation {
                            structure: StructureState::Loaded(loaded),
                            ..
                        } if tab.database == database
                            && tab.schema == schema
                            && tab.name == name =>
                        {
                            Some(loaded.clone())
                        }
                        _ => None,
                    })
            })
            .flatten();
        if known.is_some() {
            profile.session.structure_requests.insert(id, 0);
        }
        let body = match opened {
            OpenedObject::Routine { routine, .. } => ObjectBody::Routine(routine),
            OpenedObject::Structure { origin, .. } => {
                let grid = structure_grid::new_grid(engine.syntax(), window, cx);
                if let Some(loaded) = &known {
                    let section = grid.read(cx).delegate().section();
                    structure_grid::show(&grid, loaded, section, cx);
                }
                ObjectBody::Structure {
                    structure: known.map_or(StructureState::Loading, StructureState::Loaded),
                    grid,
                    origin,
                }
            }
            OpenedObject::Relation {
                filter, filters, ..
            } => {
                let filters = filters
                    .into_iter()
                    .map(|bar| filter_row(engine, id, bar, completions.clone(), window, cx))
                    .collect();
                ObjectBody::Relation {
                    structure: StructureState::Loading,
                    results: result_grid::new_grid(window, cx),
                    query: QueryState::Idle,
                    sort: Vec::new(),
                    sorting,
                    edit_mode,
                    filter,
                    filters,
                    next_join: Conjunction::default(),
                    limit: row_limit,
                    offset: 0,
                    stale: false,
                    hydrated: false,
                    row_panel_folded: false,
                    row_panel_editing: false,
                    row_panel_filter: Default::default(),
                    row_panel_split: cx.new(|_| ResizableState::default()),
                    count: RowCount::Unasked,
                    sent_from: None,
                }
            }
        };
        let profile = self.profile_mut()?;
        profile.session.objects.push(ObjectTab {
            id,
            schema,
            name,
            kind: Some(kind),
            database,
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
        // The connect's completion runs the tab in front, and switching back to
        // the profile runs it if the connect landed behind another one. Run
        // now, it would only fail on the connection still opening -- over the
        // snapshot it was restored with.
        if matches!(
            self.profile().map(|profile| &profile.state),
            Some(ProfileState::Connecting { .. })
        ) {
            return;
        }
        // An idle-closed profile stays in front with its tabs, so visiting one
        // is its reconnect; the landing's `reload_front_object` loads this tab.
        if let Some(index) = self.active
            && matches!(
                self.profiles.get(index).map(|profile| &profile.state),
                Some(ProfileState::Idle)
            )
        {
            self.begin_connect(index, cx);
            return;
        }
        let (schema, relation) = (tab.schema.clone(), tab.name.clone());
        let (query, stale, sent) = match &tab.body {
            ObjectBody::Relation {
                query,
                stale,
                sent_from,
                ..
            } => (query, *stale, sent_from.is_some()),
            // Asked for once per connection, and again only after a failure.
            ObjectBody::Structure { structure, .. } => {
                let asked = self
                    .profile()
                    .is_some_and(|profile| profile.session.structure_requests.contains_key(&id));
                if !asked || matches!(structure, StructureState::Failed(_)) {
                    self.load_structure(id, schema, relation, cx);
                }
                return;
            }
            ObjectBody::Diagram { .. } => {
                self.load_diagram(id, cx);
                return;
            }
            ObjectBody::Users { .. } => {
                self.show_users(cx);
                return;
            }
            ObjectBody::Routine(_) => return,
        };
        // A tab restored from a snapshot is `Complete` over rows nothing has
        // checked against the server, so it gets exactly one run -- and its
        // structure, which no snapshot keeps.
        if !stale && !matches!(query, QueryState::Idle | QueryState::Failed(_)) {
            return;
        }
        // Failed with nothing sent is `requery_relation` refusing the tab's
        // statement, which a visit would only refuse again. A stale one was
        // marked by a reconnect, whose dialect may now read it.
        if !stale && matches!(query, QueryState::Failed(_)) && !sent {
            return;
        }
        // The rows on screen carry edits nobody has applied. A visit is not a
        // request to replace them, so the tab stays owed its reload, and asks
        // nothing: the next refresh the user asks for is the one that asks.
        if self.has_unapplied_edits(Tab::Object(id), cx) {
            return;
        }

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
        change: impl FnOnce(&mut String, &mut Vec<SortKey>, &mut Option<usize>, &mut usize) -> bool,
        cx: &mut Context<Self>,
    ) {
        // `change` is asked of copies, and the tab takes them only once the
        // action may run: a guard after the change would leave a cancelled tab
        // claiming a page, sort or limit it is not showing.
        let Some((mut new_filter, mut new_sort, mut new_limit, mut new_offset)) =
            self.profile().and_then(|profile| {
                let tab = profile.session.objects.iter().find(|tab| tab.id == id)?;
                match &tab.body {
                    ObjectBody::Relation {
                        filter,
                        sort,
                        limit,
                        offset,
                        ..
                    } => Some((filter.clone(), sort.clone(), *limit, *offset)),
                    ObjectBody::Routine(_)
                    | ObjectBody::Structure { .. }
                    | ObjectBody::Diagram { .. }
                    | ObjectBody::Users { .. } => None,
                }
            })
        else {
            return;
        };
        if !change(
            &mut new_filter,
            &mut new_sort,
            &mut new_limit,
            &mut new_offset,
        ) {
            return;
        }
        let held = AfterEdits::Requery {
            filter: new_filter.clone(),
            sort: new_sort.clone(),
            limit: new_limit,
            offset: new_offset,
        };
        if self.held_for_edits(Tab::Object(id), held, cx) {
            return;
        }
        let engine = self.engine();
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let connected = profile.connection_for(Tab::Object(id)).is_available();
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
            sent_from,
            filters,
            ..
        } = &mut tab.body
        else {
            return;
        };
        *filter = new_filter;
        *sort = new_sort;
        *limit = new_limit;
        *offset = new_offset;
        // Bars this engine refuses reach here only from a tab restored under
        // another one, whose filter is still that engine's (`restored_filter`):
        // the tab stays as it was and says why, as `apply_filter` leaves it.
        if let Some(sentence) = crate::filter::refusal(engine, &crate::filter::filter_bars(filters))
        {
            *stale = false;
            match refuse(query, sentence) {
                Some(note) => self.note(note, cx),
                None => cx.notify(),
            }
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
        *sent_from = paged.clone().map(|paged| (paged, sql.clone()));
        let Some(sql) = paged else {
            // Spent all the same. Left standing, it would have `load_relation`
            // load the structure and try again on every visit to the tab, for
            // a filter refused every time.
            *stale = false;
            let refusal = match filter.is_empty() {
                true => "dbdelve cannot read its own query for this table, so it will not run it.",
                false => "dbdelve will not run a filter it cannot read as one query.",
            };
            match refuse(query, refusal) {
                Some(note) => self.note(note, cx),
                None => cx.notify(),
            }
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

    /// The statement a preview was paged from, when `sent` is the one it last
    /// sent: anything else, such as the edit a refresh runs, was not paged.
    pub(crate) fn statement_before_paging(&self, object: u64, sent: &str) -> Option<String> {
        let tab = self
            .profile()?
            .session
            .objects
            .iter()
            .find(|tab| tab.id == object)?;
        match &tab.body {
            ObjectBody::Relation {
                sent_from: Some((paged, before)),
                ..
            } if paged == sent => Some(before.clone()),
            _ => None,
        }
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
        let target = self.target();
        let engine = target.engine;
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let routed = profile.connection_for(Tab::Object(id));
        if !routed.is_available() {
            self.note("The connection is not open.".into(), cx);
            return;
        }
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
        if sql.is_empty() {
            self.note(
                "The server has no count for this filter: only reading every row it matches \
                 would give one."
                    .into(),
                cx,
            );
            return;
        }
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
        // On a session shared with other tabs a Cancel stops only what this
        // count runs there, as a bound tab's run does (`CancelToken::alongside`).
        let bound = !matches!(routed, Routed::Own(_));
        let cancel = match bound {
            true => CancelToken::alongside(),
            false => CancelToken::default(),
        };
        *count = RowCount::Counting {
            filter: asked.clone(),
            started,
            cancel: cancel.clone(),
            cancelling: false,
        };
        let recorded = store::HistoryEntry::started(sql.as_str());
        let _ = store::append_history(&profile_id, &recorded);
        remember_statement(&mut profile.session.history, recorded.clone());
        cx.notify();

        let task = self.run_routed(
            &profile_id,
            generation,
            routed,
            move |connection| match bound {
                true => connection.generated_bound(None, &sql, &cancel, None, None),
                false => connection.generated(&sql, &cancel, None, None),
            },
            cx,
        );
        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            // Before the checks below: a filter changed or a tab closed under
            // the count drops its answer from the tab, not from the history.
            let landed =
                super::queries::answered(&recorded, &result, false, started.elapsed(), target);
            let _ = store::append_history(&profile_id, &landed);
            _ = workspace.update(cx, |workspace, cx| {
                workspace.remember_run(&profile_id, landed);
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
                        .and_then(|row| row.get(0)?.trim().parse::<u64>().ok())
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
        let Some(connection) = self
            .profile()
            .and_then(|profile| profile.connection_for(Tab::Object(id)).existing().cloned())
        else {
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
        let task = spawn_cancel(cx.background_executor(), move || connection.cancel(&cancel));
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

    /// The object `target` names in the catalog `profile` read for `database`
    /// (`None` for its own): its schema, its name as the catalog lists it, and
    /// what kind of node it is.
    pub(crate) fn object_in(
        &self,
        profile: &str,
        database: Option<&str>,
        target: ExplorerTarget,
    ) -> Option<(String, String, NodeKind)> {
        let profile = self.profiles.iter().find(|found| found.id == profile)?;
        let catalog = catalog_in(profile, database)?;
        match target {
            ExplorerTarget::Relation {
                schema_index,
                relation_index,
            } => {
                let schema = catalog.schemas.get(schema_index)?;
                let relation = schema.relations.get(relation_index)?;
                Some((
                    schema.name.clone(),
                    relation.name.clone(),
                    NodeKind::Relation(relation.kind),
                ))
            }
            ExplorerTarget::Routine {
                schema_index,
                routine_index,
            } => {
                let schema = catalog.schemas.get(schema_index)?;
                let routine = schema.routines.get(routine_index)?;
                Some((schema.name.clone(), routine.name.clone(), NodeKind::Routine))
            }
            ExplorerTarget::Sequence {
                schema_index,
                sequence_index,
            } => {
                let schema = catalog.schemas.get(schema_index)?;
                let sequence = schema.sequences.get(sequence_index)?;
                Some((schema.name.clone(), sequence.clone(), NodeKind::Sequence))
            }
        }
    }

    /// What `action` does to `node`, from a menu or the palette alike. The
    /// node's connection comes to the front first, so the gate, the mode and
    /// the engine anything it sends meets are that connection's. Nothing here
    /// sends a statement dbdelve writes: a definition is the driver's own read
    /// and a refresh its own catalog read.
    pub(crate) fn run_node_action(
        &mut self,
        node: &SelectedNode,
        action: NodeAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            NodeAction::CopyName(name) => cx.write_to_clipboard(ClipboardItem::new_string(name)),
            NodeAction::ShowDdl => {
                if !self.make_front(&node.profile, cx) {
                    return;
                }
                match (node.kind, node.schema.clone(), node.leaf) {
                    (NodeKind::Relation(kind), Some(schema), _) => self.show_ddl(
                        node.database.clone(),
                        schema,
                        node.name.clone(),
                        ObjectKind::Relation(kind),
                        cx,
                    ),
                    (NodeKind::Sequence, Some(schema), _) => self.show_ddl(
                        node.database.clone(),
                        schema,
                        node.name.clone(),
                        ObjectKind::Sequence,
                        cx,
                    ),
                    (NodeKind::Routine, _, Some(_)) => self.open_node(node, window, cx),
                    _ => {}
                }
            }
            NodeAction::ShowGrants => {
                let kind = match node.kind {
                    NodeKind::Relation(kind) => relation_kind_label(kind),
                    NodeKind::Sequence => "sequence",
                    _ => return,
                };
                self.show_object_grants(node, kind, window, cx);
            }
            NodeAction::NewQueryHere => {
                if !self.make_front(&node.profile, cx) {
                    return;
                }
                match self.view_mode() {
                    ViewMode::Complex => {
                        let target = self.target();
                        let (database, schema) = new_query_binding(node, target);
                        self.new_query_here(&node.profile, database, schema, window, cx);
                    }
                    // Simple mode has one active schema for every tab, so it
                    // moves before the tab opens, and a move that is refused
                    // leaves no tab on the schema it did not reach.
                    ViewMode::Simple => {
                        let moved = match node.kind {
                            NodeKind::Schema => {
                                let is_database = schema_row_is_a_database(self.target().levels());
                                self.move_to_schema_row(node.name.clone(), is_database, window, cx)
                            }
                            _ => true,
                        };
                        if moved {
                            self.open_query_tab(None, window, cx);
                        }
                    }
                }
            }
            NodeAction::Refresh => {
                if !self.make_front(&node.profile, cx) {
                    return;
                }
                // Another database is read over its own session, and only it:
                // the connection's own catalog is not asked again for it.
                match self.elsewhere(node.database.clone()) {
                    Some(database) => self.reload_database(&node.profile, database, cx),
                    None => self.refresh_schema(cx),
                }
            }
            NodeAction::FilterDatabases => {
                if self.make_front(&node.profile, cx) {
                    self.open_filter_picker(&node.profile, window, cx);
                }
            }
        }
    }

    /// What `action` does to every row the connections tree has selected. An
    /// action the rows do not all offer does nothing. Nothing here sends a
    /// statement dbdelve writes: a definition is the driver's own read, and a
    /// refresh its own catalog read.
    pub(crate) fn run_selection_action(
        &mut self,
        action: SelectionAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let nodes = self.selected_nodes(cx);
        if !self.selection_offers(&nodes).contains(&action) {
            return;
        }
        match action {
            SelectionAction::OpenAll => {
                for node in &nodes {
                    self.open_node(node, window, cx);
                }
            }
            SelectionAction::CopyNames => {
                let names: Vec<&str> = nodes.iter().map(|node| node.name.as_str()).collect();
                cx.write_to_clipboard(ClipboardItem::new_string(names.join("\n")));
            }
            SelectionAction::CopyDdl => self.copy_definitions(nodes, cx),
            SelectionAction::ShowDdl => match nodes.as_slice() {
                [only] => self.run_node_action(only, NodeAction::ShowDdl, window, cx),
                _ => self.show_definitions(nodes, cx),
            },
            SelectionAction::Pin => self.pin_all(&nodes, cx),
            SelectionAction::Refresh => self.refresh_all(&nodes, cx),
        }
    }

    /// Pin every object of `nodes` that is not pinned yet, each on its own
    /// connection. A row of another database names an object the shortcuts
    /// cannot hold, which is why it is not offered the pin either.
    fn pin_all(&mut self, nodes: &[SelectedNode], cx: &mut Context<Self>) {
        for node in nodes {
            let Some(leaf) = node.leaf else {
                continue;
            };
            if !self.make_front(&node.profile, cx)
                || self.elsewhere(node.database.clone()).is_some()
            {
                continue;
            }
            let Some(member) = self.member_in(&node.profile, leaf.target) else {
                continue;
            };
            let pin = Pin::Member(member);
            if self
                .profile()
                .is_some_and(|profile| profile.shortcuts.pinned(&pin))
            {
                continue;
            }
            self.toggle_pin(pin, cx);
        }
    }

    /// Read again each connection or database `nodes` belong to, once. A
    /// connection's own refresh reads every database of it that is open as
    /// well, so those are not asked for twice.
    fn refresh_all(&mut self, nodes: &[SelectedNode], cx: &mut Context<Self>) {
        // Which database a row is in depends on the connection being in
        // front, so each is brought there to be asked.
        let mut targets: Vec<(String, Option<String>)> = Vec::new();
        for node in nodes {
            if self.make_front(&node.profile, cx) {
                let target = (node.profile.clone(), self.elsewhere(node.database.clone()));
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        // A connection's own first: it reads the open databases after it.
        for (profile, _) in targets.iter().filter(|(_, database)| database.is_none()) {
            if self.make_front(profile, cx) {
                self.refresh_schema(cx);
            }
        }
        for (profile, database) in &targets {
            let Some(database) = database else {
                continue;
            };
            let covered = targets.contains(&(profile.clone(), None))
                && self.profiles.iter().any(|found| {
                    found.id == *profile
                        && matches!(found.bound.get(database), Some(BoundSession::Open(_)))
                });
            if !covered {
                self.reload_database(profile, database.clone(), cx);
            }
        }
    }

    /// Open the object `node` is on its own connection, which comes to the
    /// front first, as a click on its row does.
    fn open_node(&mut self, node: &SelectedNode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(leaf) = node.leaf else {
            return;
        };
        if !self.make_front(&node.profile, cx) {
            return;
        }
        match node.database.clone() {
            Some(database) => self.open_object_on(&node.profile, database, leaf.target, window, cx),
            None => self.open_explorer_target(leaf.target, window, cx),
        }
    }

    /// Move the connection in front onto the schema row `name`, which is a
    /// database where `is_database`. Whether it is there now: a refusal is the
    /// notice it set, and nothing moved.
    pub(crate) fn move_to_schema_row(
        &mut self,
        name: String,
        is_database: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match is_database {
            true => self.set_database(name, window, cx),
            false if self.target().picks_schema() => self.set_schema(name, cx),
            // A target that picks no schema has nothing to move.
            false => true,
        }
    }

    /// Bring the connection `id` in front, as acting on one of its rows
    /// does. Whether there is one.
    pub(crate) fn make_front(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.profiles.iter().position(|found| found.id == id) else {
            return false;
        };
        if self.active != Some(index) {
            self.activate(index, cx);
        }
        true
    }

    /// `database` as an object's own database says it: `None` for the
    /// profile's own, which is the profile's connection and not a session of
    /// its own, where asking it for itself by name would open a second one.
    pub(super) fn elsewhere(&self, database: Option<String>) -> Option<String> {
        let own = self.profile()?.active_database();
        database.filter(|database| own.as_ref() != Some(database))
    }

    /// The connection of `database` (`None` for its own) of the connection
    /// `id`, in front or not, by id and generation, with how to reach it.
    /// Another database's is its session, which a read with none open opens
    /// first.
    pub(super) fn route_in(
        &self,
        id: &str,
        database: Option<String>,
    ) -> Option<(String, u64, Routed)> {
        let profile = self.profiles.iter().find(|found| found.id == id)?;
        let own = profile.active_database();
        let database = database.filter(|database| own.as_ref() != Some(database));
        let routed = profile.route(database.map(|database| Binding {
            database,
            schema: None,
        }));
        routed
            .is_available()
            .then(|| (profile.id.clone(), profile.generation, routed))
    }

    /// Ask for `schema.name`'s definition over the connection of its
    /// database, `None` for the profile's own: another database's is read over
    /// that database's session, which a read with none open opens first.
    /// Nothing is sent but the driver's own DDL read.
    fn read_ddl(
        &mut self,
        database: Option<String>,
        schema: String,
        name: String,
        kind: ObjectKind,
        cx: &mut Context<Self>,
    ) -> Option<DdlRead> {
        let id = self.profile()?.id.clone();
        self.read_ddl_on(&id, database, schema, name, kind, cx)
    }

    /// [`Self::read_ddl`] of an object of the connection `id`, in front or not.
    fn read_ddl_on(
        &mut self,
        id: &str,
        database: Option<String>,
        schema: String,
        name: String,
        kind: ObjectKind,
        cx: &mut Context<Self>,
    ) -> Option<DdlRead> {
        let (profile_id, generation, routed) = self.route_in(id, database)?;
        let task = self.run_routed(
            &profile_id,
            generation,
            routed,
            move |connection| match kind {
                ObjectKind::Relation(kind) => connection.ddl(&schema, &name, kind),
                ObjectKind::Sequence => connection.sequence_ddl(&schema, &name),
                // A routine's definition is its tab, which `run_node_action`
                // opens; nothing asks for one here.
                ObjectKind::Routine(_) => Err(DbError {
                    message: "A routine's definition is shown in its own tab.".into(),
                    position: None,
                }),
            },
            cx,
        );
        Some((profile_id, generation, task))
    }

    /// Read the definition of each of `nodes` in turn, one at a time, over
    /// the connection and database it is in. `each` is told as every one
    /// lands, and says whether to go on; `done` is given them all. A read
    /// that fails is its own answer, and the rest are still read.
    fn read_definitions(
        &mut self,
        nodes: Vec<SelectedNode>,
        mut each: impl FnMut(&mut Self, usize, &Result<String, String>, &mut Context<Self>) -> bool
        + 'static,
        done: impl FnOnce(&mut Self, Vec<Result<String, String>>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |workspace, cx| {
            let mut results = Vec::new();
            for (index, node) in nodes.iter().enumerate() {
                let Ok(read) = workspace.update(cx, |workspace, cx| {
                    let kind = match node.kind {
                        NodeKind::Relation(kind) => ObjectKind::Relation(kind),
                        NodeKind::Sequence => ObjectKind::Sequence,
                        _ => return None,
                    };
                    workspace.read_ddl_on(
                        &node.profile,
                        node.database.clone(),
                        node.schema.clone()?,
                        node.name.clone(),
                        kind,
                        cx,
                    )
                }) else {
                    return;
                };
                let result = match read {
                    Some((profile, generation, task)) => {
                        let answer = task.await;
                        let current = workspace
                            .update(cx, |workspace, _| {
                                workspace.issued_to(&profile, generation).is_some()
                            })
                            .unwrap_or(false);
                        match answer {
                            _ if !current => {
                                Err("The connection was closed before the definition arrived."
                                    .to_string())
                            }
                            Ok(ddl) => Ok(ddl),
                            Err(error) => Err(format!("Could not read the definition: {error}")),
                        }
                    }
                    None => Err("The connection is not open.".to_string()),
                };
                match workspace.update(cx, |workspace, cx| each(workspace, index, &result, cx)) {
                    Ok(true) => results.push(result),
                    _ => return,
                }
            }
            _ = workspace.update(cx, |workspace, cx| done(workspace, results, cx));
        })
        .detach();
    }

    /// Put every definition of `nodes` on the clipboard, a blank line between
    /// them. A definition that could not be read is said so and left out.
    fn copy_definitions(&mut self, nodes: Vec<SelectedNode>, cx: &mut Context<Self>) {
        self.clear_notice();
        let names: Vec<String> = nodes.iter().map(|node| node.name.clone()).collect();
        self.read_definitions(
            nodes,
            |_, _, _, _| true,
            move |workspace, results, cx| {
                let mut texts = Vec::new();
                let mut refusals = Vec::new();
                for (name, result) in names.iter().zip(results) {
                    match result {
                        Ok(text) if !text.is_empty() => texts.push(text),
                        Ok(_) => {}
                        Err(error) => refusals.push(format!("{name}: {error}")),
                    }
                }
                let copied = texts.len();
                if copied > 0 {
                    cx.write_to_clipboard(ClipboardItem::new_string(texts.join("\n\n")));
                }
                match (refusals.is_empty(), copied) {
                    (true, _) => workspace.confirm(
                        match copied {
                            1 => "Copied the CREATE statement.".to_string(),
                            n => format!("Copied {n} CREATE statements."),
                        },
                        cx,
                    ),
                    (false, _) => workspace.note(refusals.join(" "), cx),
                }
            },
            cx,
        );
    }

    /// The `Show DDL` dialog over several objects: each definition under its
    /// name, read one after the other and drawn as it lands. The connection
    /// of the last is in front, which is the one whose dialog is drawn.
    fn show_definitions(&mut self, nodes: Vec<SelectedNode>, cx: &mut Context<Self>) {
        let Some(last) = nodes.last() else {
            return;
        };
        self.clear_notice();
        if !self.make_front(&last.profile.clone(), cx) {
            return;
        }
        let across = nodes.iter().any(|node| node.profile != nodes[0].profile);
        let sections: Vec<DdlSection> = nodes
            .iter()
            .map(|node| {
                let kind = match node.kind {
                    NodeKind::Relation(kind) => ObjectKind::Relation(kind),
                    _ => ObjectKind::Sequence,
                };
                // The connection's own database is the connection, as
                // `elsewhere` says it; only another one is named.
                let own = self
                    .profiles
                    .iter()
                    .find(|profile| profile.id == node.profile)
                    .and_then(|profile| profile.active_database());
                let database = node
                    .database
                    .as_deref()
                    .filter(|database| own.as_deref() != Some(*database));
                let mut title = ddl_title(
                    kind,
                    database,
                    node.schema.as_deref().unwrap_or(""),
                    &node.name,
                );
                // Two connections can hold the same `main.birds`.
                if across && let Some(profile) = self.profiles.iter().find(|p| p.id == node.profile)
                {
                    title = format!("{title} ({})", profile.name);
                }
                DdlSection {
                    title,
                    text: DdlText::Loading,
                }
            })
            .collect();
        self.last_ddl_request += 1;
        let request = self.last_ddl_request;
        self.ddl_view = Some(DdlView {
            profile: nodes[nodes.len() - 1].profile.clone(),
            name: String::new(),
            sections,
            request,
        });
        cx.notify();
        self.read_definitions(
            nodes,
            move |workspace, index, result, cx| {
                // Closed, or put up for another object, since: that answer is
                // not this dialog's, nor are the ones to come.
                let Some(view) = workspace
                    .ddl_view
                    .as_mut()
                    .filter(|view| view.request == request)
                else {
                    return false;
                };
                view.sections[index].text = match result {
                    Ok(ddl) => DdlText::Ready(ddl.clone()),
                    Err(error) => DdlText::Failed(error.clone()),
                };
                cx.notify();
                true
            },
            |_, _, _| {},
            cx,
        );
    }

    pub(crate) fn copy_ddl(
        &mut self,
        database: Option<String>,
        schema: String,
        relation: String,
        kind: RelationKind,
        cx: &mut Context<Self>,
    ) {
        self.clear_notice();
        let Some((profile_id, generation, task)) =
            self.read_ddl(database, schema, relation, ObjectKind::Relation(kind), cx)
        else {
            return;
        };

        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            workspace
                .update(cx, |workspace, cx| {
                    if workspace.issued_to(&profile_id, generation).is_none() {
                        return;
                    }
                    match result {
                        Ok(ddl) => {
                            cx.write_to_clipboard(ClipboardItem::new_string(ddl));
                            workspace.confirm("Copied the CREATE statement.".into(), cx);
                        }
                        Err(error) => {
                            workspace.note(format!("Could not read the DDL: {error}"), cx)
                        }
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Open the `Show DDL` dialog over `schema.relation` of `database` (`None`
    /// for the profile's own): the text is read over that database's
    /// connection and drawn read-only. It is not a buffer, so nothing in it
    /// can be run. A relation or a sequence; a routine's definition is its
    /// tab and is not shown here.
    pub(crate) fn show_ddl(
        &mut self,
        database: Option<String>,
        schema: String,
        relation: String,
        kind: ObjectKind,
        cx: &mut Context<Self>,
    ) {
        self.clear_notice();
        let shown = relation.clone();
        let shown_database = self.elsewhere(database.clone());
        let title = ddl_title(kind, shown_database.as_deref(), &schema, &relation);
        let Some((profile_id, generation, task)) =
            self.read_ddl(database, schema, relation, kind, cx)
        else {
            return;
        };
        self.last_ddl_request += 1;
        let request = self.last_ddl_request;
        self.ddl_view = Some(DdlView {
            profile: profile_id.clone(),
            name: shown,
            sections: vec![DdlSection {
                title,
                text: DdlText::Loading,
            }],
            request,
        });
        cx.notify();

        cx.spawn(async move |workspace, cx| {
            let result = task.await;
            workspace
                .update(cx, |workspace, cx| {
                    if workspace.issued_to(&profile_id, generation).is_none() {
                        return;
                    }
                    // Closed, or put up for another object, since: that answer
                    // is not this dialog's.
                    let Some(view) = workspace
                        .ddl_view
                        .as_mut()
                        .filter(|view| view.request == request)
                    else {
                        return;
                    };
                    view.sections[0].text = match result {
                        Ok(ddl) => DdlText::Ready(ddl),
                        Err(error) => {
                            DdlText::Failed(format!("Could not read the definition: {error}"))
                        }
                    };
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// The `Show DDL` dialog, when it is the front connection's: one left up
    /// for a connection that is no longer in front is not drawn and `escape`
    /// does not stop at it.
    pub(crate) fn ddl_view_in_front(&self) -> Option<&DdlView> {
        let profile = self.profile()?;
        self.ddl_view
            .as_ref()
            .filter(|view| view.profile == profile.id)
    }

    /// Close the dialog and hand the keyboard back. Answers whether it was
    /// drawn, so `escape` is only spent on a dialog that was.
    pub(crate) fn close_ddl_view(&mut self, cx: &mut Context<Self>) -> bool {
        if self.ddl_view_in_front().is_none() {
            return false;
        }
        self.ddl_view = None;
        self.refocus_front();
        cx.notify();
        true
    }

    /// Put the shown definitions on the clipboard, a blank line between them.
    /// Nothing when there is none to put there: still loading, failed, or
    /// empty.
    pub(crate) fn copy_shown_ddl(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.ddl_view_in_front() else {
            return;
        };
        let ready: Vec<&str> = view
            .sections
            .iter()
            .filter_map(|section| match &section.text {
                DdlText::Ready(text) if !text.is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if ready.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(ready.join("\n\n")));
        self.confirm("Copied the CREATE statement.".into(), cx);
    }

    /// Put one of the listed definitions on the clipboard.
    pub(crate) fn copy_ddl_section(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(DdlText::Ready(text)) = self
            .ddl_view_in_front()
            .and_then(|view| view.sections.get(at))
            .map(|section| &section.text)
        else {
            return;
        };
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.confirm("Copied the CREATE statement.".into(), cx);
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
        let routed = profile.connection_for(Tab::Object(id));
        if !routed.is_available() {
            return;
        }
        let profile_id = profile.id.clone();
        let generation = profile.generation;
        // Numbered across tabs and refreshes: a refresh forgets what was asked
        // on a connection it keeps, and a number used again would let an answer
        // from before it pass for the one after.
        profile.session.last_structure_request += 1;
        let request = profile.session.last_structure_request;
        profile.session.structure_requests.insert(id, request);
        let key = (schema.clone(), relation.clone());
        let structure_task = self.run_routed(
            &profile_id,
            generation,
            routed,
            move |connection| {
                let mut structure = connection.structure(&schema, &relation)?;
                // Only an arrow hangs off this, so a failed lookup is no arrows
                // and not a failed structure.
                structure.referenced_by = connection
                    .references(&schema, &relation)
                    .unwrap_or_default();
                Ok::<_, DbError>(structure)
            },
            cx,
        );

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
                    // make it again for this relation. Not for another
                    // database's: the cache is keyed by schema and name alone,
                    // and this would answer for the profile's own `main.t`.
                    let own = !profile
                        .session
                        .objects
                        .iter()
                        .any(|tab| tab.id == id && tab.database.is_some());
                    if own && let Ok(structure) = &result {
                        profile
                            .session
                            .completion_columns
                            .borrow_mut()
                            .insert(key, completion::ColumnState::Loaded(structure.into()));
                    }
                    // Addressed by tab, so a second object opened while this was
                    // in flight cannot end up wearing this one's columns.
                    let Some(tab) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                    else {
                        return;
                    };
                    let mut waited = false;
                    match &mut tab.body {
                        ObjectBody::Relation { structure, .. } => {
                            waited = matches!(structure, StructureState::Loading);
                            *structure = loaded(result);
                        }
                        ObjectBody::Structure {
                            structure, grid, ..
                        } => {
                            if let Ok(loaded) = &result {
                                let section = grid.read(cx).delegate().section();
                                structure_grid::show(grid, loaded, section, cx);
                            }
                            *structure = loaded(result);
                        }
                        ObjectBody::Routine(_)
                        | ObjectBody::Diagram { .. }
                        | ObjectBody::Users { .. } => {}
                    }
                    let pages_by_key = profile.config.engine().pages_by_key();
                    cx.notify();
                    // Both act on the profile in front, whose tab ids count
                    // from zero like every other's: after a switch they would
                    // reach another connection's tab of the same id. The page
                    // held back is still `Idle`, which a visit to the tab runs.
                    let in_front = workspace.profile().is_some_and(|profile| {
                        profile.id == profile_id && profile.generation == generation
                    });
                    if !in_front {
                        return;
                    }
                    // The rows and the structure are two requests and either can
                    // land last, so both sides mark.
                    workspace.mark_columns(id, cx);
                    // The page `requery_relation` held back for this key.
                    if waited && pages_by_key && !workspace.has_unapplied_edits(Tab::Object(id), cx)
                    {
                        workspace.requery_relation(id, |_, _, _, _| true, cx);
                    }
                })
                .ok();
        })
        .detach();
    }

    /// Run `work` on the connection `routed` names, on the blocking pool. A
    /// database with no session open has one opened there first, and kept
    /// where the connections tree and later reads find it
    /// ([`Workspace::keep_session`]) before `work` sends anything on it.
    /// `work` is told whether it runs on a session shared with other tabs,
    /// which a Cancel must reach only while it holds.
    pub(crate) fn run_routed<T: Send + 'static>(
        &mut self,
        id: &str,
        generation: u64,
        routed: Routed,
        work: impl FnOnce(Connection) -> Result<T, DbError> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Task<Result<T, DbError>> {
        let opens = match &routed {
            Routed::Open {
                database,
                read_only,
                ..
            } => Some((database.clone(), *read_only)),
            _ => None,
        };
        // The session already open, to take out if the read finds it lost.
        let bound = routed.bound();
        // A tab's read of its own: the user is looking at it.
        self.mark_activity(id, cx);
        let id = id.to_string();
        cx.spawn(async move |workspace, cx| {
            let resolved = match opens {
                Some(_) => spawn_blocking(cx.background_executor(), move || routed.resolve()).await,
                None => routed.resolve(),
            };
            let (connection, _) = resolved?;
            let (connection, bound) = match opens {
                Some((database, held)) => {
                    let session = workspace
                        .update(cx, |workspace, cx| {
                            workspace.keep_session(&id, generation, database, held, connection, cx)
                        })
                        .map_err(|_| DbError {
                            message: "The window is closing.".into(),
                            position: None,
                        })?
                        .ok_or_else(|| DbError {
                            message: Self::SESSION_OUTLIVED.into(),
                            position: None,
                        })?;
                    (session.connection.clone(), Some(session))
                }
                None => (connection, bound),
            };
            let (result, lost) = spawn_blocking(cx.background_executor(), move || {
                let result = work(connection.clone());
                let lost = result.is_err() && connection.is_lost();
                (result, lost)
            })
            .await;
            // A session of another database found lost is taken out, as a
            // run's is, so the next read opens another rather than failing on
            // the closed one. A replacement registered since is left alone.
            if lost && let Some(bound) = bound {
                _ = workspace.update(cx, |workspace, _| {
                    if let Some(profile) = workspace.issued_to(&id, generation) {
                        profile.bound.retain(|_, open| {
                            !matches!(open, BoundSession::Open(open) if Arc::ptr_eq(open, &bound))
                        });
                    }
                });
            }
            result
        })
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
            structure, results, ..
        } = &tab.body
        else {
            return;
        };
        let takes_imports = tab.takes_imports(self.target());
        results.update(cx, |table, _| {
            table.delegate_mut().mark_takes_imports(takes_imports)
        });
        let StructureState::Loaded(structure) = structure else {
            return;
        };
        // A row's opener carries the arrow a key does: following it is the
        // same gesture.
        let foreign_keys: Vec<String> = structure
            .foreign_keys
            .iter()
            .map(|key| key.column.clone())
            .chain(engine.opened_column().map(str::to_string))
            .collect();
        let referenced_by = structure.referenced_by.clone();
        let primary_key = structure.primary_key();
        let schema = tab.schema.clone();
        let not_nullable = columns_not_nullable(&structure.columns);
        let has_default = columns_with_defaults(engine, &structure.columns);
        let generated = columns_generated(engine, &structure.columns);
        let results = results.clone();
        results.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            grid.mark_foreign_keys(&foreign_keys);
            grid.mark_references(&referenced_by, &schema);
            grid.mark_primary_key(&primary_key);
            grid.mark_opened(engine.opened_column());
            grid.mark_columns(&not_nullable, &has_default);
            grid.mark_generated(&generated);
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
        let database = tab.database.clone();
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
            // Any cell of a row that opens as a result of its own opens it.
            if engine.opened_column().is_none() {
                return;
            }
            // The new query tab's limit, and a bound even where it has none:
            // the tab is given it too.
            let rows = self
                .settings
                .row_limit
                .unwrap_or(explorer::PREVIEW_ROW_LIMIT);
            let binding = profile.binding_of(profile.session.active);
            let opened = grid.active().and_then(|(row, _)| {
                explorer::open_row_sql(engine, rows, |column| {
                    grid.cell(row, grid.column_named(column)?)
                })
            });
            match opened {
                Some(statement) => self.run_in_new_query(statement, binding, rows, window, cx),
                None => self.note("There is no read for this row's type.".into(), cx),
            }
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
        let kind = profile
            .catalog_of(database.as_deref())
            .and_then(|catalog| {
                relation_kind(catalog, &key.referenced_schema, &key.referenced_table)
            })
            .unwrap_or_default();
        let opened = OpenedObject::Relation {
            schema: key.referenced_schema,
            name: key.referenced_table,
            kind,
            filter,
            filters,
        };

        if let Some(id) = self.open_object(opened, database, window, cx) {
            self.activate_tab(Tab::Object(id), window, cx);
            self.remember_profiles(cx);
        }
    }

    /// The arrow on a key cell. Which relations reference the key is not what
    /// the structure says -- that is every relation that *could* -- but which
    /// hold a row for this value, so each candidate is asked for one row and
    /// only the ones that answer are listed, beside any whose check failed.
    /// Asked from the keyboard, with no arrow to hang a list off, the answer
    /// is put up in the palette instead.
    /// The key in the active cell of the relation in front, as a reference
    /// lookup names it.
    pub(crate) fn reference_source(&self, cx: &App) -> Option<ReferenceSource> {
        let profile = self.profile()?;
        let tab = profile.session.active_object()?;
        let ObjectBody::Relation {
            structure: StructureState::Loaded(structure),
            results,
            ..
        } = &tab.body
        else {
            return None;
        };
        let grid = results.read(cx).delegate();
        let (_, col) = grid.active()?;
        Some(ReferenceSource {
            profile: profile.id.clone(),
            tab: tab.id,
            column: grid.columns().get(col)?.name.clone(),
            value: grid.active_value()?.to_string(),
            referenced_by: structure.referenced_by.clone(),
        })
    }

    /// A relation picked from a lookup's answer, opened only if the grid in
    /// front is still on the key it was asked about.
    pub(crate) fn open_listed_reference(
        &mut self,
        source: &ReferenceSource,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.reference_source(cx).as_ref() != Some(source) {
            self.note(
                "The key these references were listed for is no longer in front.".into(),
                cx,
            );
            return;
        }
        self.open_reference(&OpenReference { index }, window, cx);
    }

    pub(crate) fn show_references(
        &mut self,
        _: &ShowReferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.reference_source(cx) else {
            return;
        };
        let engine = self.engine();
        let Some(profile) = self.profile() else {
            return;
        };
        let Some(tab) = profile.session.active_object() else {
            return;
        };
        let routed = profile.connection_for(Tab::Object(tab.id));
        if !routed.is_available() {
            return;
        }
        let (profile_id, generation) = (profile.id.clone(), profile.generation);
        let ObjectBody::Relation {
            structure: StructureState::Loaded(structure),
            results,
            ..
        } = &tab.body
        else {
            return;
        };
        let at = results.update(cx, |table, _| table.delegate_mut().take_reference_anchor());
        let grid = results.read(cx);
        let grid = grid.delegate();
        let Some((_, col)) = grid.active() else {
            return;
        };
        let Some(name) = grid.columns().get(col).map(|column| column.name.clone()) else {
            return;
        };
        let Some(value) = grid.active_value() else {
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
            source,
            choices: None,
            check,
        });
        cx.notify();

        let labels: Vec<_> = candidates
            .iter()
            .map(|(index, label, _)| (*index, label.clone()))
            .collect();
        let task = self.run_routed(
            &profile_id,
            generation,
            routed,
            move |connection| {
                Ok(reference_answers(
                    candidates
                        .into_iter()
                        .map(|(index, label, sql)| {
                            let answer = match sql {
                                Some(sql) => connection
                                    .generated(&sql, &CancelToken::default(), None, None)
                                    .map(|result| !result.rows.is_empty())
                                    .map_err(|error| error.message),
                                None => Err("dbdelve will not run a check it cannot read as a \
                                     single read."
                                    .into()),
                            };
                            (index, label, answer)
                        })
                        .collect(),
                ))
            },
            cx,
        );
        cx.spawn_in(window, async move |workspace, cx| {
            // A session that would not open fails every check, each with the
            // open's own words.
            let found = match task.await {
                Ok(found) => found,
                Err(error) => reference_answers(
                    labels
                        .into_iter()
                        .map(|(index, label)| (index, label, Err(error.message.clone())))
                        .collect(),
                ),
            };
            _ = workspace.update_in(cx, |workspace, window, cx| {
                if workspace.issued_to(&profile_id, generation).is_none() {
                    return;
                }
                let current = workspace.reference_source(cx);
                let Some(popup) = workspace
                    .reference_popup
                    .as_mut()
                    .filter(|popup| popup.check == check)
                else {
                    return;
                };
                if current.as_ref() != Some(&popup.source) {
                    workspace.reference_popup = None;
                    cx.notify();
                    return;
                }
                let keyboard = popup.at.is_none();
                popup.choices = Some(found);
                cx.notify();
                if !keyboard {
                    return;
                }
                let listed = workspace
                    .reference_popup
                    .as_ref()
                    .and_then(|popup| popup.choices.as_ref())
                    .is_some_and(|choices| !choices.is_empty());
                // Not over a palette put up meanwhile for something else.
                if listed && workspace.palette.is_none() {
                    workspace.open_palette(PaletteMode::References, window, cx);
                } else if !listed {
                    workspace.note("No rows reference this key.".into(), cx);
                }
                workspace.reference_popup = None;
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
        let database = tab.database.clone();
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
        let kind = profile
            .catalog_of(database.as_deref())
            .and_then(|catalog| relation_kind(catalog, &reference.schema, &reference.table))
            .unwrap_or_default();
        let opened = OpenedObject::Relation {
            schema: reference.schema,
            name: reference.table,
            kind,
            filter,
            filters,
        };

        if let Some(id) = self.open_object(opened, database, window, cx) {
            self.activate_tab(Tab::Object(id), window, cx);
            self.remember_profiles(cx);
        }
    }

    /// Open a relation's rows or its structure, or bring forward the tab
    /// already showing them. The rows open unfiltered.
    // The relation's identity is its database, schema and name, with the kind
    // and which view: a struct for these would be a type three call sites pass
    // through and nothing else reads.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_relation_view(
        &mut self,
        database: Option<String>,
        schema: String,
        name: String,
        kind: RelationKind,
        structure: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let database = self.elsewhere(database);
        let opened = match structure {
            true => OpenedObject::Structure {
                schema,
                name,
                kind,
                origin: None,
            },
            false => OpenedObject::Relation {
                schema,
                name,
                kind,
                filter: String::new(),
                filters: Vec::new(),
            },
        };
        let id = self.open_object(opened, database, window, cx)?;
        self.activate_tab(Tab::Object(id), window, cx);
        self.remember_profiles(cx);
        Some(id)
    }

    pub(crate) fn toggle_structure(
        &mut self,
        _: &ToggleStructure,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_object())
        else {
            return;
        };
        let to_structure = !matches!(tab.body, ObjectBody::Structure { .. });
        self.show_structure(to_structure, window, cx);
    }

    /// The Data | Structure toggle: the other view of the relation in front.
    pub(crate) fn show_structure(
        &mut self,
        structure: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_object())
        else {
            return;
        };
        let Some(ObjectKind::Relation(kind)) = tab.kind else {
            return;
        };
        let origin = match &tab.body {
            ObjectBody::Structure { origin, .. } if !structure => origin.clone(),
            ObjectBody::Relation { .. } if structure => Some(tab.filter().to_string()),
            _ => return,
        };
        let (database, schema, name) = (tab.database.clone(), tab.schema.clone(), tab.name.clone());
        if structure {
            let id = self.open_relation_view(database, schema, name, kind, true, window, cx);
            let tab = id.and_then(|id| {
                let profile = self.profile_mut()?;
                profile.session.objects.iter_mut().find(|tab| tab.id == id)
            });
            if let Some(ObjectTab {
                body: ObjectBody::Structure { origin: slot, .. },
                ..
            }) = tab
            {
                *slot = origin;
            }
            // Opening saved the tab before it had an origin to save.
            self.remember_profiles(cx);
            return;
        }
        let data = self.profile().and_then(|profile| {
            let rows = || {
                profile.session.objects.iter().filter(|tab| {
                    matches!(tab.body, ObjectBody::Relation { .. })
                        && tab.database == database
                        && tab.schema == schema
                        && tab.name == name
                })
            };
            rows()
                .find(|tab| origin.as_deref() == Some(tab.filter()))
                .or_else(|| rows().next())
                .map(|tab| tab.id)
        });
        match data {
            Some(id) => self.activate_tab(Tab::Object(id), window, cx),
            None => {
                self.open_relation_view(database, schema, name, kind, false, window, cx);
            }
        }
    }

    /// From a structure tab's Columns section: the relation's rows, at that
    /// column. Rows still on their way are revealed at once they land.
    pub(crate) fn reveal_in_data(
        &mut self,
        column: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Off a structure tab, `show_structure` stays put, and the reveal
        // would wait on whatever tab is in front -- to tell it later, after
        // the user moved on, that its rows have no such column.
        if !self
            .profile()
            .and_then(|profile| profile.session.active_object())
            .is_some_and(|tab| matches!(tab.body, ObjectBody::Structure { .. }))
        {
            return;
        }
        self.show_structure(false, window, cx);
        let ready = self.profile().is_some_and(|profile| {
            profile
                .session
                .active_results()
                .is_some_and(|results| results.read(cx).delegate().column_named(&column).is_some())
        });
        match ready {
            true => self.reveal_column_named(&column, window, cx),
            false => {
                self.reveal_on_rows =
                    self.profile()
                        .and_then(|profile| match profile.session.active {
                            Tab::Object(id) => Some((id, column)),
                            Tab::Query(_) => None,
                        });
            }
        }
    }

    /// `reveal_on_rows`, once its rows are in: here rather than where they
    /// land, which has no window to move focus in.
    pub(crate) fn reveal_landed_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, _)) = &self.reveal_on_rows else {
            return;
        };
        let waiting = self.profile().is_some_and(|profile| {
            profile.session.active == Tab::Object(*id)
                && matches!(
                    profile.session.active_query(),
                    Some(QueryState::Idle | QueryState::Running { .. })
                )
        });
        if waiting {
            return;
        }
        let Some((id, column)) = self.reveal_on_rows.take() else {
            return;
        };
        let landed = self.profile().is_some_and(|profile| {
            profile.session.active == Tab::Object(id)
                && matches!(
                    profile.session.active_query(),
                    Some(QueryState::Complete { .. })
                )
        });
        if landed {
            self.reveal_column_named(&column, window, cx);
        }
    }

    /// The active relation's `INSERT` template on the clipboard. Nothing runs.
    pub(crate) fn copy_insert_template(&mut self, cx: &mut Context<Self>) {
        let engine = self.engine();
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_object())
        else {
            return;
        };
        let ObjectBody::Structure {
            structure: StructureState::Loaded(structure),
            ..
        } = &tab.body
        else {
            return;
        };
        let Some(sql) = sql::insert_template(engine, &tab.schema, &tab.name, &structure.columns)
        else {
            // Nothing left over a column the server generates means it
            // generates them all.
            if structure
                .columns
                .iter()
                .any(|column| engine.generates(column))
            {
                self.note("The server fills every column here.".into(), cx);
            }
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(sql));
        self.confirm("Copied the INSERT template.".into(), cx);
    }

    pub(crate) fn show_structure_section(
        &mut self,
        section: structure_grid::Section,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self
            .profile()
            .and_then(|profile| profile.session.active_object())
        else {
            return;
        };
        if let ObjectBody::Structure {
            structure: StructureState::Loaded(structure),
            grid,
            ..
        } = &tab.body
        {
            structure_grid::show(grid, structure, section, cx);
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
                let key = store::object_grid_key_in(
                    object.database.as_deref(),
                    &object.schema,
                    &object.name,
                    object.filter(),
                );
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
        let Some((results, mode, row_washes)) = self.profile().and_then(|profile| {
            Some((
                profile.session.results(tab)?.clone(),
                profile.mode,
                // A restored table starts from the default mode, and its grid
                // takes edits once the stale rows are confirmed.
                profile.session.edit_mode(tab) != crate::session::EditMode::Classic,
            ))
        }) else {
            return;
        };
        let engine = self.engine();
        show_snapshot(&results, &snapshot, mode, self.target(), cx);
        results.update(cx, |table, _| {
            table.delegate_mut().set_row_washes(row_washes)
        });
        // The snapshot holds the rows already in this order. Sorted again so
        // the headers take a click, which a restored grid's do not.
        let sorting = Sorting::restored(snapshot.client_sort.as_deref()).on(engine);
        if let Some(keys) = sorting.client_keys() {
            results.update(cx, |table, cx| {
                let order = sort_columns(engine, keys, table.delegate().columns());
                table.delegate_mut().sort_restored_in_memory(order);
                cx.notify();
            });
        }
        let row_limit = self.settings.row_limit;
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
                    if let Some((plan, showing)) = Explained::restored(&snapshot) {
                        query_tab.plan = Some(plan);
                        query_tab.showing_plan = showing;
                        // A restored queue's results are copies, none of them
                        // the slot the plan was asked of.
                        if showing {
                            query_tab.show_slot();
                        }
                    }
                    query_tab.last_query = snapshot.last_query;
                }
            }
            Tab::Object(id) => {
                if let Some(object) = profile.session.objects.iter_mut().find(|tab| tab.id == id)
                    && let ObjectBody::Relation {
                        query,
                        sort,
                        filter,
                        limit,
                        stale,
                        ..
                    } = &mut object.body
                {
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
                    *limit = match snapshot.limit {
                        Some(0) => None,
                        Some(rows) => Some(rows),
                        None => row_limit,
                    };
                    *stale = true;
                }
            }
        }
        // A snapshot from before views had a sorting reads as the server's,
        // which is how that view sorted.
        if let Some(view) = profile.session.sorting_mut(tab) {
            *view = sorting;
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
        let target = self.target();
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
            show_snapshot(&grid, &snapshot, mode, target, cx);
            done.push(Finished {
                // ponytail: a restored result has no buffer offset to point
                // at, and `start` only feeds the line number in the failure
                // dialog, which a restored result never raises -- it is
                // `Complete` or nothing. Keep the offset in the snapshot if a
                // failed one is ever restored too. Distinct all the same:
                // a header sort reruns a result as a lane, which finds its
                // entry by `start`.
                start: index,
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
        // Before anything is written: a Row view's edited row is saved and the
        // tab comes forward once it has landed (`AfterEdits::Activate`). A
        // Draft view is not asked, its draft stays with its tab. A tab just
        // closed holds no edits and mode, so `front_after_close` passes.
        let front = self.profile().map(|profile| profile.session.active);
        if let Some(front) = front
            && front != tab
            && self
                .profile()
                .is_some_and(|profile| profile.session.edit_mode(front) == EditMode::Row)
            && self.held_for_edits(front, AfterEdits::Activate(tab), cx)
        {
            return;
        }
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
        self.select_front_row(cx);
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
                self.select_front_row(cx);
            }
        }
    }

    pub(crate) fn close_object(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_run(Tab::Object(id), cx);
        // A count outliving its tab would hold the connection for an answer
        // nothing is left to show.
        self.cancel_count(id, cx);
        self.cancel_diagram(id, cx);
        let mut in_front = None;
        if let Some(profile) = self.profile_mut() {
            if profile.session.active == Tab::Object(id) {
                in_front = Some(profile.session.fallback(Tab::Object(id)));
            }
            // Read before the tab goes, because the key is made of its schema,
            // name and filter, and there is nothing left to make it from
            // afterwards. Only a relation tab writes a snapshot: a structure
            // tab's key, filterless, is its unfiltered data tab's, whose rows
            // are not this tab's to throw away.
            let snapshot = profile
                .session
                .objects
                .iter()
                .find(|tab| tab.id == id && matches!(tab.body, ObjectBody::Relation { .. }))
                .map(|tab| {
                    store::object_grid_key_in(
                        tab.database.as_deref(),
                        &tab.schema,
                        &tab.name,
                        tab.filter(),
                    )
                });
            let profile_id = profile.id.clone();
            profile.session.objects.retain(|tab| tab.id != id);
            profile.session.structure_requests.remove(&id);
            profile.session.forget(Tab::Object(id));
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
    /// opened at once, before any connection, because everything its tab
    /// needs is on disk and its snapshot is what it was showing; only a
    /// routine, whose body the tab renders, waits for the catalog's routines.
    /// Anything the database no longer has simply does not come back -- a
    /// relation it has dropped comes back as a tab whose query fails, which
    /// says so where silence did not.
    pub(crate) fn restore_objects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let pending = profile.session.pending_objects.len();
        if pending == 0 {
            return;
        }
        // Resolved one at a time, in stored order: each against the catalog
        // of its own database, which for another one may be a while coming.
        let mut opened = Vec::new();
        let mut still_pending = Vec::new();
        for stored in &profile.session.pending_objects {
            let (got, waiting) = restorable(
                profile.config.engine(),
                profile.draws_diagrams(),
                profile.catalog_state_of(stored.database.as_deref()),
                std::slice::from_ref(stored),
            );
            opened.extend(
                got.into_iter()
                    .map(|(object, active)| (object, active, stored.database.clone())),
            );
            still_pending.extend(waiting);
        }
        // Called on every frame, so a pass that could do nothing has to change
        // nothing: rewriting the profile here would write to disk per frame.
        if opened.is_empty() && still_pending.len() == pending {
            return;
        }

        if let Some(profile) = self.profile_mut() {
            profile.session.pending_objects = still_pending;
        }
        let mut restored_active = None;
        for (opened, active, database) in opened {
            let id = match opened {
                Reopened::Object(opened) => self.open_object(opened, database, window, cx),
                Reopened::Diagram(schema, view) => {
                    self.diagram_tab(database, schema, Some(view), window, cx)
                }
            };
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

/// A stored tab that can open: an object the explorer lists, or a schema's
/// diagram and how it was looked at.
enum Reopened {
    Object(OpenedObject),
    Diagram(String, store::StoredDiagram),
}

/// The catalog `profile` has read for `database`: its own for `None` or its
/// own database's name, another database's from the session opened on it.
pub(crate) fn catalog_in<'a>(profile: &'a Profile, database: Option<&str>) -> Option<&'a Catalog> {
    let state = match database {
        Some(database) if profile.active_database().as_deref() != Some(database) => {
            profile.database_catalogs.get(database)?
        }
        _ => &profile.catalog,
    };
    match state {
        CatalogState::Loaded(catalog, _) => Some(catalog),
        _ => None,
    }
}

/// The object `target` names in `catalog`, ready to open.
fn opened_from(catalog: &Catalog, target: ExplorerTarget) -> Option<OpenedObject> {
    match target {
        ExplorerTarget::Relation {
            schema_index,
            relation_index,
        } => {
            let schema = catalog.schemas.get(schema_index)?;
            let relation = schema.relations.get(relation_index)?;
            Some(OpenedObject::Relation {
                schema: schema.name.clone(),
                name: relation.name.clone(),
                kind: relation.kind,
                // A click in the explorer opens the whole relation.
                filter: String::new(),
                filters: Vec::new(),
            })
        }
        ExplorerTarget::Routine {
            schema_index,
            routine_index,
        } => {
            let schema = catalog.schemas.get(schema_index)?;
            let routine = schema.routines.get(routine_index)?;
            Some(OpenedObject::Routine {
                schema: schema.name.clone(),
                routine: routine.clone(),
            })
        }
        // A sequence is listed and nothing more: it has no tab to open (D-24).
        ExplorerTarget::Sequence { .. } => None,
    }
}

/// Split the stored tabs into those that can open now and those still waiting.
///
/// A diagram opens at once, as a relation does, and is read when shown: a
/// schema since dropped says so in its tab. That is once `draws` says the
/// profile draws diagrams, which a plugin's answers only once connected, so
/// until then it waits; on a connection that does not draw it waits on, kept
/// on disk with how it was looked at for a plugin that comes to offer `graph`.
///
/// A routine waits for the routines, not just the catalog: resolved against
/// relations alone it would find nothing and be dropped for good. One whose
/// routines failed to load keeps waiting, because a failed listing says nothing
/// about whether the routine still exists.
fn restorable(
    engine: Engine,
    draws: bool,
    catalog: &CatalogState,
    pending: &[store::StoredObject],
) -> (Vec<(Reopened, bool)>, Vec<store::StoredObject>) {
    let (catalog, routines) = match catalog {
        CatalogState::Loaded(catalog, routines) => (Some(catalog), *routines),
        _ => (None, Routines::Loading),
    };

    let mut opened = Vec::new();
    let mut still_pending = Vec::new();
    for stored in pending {
        if stored.diagram && !draws {
            still_pending.push(stored.clone());
        } else if stored.diagram {
            opened.push((
                Reopened::Diagram(
                    stored.schema.clone(),
                    stored.diagram_view.clone().unwrap_or_default(),
                ),
                stored.active,
            ));
        } else if stored.routine {
            match catalog {
                Some(catalog) if routines == Routines::Loaded => opened.extend(
                    OpenedObject::resolve(engine, catalog, stored)
                        .map(|object| (Reopened::Object(object), stored.active)),
                ),
                _ => still_pending.push(stored.clone()),
            }
        } else if stored.structure {
            opened.push((
                Reopened::Object(OpenedObject::Structure {
                    schema: stored.schema.clone(),
                    name: stored.name.clone(),
                    kind: catalog
                        .and_then(|catalog| relation_kind(catalog, &stored.schema, &stored.name))
                        .unwrap_or(stored.kind),
                    origin: stored.origin.clone(),
                }),
                stored.active,
            ));
        } else {
            let (filter, filters) = restored_filter(engine, stored);
            opened.push((
                Reopened::Object(OpenedObject::Relation {
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
                }),
                stored.active,
            ));
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

/// Why a relation tab's statement did not run, as the note to show over the
/// rows it keeps, so a filter can be corrected rather than retyped. With no
/// rows to keep the refusal is the tab's answer instead, and `None`: an idle
/// tab would spin forever.
fn refuse(query: &mut QueryState, refusal: &str) -> Option<String> {
    match query {
        QueryState::Idle | QueryState::Failed(_) => {
            *query = QueryState::Failed(DbError {
                message: refusal.into(),
                position: None,
            });
            None
        }
        _ => Some(refusal.into()),
    }
}

/// The columns the server fills itself and refuses a value for
/// ([`Engine::generates`]): on an added row of a draft they take no value,
/// where a `GENERATED BY DEFAULT` identity does.
fn columns_generated(engine: Engine, columns: &[ColumnDefinition]) -> Vec<String> {
    columns
        .iter()
        .filter(|column| engine.generates(column))
        .map(|column| column.name.clone())
        .collect()
}

fn loaded(result: Result<Structure, DbError>) -> StructureState {
    match result {
        Ok(structure) => StructureState::Loaded(structure),
        Err(error) => StructureState::Failed(error.message),
    }
}

/// What Show Grants calls a relation of `kind` under its title.
fn relation_kind_label(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table | RelationKind::PartitionedTable => "table",
        RelationKind::View => "view",
        RelationKind::MaterializedView => "materialized view",
        RelationKind::ForeignTable => "foreign table",
        RelationKind::Dictionary => "dictionary",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::{Routine, RoutineKind, Schema};

    fn stored(name: &str, routine: bool) -> store::StoredObject {
        store::StoredObject {
            database: None,
            schema: "public".into(),
            name: name.into(),
            routine,
            structure: false,
            diagram: false,
            diagram_view: None,
            kind: RelationKind::Table,
            filter: String::new(),
            filter_engine: None,
            origin: None,
            filters: Vec::new(),
            active: false,
            bars: Vec::new(),
        }
    }

    fn name(opened: &Reopened) -> String {
        match opened {
            Reopened::Object(object) => object.name(),
            Reopened::Diagram(..) => String::new(),
        }
    }

    #[test]
    fn a_stored_diagram_comes_back_in_its_place_with_how_it_was_looked_at() {
        let view = store::StoredDiagram {
            centre: Some("accounts".into()),
            keys_only: Some(true),
            zoom: Some(0.5),
            x: 10.0,
            y: -20.0,
        };
        let pending = [
            stored("accounts", false),
            store::StoredObject {
                diagram: true,
                diagram_view: Some(view.clone()),
                active: true,
                ..stored("", false)
            },
            stored("digest()", true),
        ];

        let (opened, waiting) =
            restorable(Engine::Postgres, true, &CatalogState::Loading, &pending);

        assert_eq!(opened.len(), 2, "before the catalog, as a relation");
        assert_eq!(name(&opened[0].0), "accounts");
        match &opened[1] {
            (Reopened::Diagram(schema, restored), true) => {
                assert_eq!((schema.as_str(), restored), ("public", &view));
            }
            _ => panic!("the diagram comes back second, in front"),
        }
        assert_eq!(waiting.len(), 1);
    }

    /// A plugin's profile says it draws only once connected, after the
    /// restore first runs: a diagram dropped then, as a tab that could not
    /// open, would be gone from disk at the next save.
    #[test]
    fn a_stored_diagram_waits_while_the_profile_cannot_say_it_draws() {
        let diagram = store::StoredObject {
            diagram: true,
            diagram_view: Some(store::StoredDiagram {
                zoom: Some(0.5),
                ..store::StoredDiagram::default()
            }),
            active: true,
            ..stored("", false)
        };
        let pending = [stored("accounts", false), diagram.clone()];

        let (opened, waiting) =
            restorable(Engine::Postgres, false, &CatalogState::Loading, &pending);
        assert_eq!(opened.len(), 1);
        assert_eq!(name(&opened[0].0), "accounts");
        assert_eq!(
            waiting,
            std::slice::from_ref(&diagram),
            "kept whole, view and all"
        );

        let (opened, waiting) =
            restorable(Engine::Postgres, true, &CatalogState::Loading, &waiting);
        assert!(matches!(opened[..], [(Reopened::Diagram(..), true)]));
        assert!(waiting.is_empty());
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
                    sequences: Vec::new(),
                }],
                database: None,
                ..Default::default()
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
            assert_eq!(name(&opened[0].0), "accounts");
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
    fn a_stored_routine_waits_while_the_depth_skips_the_routines() {
        let pending = [stored("accounts", false), stored("digest()", true)];

        let (opened, waiting) = restorable(
            Engine::Postgres,
            true,
            &catalog_with(Routines::Skipped),
            &pending,
        );

        assert_eq!(opened.len(), 1);
        assert_eq!(name(&opened[0].0), "accounts");
        assert_eq!(waiting, [stored("digest()", true)]);
    }

    #[test]
    fn a_stored_structure_tab_comes_back_as_a_structure_tab() {
        let pending = [
            store::StoredObject {
                database: None,
                structure: true,
                ..stored("accounts", false)
            },
            stored("accounts", false),
        ];

        let (opened, _) = restorable(Engine::Postgres, true, &CatalogState::Loading, &pending);

        assert!(matches!(
            opened[0].0,
            Reopened::Object(OpenedObject::Structure { .. })
        ));
        assert!(matches!(
            opened[1].0,
            Reopened::Object(OpenedObject::Relation { .. })
        ));
    }

    #[test]
    fn a_stored_relation_opens_before_the_connection_and_the_catalog() {
        let pending = [store::StoredObject {
            database: None,
            active: true,
            ..stored("accounts", false)
        }];

        let (opened, waiting) =
            restorable(Engine::Postgres, true, &CatalogState::Loading, &pending);

        assert_eq!(opened.len(), 1);
        assert_eq!(name(&opened[0].0), "accounts");
        assert!(opened[0].1, "the tab that was in front comes back in front");
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
