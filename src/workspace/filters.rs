//! The filter bars as the user edits them, above the rows they narrow.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::*;

impl Workspace {
    /// Run a relation's statement again. The rows are a snapshot, and this is
    /// the only way to ask for a newer one.
    ///
    /// The structure is a snapshot too, and was loaded once when the tab
    /// opened: a column added or a key declared since then stayed invisible
    /// until the tab was closed and reopened. It is what decides which cells
    /// can be edited and which can be followed, so rows read fresh against a
    /// stale one are not a fresh answer.
    pub(crate) fn refresh_relation(&mut self, id: u64, cx: &mut Context<Self>) {
        self.clear_notice();
        if let Some((schema, relation)) = self
            .profile()
            .and_then(|profile| profile.session.objects.iter().find(|tab| tab.id == id))
            // A routine has no structure to ask for.
            .filter(|tab| matches!(tab.body, ObjectBody::Relation { .. }))
            .map(|tab| (tab.schema.clone(), tab.name.clone()))
        {
            self.load_structure(id, schema, relation, cx);
        }
        self.requery_relation(id, |_, _, _, _| true, cx);
    }

    /// Ask a relation's preview for a different number of rows.
    ///
    /// The cap is the point of the row limit, so this moves it rather than
    /// removing it: a `SELECT` with no limit at all is what the query tab is
    /// for, where the statement is the user's and its cost is theirs to judge.
    pub(crate) fn set_row_limit(
        &mut self,
        action: &SetRowLimit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = action.rows;
        self.clear_notice();
        let Some(Tab::Object(id)) = self.profile().map(|profile| profile.session.active) else {
            return;
        };
        self.requery_relation(
            id,
            move |_, _, limit, offset| {
                let moved = *limit != rows;
                *limit = rows;
                if moved {
                    // A new page size redraws every page boundary, so the only
                    // page that still means anything is the first.
                    *offset = 0;
                }
                moved
            },
            cx,
        );
    }

    /// Run the filter the bars add up to (spec §2.4). The bars are the only
    /// state there is: the expression is derived here rather than kept beside
    /// them, so the controls on screen and the statement that ran cannot
    /// disagree.
    pub(crate) fn apply_filter(&mut self, id: u64, cx: &mut Context<Self>) {
        self.clear_notice();
        let engine = self.engine();
        let Some(bars) = self.profile().and_then(|profile| {
            let tab = profile.session.objects.iter().find(|tab| tab.id == id)?;
            match &tab.body {
                ObjectBody::Relation { filters, .. } => Some(filter_bars(filters, cx)),
                ObjectBody::Routine(_) => None,
            }
        }) else {
            return;
        };
        let derived = derived_filter(engine, &bars);
        self.requery_relation(
            id,
            move |filter, _, _, offset| changed_filter(filter, offset, &derived),
            cx,
        );
    }

    /// Add a bar to the active preview, ready to be filled in. Nothing runs
    /// yet: a bar with no column and no value narrows nothing.
    pub(crate) fn add_filter(
        &mut self,
        _: &AddFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Tab::Object(id)) = self.profile().map(|profile| profile.session.active) else {
            return;
        };
        self.push_filter_row(id, window, cx);
        cx.notify();
    }

    /// Take a bar away and ask again without it.
    pub(crate) fn remove_filter(
        &mut self,
        action: &RemoveFilter,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Tab::Object(id)) = self.profile().map(|profile| profile.session.active) else {
            return;
        };
        let row = action.row;
        match self.filter_rows_mut(id) {
            Some(filters) if row < filters.len() => {
                filters.remove(row);
            }
            _ => return,
        }
        self.apply_filter(id, cx);
        cx.notify();
    }

    /// Point a bar at a column and ask again.
    pub(crate) fn set_filter_column(
        &mut self,
        action: &SetFilterColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active_object_id() else {
            return;
        };
        let Some(filter) = self.filter_row_mut(id, action.row) else {
            return;
        };
        filter.column = Some(action.column.clone());
        filter.raw = false;
        let (operator, input) = (filter.operator, filter.value.clone());
        input.update(cx, |input, cx| {
            input.set_placeholder(value_placeholder(false, operator), window, cx);
        });
        self.apply_filter(id, cx);
        cx.notify();
    }

    /// Turn a bar into a raw one. The value it already holds stays: it is the
    /// only thing the two shapes have in common, and dropping it would lose
    /// what was typed to a dropdown.
    pub(crate) fn set_filter_raw(
        &mut self,
        action: &SetFilterRaw,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active_object_id() else {
            return;
        };
        let Some(filter) = self.filter_row_mut(id, action.row) else {
            return;
        };
        filter.raw = true;
        let input = filter.value.clone();
        input.update(cx, |input, cx| {
            input.set_placeholder(value_placeholder(true, Operator::default()), window, cx);
        });
        self.apply_filter(id, cx);
        cx.notify();
    }

    pub(crate) fn set_filter_operator(
        &mut self,
        action: &SetFilterOperator,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active_object_id() else {
            return;
        };
        let Some(filter) = self.filter_row_mut(id, action.row) else {
            return;
        };
        filter.operator = action.operator;
        let input = filter.value.clone();
        input.update(cx, |input, cx| {
            input.set_placeholder(value_placeholder(false, action.operator), window, cx);
        });
        self.apply_filter(id, cx);
        cx.notify();
    }

    pub(crate) fn toggle_filter_join(
        &mut self,
        action: &ToggleFilterJoin,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active_object_id() else {
            return;
        };
        match self.filter_row_mut(id, action.row) {
            Some(filter) => filter.conjunction = filter.conjunction.toggled(),
            None => return,
        }
        self.apply_filter(id, cx);
        cx.notify();
    }

    /// Flip the joiner the next bar will carry. Shown on the "Add filter" row
    /// so the choice is made where the bar is added rather than after it lands.
    pub(crate) fn toggle_next_join(
        &mut self,
        _: &ToggleNextJoin,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.active_object_id() else {
            return;
        };
        if let Some(ObjectBody::Relation { next_join, .. }) = self.object_body_mut(id) {
            *next_join = next_join.toggled();
        }
        cx.notify();
    }

    /// Take every bar off the active preview and ask for the whole relation.
    pub(crate) fn clear_filter(&mut self, _: &ClearFilter, _: &mut Window, cx: &mut Context<Self>) {
        let Some(Tab::Object(id)) = self.profile().map(|profile| profile.session.active) else {
            return;
        };
        let Some(filters) = self.filter_rows_mut(id) else {
            return;
        };
        filters.clear();
        self.apply_filter(id, cx);
        cx.notify();
    }

    /// Put the cursor in the active preview's filter, adding the bar to type
    /// into when the stack is empty: the ask is to filter, and an empty stack
    /// has nowhere to do it.
    pub(crate) fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Tab::Object(id)) = self.profile().map(|profile| profile.session.active) else {
            return;
        };
        if self
            .filter_rows_mut(id)
            .is_some_and(|filters| filters.is_empty())
        {
            self.push_filter_row(id, window, cx);
        }
        if let Some(input) = self
            .filter_rows_mut(id)
            .and_then(|filters| filters.last())
            .map(|filter| filter.value.clone())
        {
            input.focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn push_filter_row(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let conjunction = match self.object_body_mut(id) {
            Some(ObjectBody::Relation { next_join, .. }) => *next_join,
            _ => return,
        };
        let row = filter_row(
            id,
            FilterBar {
                conjunction,
                ..FilterBar::default()
            },
            window,
            cx,
        );
        if let Some(filters) = self.filter_rows_mut(id) {
            filters.push(row);
        }
    }

    pub(crate) fn active_object_id(&self) -> Option<u64> {
        match self.profile()?.session.active {
            Tab::Object(id) => Some(id),
            Tab::Query(_) => None,
        }
    }

    pub(crate) fn object_body_mut(&mut self, id: u64) -> Option<&mut ObjectBody> {
        let tab = self
            .profile_mut()?
            .session
            .objects
            .iter_mut()
            .find(|tab| tab.id == id)?;
        Some(&mut tab.body)
    }

    pub(crate) fn filter_rows_mut(&mut self, id: u64) -> Option<&mut Vec<FilterRow>> {
        match self.object_body_mut(id)? {
            ObjectBody::Relation { filters, .. } => Some(filters),
            ObjectBody::Routine(_) => None,
        }
    }

    pub(crate) fn filter_row_mut(&mut self, id: u64, row: usize) -> Option<&mut FilterRow> {
        self.filter_rows_mut(id)?.get_mut(row)
    }
}
