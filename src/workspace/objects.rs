//! Opening what the explorer lists, and keeping an opened object current.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;

use crate::db::ColumnDefinition;

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
            self.activate_tab(Tab::Object(id), cx);
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
                    .map(|bar| filter_row(id, bar, window, cx))
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
                }
            }
        };
        self.profile_mut()?.session.objects.push(ObjectTab {
            id,
            schema,
            name,
            kind,
            body,
        });
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
            ..
        } = &mut tab.body
        else {
            return;
        };
        if !change(filter, sort, limit, offset) {
            return;
        }

        let sql = relation_sql(engine, &schema, &relation, filter, sort, *limit, *offset);
        // Checked before anything leaves the machine, and before the tab's
        // staleness is spent: a refused filter leaves the rows on screen and
        // the bars as they stand, so it can be corrected rather than retyped.
        if !sql::is_generated_select(&sql) {
            self.note(
                "dbdelve will not run a filter it cannot read as one SELECT.".into(),
                cx,
            );
            return;
        }

        // The one run that must not blank the grid first: a restored tab's rows
        // are the rows it was showing, and clearing them to fetch the same
        // thing again is a flash of nothing. Taken here rather than tested,
        // because every later run is replacing rows the server sent and has to
        // clear them.
        let keep_rows = std::mem::take(stale);
        // A preview only re-queries when it is asked to, and this is the ask.
        *query = QueryState::Idle;
        self.execute_and_then(sql, Tab::Object(id), None, keep_rows, None, cx);
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
        let structure_task = cx
            .background_executor()
            .spawn(async move { connection.structure(&schema, &relation) });

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
                    if let ObjectBody::Relation { structure, .. } = &mut tab.body {
                        *structure = match result {
                            Ok(loaded) => StructureState::Loaded(loaded),
                            Err(error) => StructureState::Failed(error.message),
                        };
                        cx.notify();
                    }
                    // The rows and the structure are two requests and either can
                    // land last, so both sides mark.
                    workspace.mark_columns(id, cx);
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
        let not_nullable = columns_not_nullable(&structure.columns);
        let has_default = columns_with_defaults(engine, &structure.columns);
        let results = results.clone();
        results.update(cx, |table, cx| {
            let grid = table.delegate_mut();
            grid.mark_foreign_keys(&foreign_keys);
            grid.mark_columns(&not_nullable, &has_default);
            cx.notify();
        });
    }

    /// Open the row the active cell references (spec §6.2): a preview of the
    /// referenced relation, filtered to the value the cell holds.
    ///
    /// Outbound only — from the row holding the key to the row it references.
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
        let filters = vec![bar];
        let filter = derived_filter(engine, &filters);
        // The catalog is the only authority on what the referenced relation is;
        // a default is what a tab opened before it loaded would have worn too.
        let kind = match &profile.catalog {
            CatalogState::Loaded(catalog) => {
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
            self.activate_tab(Tab::Object(id), cx);
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
    pub(crate) fn hydrate_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let profile_id = profile.id.clone();
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

        let Some(snapshot) = store::read_grid(&profile_id, &key) else {
            return;
        };
        let Some((results, mode)) = self
            .profile()
            .and_then(|profile| Some((profile.session.results(tab)?.clone(), profile.mode)))
        else {
            return;
        };
        show_snapshot(&results, &snapshot, mode, cx);
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

    pub(crate) fn activate_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
            profile.session.active = tab;
            profile.session.clear_prompts();
            profile.session.editor_needs_focus = true;
        }
        // Before `load_relation`, which decides whether to re-query from the
        // state the snapshot leaves the tab in: hydrating afterwards would
        // arrive over a run already in flight and be refused.
        self.hydrate_tab(tab, cx);
        if let Tab::Object(id) = tab {
            self.load_relation(id, cx);
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    pub(crate) fn close_object(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_mut() {
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
            if let Some(key) = snapshot {
                let _ = store::remove_grid(&profile_id, &key);
            }
            if profile.session.active == Tab::Object(id)
                && let Some(first) = profile.session.queries.first().map(|tab| tab.id)
            {
                profile.session.active = Tab::Query(first);
                profile.session.editor_needs_focus = true;
            }
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Turn the object tabs read back from disk into live ones. A relation is
    /// opened as soon as there is a connection to query, because everything its
    /// tab needs is on disk; only a routine, whose body the tab renders, waits
    /// for the catalog. Anything the database no longer has simply does not
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
        let connected = profile.connection().is_some();
        let engine = profile.config.engine();
        let catalog = match &profile.catalog {
            CatalogState::Loaded(catalog) => Some(catalog),
            _ => None,
        };

        let mut opened = Vec::new();
        let mut still_pending = Vec::new();
        for stored in &profile.session.pending_objects {
            if stored.routine {
                match catalog {
                    Some(catalog) => opened.extend(
                        OpenedObject::resolve(engine, catalog, stored)
                            .map(|object| (object, stored.active)),
                    ),
                    None => still_pending.push(stored.clone()),
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
                            .and_then(|catalog| {
                                relation_kind(catalog, &stored.schema, &stored.name)
                            })
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
            Some(id) => self.activate_tab(Tab::Object(id), cx),
            // Nothing to activate, but the pending list was drained, so what is
            // on disk has to be rewritten from the tabs that actually resolved.
            None => self.remember_profiles(cx),
        }
    }

    pub(crate) fn catalog(&self) -> Option<&Catalog> {
        match self.profile().map(|profile| &profile.catalog) {
            Some(CatalogState::Loaded(catalog)) => Some(catalog),
            _ => None,
        }
    }
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
