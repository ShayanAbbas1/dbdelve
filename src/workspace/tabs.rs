//! The tab strip, the theme, and what a tab asks before it closes.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use super::sidebar::front_row_id;
use super::*;
use crate::actions::{FocusSidebar, OpenSidebarItem, ToggleResults, ToggleResultsFocus};
use crate::explorer::Shortcut;
use crate::session::{AfterEdits, PendingDiscard};
use crate::sidebar::{ViewMode, connection_row};
use crate::tab_drag::{self, Drag, Slot};

/// How tall an Edit mode input grows before it scrolls instead.
const ROW_INPUT_MAX_LINES: usize = 8;

/// Where the sidebar's selection goes for the tab in front, whose row is
/// `front`. A selected shortcut to that row keeps it, so opening a pin from
/// the keyboard does not send the selection off to the schema tree's copy.
pub(super) fn front_index(
    tree: &gpui_component::tree::TreeState,
    front: Option<&str>,
    shortcuts: &HashMap<String, Shortcut>,
) -> Option<usize> {
    let front = front?;
    tree.selected_index()
        .filter(|&ix| {
            tree.entry(ix).is_some_and(|entry| {
                crate::explorer::stands_for(&entry.item().id, front, shortcuts)
            })
        })
        .or_else(|| tree.index_of(&front.to_string().into()))
}

impl Workspace {
    pub(crate) fn toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_hidden = !self.sidebar_hidden;
        // Put away with the keyboard inside it, the sidebar would take the
        // window's only dispatch path with it.
        let held = self.explorer_focus.contains_focused(window, cx)
            || self.profile().is_some_and(|profile| {
                profile
                    .session
                    .explorer_filter
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            });
        if self.sidebar_hidden && held {
            self.refocus_front();
        }
        cx.notify();
    }

    /// The sidebar's tree takes the keyboard, the sidebar shown if it was put
    /// away, on the row of the tab in front -- or the row last selected, or
    /// the first.
    pub(crate) fn focus_sidebar(
        &mut self,
        _: &FocusSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Over a modal the stroke is the modal's: taking the keyboard out from
        // under one would leave it on screen with no way to answer it.
        if self.palette.is_some() || self.dialog_in_front.get().is_some() || self.form.is_some() {
            return;
        }
        self.sidebar_hidden = false;
        cx.notify();
        let Some(profile) = self.profile() else {
            return;
        };
        // The connections tree is always drawn, so it always has a handle a
        // window can be focused on.
        let complex = self.view_mode() == ViewMode::Complex;
        // A handle no element tracks is a window with no dispatch path.
        if !complex && !super::forms::explorer_drawn(profile) {
            return;
        }
        let (front, tree) = match complex {
            // With no object open, the connection in front is the row.
            true => (
                Some(front_row_id(profile).unwrap_or_else(|| connection_row(&profile.id))),
                self.connections.tree.clone(),
            ),
            false => (profile.front_row(), profile.session.explorer_tree.clone()),
        };
        // Rows left out of a folded subtree are built for it first.
        if complex && let Some(front) = front.as_deref() {
            self.reveal_connections_row(front, cx);
        }
        tree.update(cx, |tree, cx| {
            let front = front.map(gpui::SharedString::from).and_then(|id| {
                tree.reveal_item(&id, gpui::ScrollStrategy::Center, cx);
                tree.index_of(&id)
            });
            let row = front.or(tree.selected_index()).or(tree.entry(0).map(|_| 0));
            tree.set_selected_index(row, cx);
            if let Some(row) = row {
                tree.scroll_to_item(row, gpui::ScrollStrategy::Center);
            }
            tree.focus(window, cx);
        });
        // A hand-back asked for earlier, as the palette asks on closing, would
        // take the keyboard straight back.
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = false;
        }
    }

    /// The sidebar's selection moved onto the row of the tab in front, or
    /// off every row when that tab has none listed. Never unfolds a group to
    /// reach it: that is for `focus_sidebar`, which the user asked for.
    pub(crate) fn select_front_row(&self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let front = profile.front_row();
        let shortcuts = profile.session.explorer_shortcuts.clone();
        profile.session.explorer_tree.update(cx, |tree, cx| {
            let row = front_index(tree, front.as_deref(), &shortcuts);
            tree.set_selected_index(row, cx);
        });
    }

    /// `enter` on the sidebar's selected row: what a click on it does.
    pub(crate) fn open_sidebar_item(
        &mut self,
        _: &OpenSidebarItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view_mode() == ViewMode::Complex {
            let selected = self
                .connections
                .tree
                .read(cx)
                .selected_entry()
                .map(|entry| (entry.item().id.clone(), entry.is_folder()));
            if let Some((id, folder)) = selected
                && !self.open_connection_row(&id, window, cx)
                && folder
            {
                // Folding is the tree's own, behind its `Confirm`.
                window.dispatch_action(
                    Box::new(gpui_base::actions::Confirm { secondary: false }),
                    cx,
                );
            }
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let Some((id, folder)) = profile
            .session
            .explorer_tree
            .read(cx)
            .selected_entry()
            .map(|entry| (entry.item().id.clone(), entry.is_folder()))
        else {
            return;
        };
        let leaf = profile.session.explorer_leaves.get(id.as_ref()).copied();
        let query = crate::explorer::saved_query(&id).or_else(|| {
            profile
                .session
                .explorer_shortcuts
                .get(id.as_ref())
                .and_then(|shortcut| shortcut.query())
        });
        let file = profile
            .listed_files()
            .into_iter()
            .find(|path| crate::explorer::file_row(path) == id.as_ref());
        if let Some(path) = file {
            self.open_listed_file(path, window, cx);
        } else if let Some(name) = query {
            self.open_saved_query(name.to_string(), window, cx);
        } else if let Some(leaf) = leaf {
            self.open_explorer_target(leaf.target, window, cx);
        } else if folder {
            // Folding is the tree's own, behind its `Confirm`.
            window.dispatch_action(
                Box::new(gpui_base::actions::Confirm { secondary: false }),
                cx,
            );
        }
    }

    /// Hide the query tab's results pane, giving the editor the whole height,
    /// or bring it back. Only a query tab has one to hide: an opened relation's
    /// rows are what the tab is for.
    pub(crate) fn toggle_results(
        &mut self,
        _: &ToggleResults,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.profile_mut().map(|profile| &mut profile.session) else {
            return;
        };
        let Tab::Query(id) = session.active else {
            return;
        };
        let Some(tab) = session.query_tab_mut(id) else {
            return;
        };
        tab.results_hidden = !tab.results_hidden;
        // The grid and the new-row form beside it are about to be unmounted,
        // and a window focused on either has no path left for the chord that
        // would bring them back.
        if tab.results_hidden {
            self.refocus_front();
        }
        cx.notify();
    }

    pub(crate) fn toggle_row_panel(
        &mut self,
        _: &ToggleRowPanel,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // With no panel on screen, flipping the flag anyway would open the
        // next selection already folded.
        if !self.row_panel.on_screen.get() {
            return;
        }
        if let Some((folded, _, _)) = self.active_row_panel() {
            *folded = !*folded;
        }
        cx.notify();
    }

    /// Read ↔ Edit. Turning Edit on asks for the mode an edit needs, as a
    /// grid edit does, rather than showing inputs that would refuse every
    /// value typed into them. Turning it off takes the inputs away, and
    /// `sync_row_panel_inputs` stages what the one being typed in holds.
    pub(crate) fn toggle_row_panel_edit(&mut self, cx: &mut Context<Self>) {
        let editing = self
            .active_row_panel()
            .is_some_and(|(_, editing, _)| *editing);
        if !editing && !self.require(Mode::ReadWrite, cx) {
            return;
        }
        if let Some((folded, editing, _)) = self.active_row_panel() {
            *editing = !*editing;
            if *editing {
                *folded = false;
            }
            self.row_panel.focus_input = *editing;
        }
        cx.notify();
    }

    /// The keystroke for the pencil. It turns Edit on only where the pencil
    /// is drawn, a row with something in it to edit, but always turns it off,
    /// so a tab whose rows stopped being editable is not left stuck in it.
    pub(crate) fn toggle_row_edit(
        &mut self,
        _: &ToggleRowEdit,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editing = self
            .active_row_panel()
            .is_some_and(|(_, editing, _)| *editing);
        let editable = self
            .profile()
            .and_then(|profile| profile.session.active_results())
            .is_some_and(|results| {
                let grid = results.read(cx).delegate();
                grid.selected_row()
                    .is_some_and(|row| grid.row_editable(row))
            });
        if self.row_panel.on_screen.get() && (editing || editable) {
            self.toggle_row_panel_edit(cx);
        }
    }

    /// The tab in front's row-panel state: whether it is folded, whether it
    /// is editing, and what it is filtered by. `None` for a tab with no rows
    /// to inspect.
    pub(crate) fn active_row_panel(
        &mut self,
    ) -> Option<(&mut bool, &mut bool, &mut gpui::SharedString)> {
        let session = &mut self.profile_mut()?.session;
        match session.active {
            Tab::Query(id) => session.query_tab_mut(id).map(|tab| {
                (
                    &mut tab.row_panel_folded,
                    &mut tab.row_panel_editing,
                    &mut tab.row_panel_filter,
                )
            }),
            Tab::Object(id) => match &mut session.objects.iter_mut().find(|tab| tab.id == id)?.body
            {
                ObjectBody::Relation {
                    row_panel_folded,
                    row_panel_editing,
                    row_panel_filter,
                    ..
                } => Some((row_panel_folded, row_panel_editing, row_panel_filter)),
                _ => None,
            },
        }
    }

    /// Give Edit mode an input for every editable field of the selected row,
    /// from render for the window an input needs. Built for the grid and row
    /// in front, and built again when either changes; an input already there
    /// is refilled when its cell changed under it -- an edit in the grid, a
    /// discard, a refresh -- unless it is the one being typed in.
    pub(crate) fn sync_row_panel_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editing = self
            .active_row_panel()
            .is_some_and(|(folded, editing, _)| *editing && !*folded);
        let target = editing
            .then(|| {
                let profile = self.profile()?;
                let results = profile.session.active_results()?.clone();
                let row = results.read(cx).delegate().selected_row()?;
                Some((results, row, profile.id.clone(), profile.session.active))
            })
            .flatten();
        if self.row_panel.inputs.as_ref().is_some_and(|inputs| {
            !target
                .as_ref()
                .is_some_and(|(results, row, ..)| inputs.name(results, *row, cx))
        }) {
            self.drop_row_inputs(window, cx);
        }
        self.row_panel.typing = None;
        let Some((results, row, profile, tab)) = target else {
            return;
        };
        let rows = results.read(cx).delegate().rows_id();
        let inputs = self
            .row_panel
            .inputs
            .get_or_insert_with(|| views::RowInputs {
                grid: results.clone(),
                profile,
                tab,
                row,
                rows,
                fields: Vec::new(),
            });
        // Only the cells that need an input made or refilled are copied out,
        // since this runs every frame and a value can be a whole document.
        type Binary = Option<(Option<result_grid::BinaryForm>, Option<NewValue>, u64)>;
        let changed: Vec<(usize, Option<(gpui::SharedString, Binary)>)> = {
            let grid = results.read(cx).delegate();
            inputs.fields.resize_with(grid.columns().len(), || None);
            inputs
                .fields
                .iter()
                .enumerate()
                .filter_map(|(col, field)| {
                    if !grid.editable(row, col) {
                        return field.is_some().then_some((col, None));
                    }
                    let binary = grid.binary_seed_key(row, col);
                    // A binary seed decodes the whole value, so it is made
                    // again only when what it is made from changed.
                    if binary.is_some()
                        && field.as_ref().is_some_and(|field| {
                            field.binary.as_ref().map(|(form, pending, replaced)| {
                                (*form, pending.as_ref(), *replaced)
                            }) == binary
                        })
                    {
                        return None;
                    }
                    let seed = grid.edit_seed(row, col);
                    let binary =
                        binary.map(|(form, pending, replaced)| (form, pending.cloned(), replaced));
                    (field
                        .as_ref()
                        .map(|field| (field.seed.as_ref(), &field.binary))
                        != Some((seed.as_ref(), &binary)))
                    .then(|| {
                        let seed = gpui::SharedString::from(seed.into_owned());
                        (col, Some((seed, binary)))
                    })
                })
                .collect()
        };
        for (col, seed) in changed {
            let slot = &mut inputs.fields[col];
            let (seed, binary) = match seed {
                Some((seed, binary)) => (Some(seed), binary),
                None => (None, None),
            };
            match (slot.as_mut(), seed) {
                (_, None) => {
                    if slot
                        .as_ref()
                        .is_some_and(|field| field.input.focus_handle(cx).is_focused(window))
                    {
                        refocus_front_later(window, cx);
                    }
                    *slot = None;
                }
                (Some(field), Some(seed)) => {
                    // Text typed and not staged yet is kept over a change made
                    // elsewhere; anything else follows the cell. Bytes typed
                    // in another form would be read in this one, so a new
                    // form refills the input whatever it holds.
                    let typed = field.input.read(cx).value();
                    let reformed = field
                        .binary
                        .as_ref()
                        .zip(binary.as_ref())
                        .is_some_and(|((was, _, _), (now, _, _))| was != now);
                    if typed != field.seed && typed != seed && !reformed {
                        continue;
                    }
                    if typed != seed {
                        field
                            .input
                            .update(cx, |input, cx| input.set_value(seed.clone(), window, cx));
                    }
                    field.seed = seed;
                    field.binary = binary;
                }
                (None, Some(seed)) => {
                    let input = cx.new(|cx| {
                        gpui_component::input::TextareaState::new(window, cx)
                            .auto_grow(1, ROW_INPUT_MAX_LINES)
                            .submit_on_enter(true)
                    });
                    input.update(cx, |input, cx| input.set_value(seed.clone(), window, cx));
                    cx.subscribe(&input, |workspace, input, event: &InputEvent, cx| {
                        // `shift-enter` is a newline in the value, not a commit.
                        if matches!(
                            event,
                            InputEvent::PressEnter { shift: false, .. } | InputEvent::Blur
                        ) {
                            workspace.commit_row_input(&input, cx);
                        }
                    })
                    .detach();
                    *slot = Some(views::RowInput {
                        input,
                        seed,
                        binary,
                    });
                }
            }
        }
        self.row_panel.typing = inputs.fields.iter().position(|field| {
            field
                .as_ref()
                .is_some_and(|field| field.input.focus_handle(cx).is_focused(window))
        });
        if std::mem::take(&mut self.row_panel.focus_input) {
            let active = results
                .read(cx)
                .delegate()
                .active()
                .filter(|(at, _)| *at == row)
                .and_then(|(_, col)| inputs.fields.get(col)?.as_ref());
            if let Some(field) = active.or_else(|| inputs.fields.iter().flatten().next()) {
                field.input.update(cx, |input, cx| input.focus(window, cx));
            }
        }
    }

    /// `up` off a field's first line or `down` off its last: stage it and put
    /// the keyboard in the field beside, scrolled into view.
    pub(crate) fn step_row_input(
        &mut self,
        from: &Entity<gpui_component::input::TextareaState>,
        to: Option<(usize, Entity<gpui_component::input::TextareaState>)>,
        scope: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((ix, to)) = to else {
            return;
        };
        cx.stop_propagation();
        self.commit_row_input(from, cx);
        to.update(cx, |input, cx| input.focus(window, cx));
        crate::scroller::scroll_to("row-inspector", scope.to_string(), ix, cx);
    }

    /// Put Edit mode's inputs away, staging first whatever any of them holds
    /// that is not staged yet. No blur is coming to do it: what takes them away
    /// is a button -- Done, the fold, closing the panel -- which does not take
    /// focus, or a click on another row, whose blur gpui dispatches only after
    /// the frame this runs in. Focus goes back to the front for the reason
    /// `searched` gives.
    fn drop_row_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (inputs, _) = self.stage_row_inputs(cx);
        if inputs
            .iter()
            .any(|input| input.focus_handle(cx).is_focused(window))
        {
            refocus_front_later(window, cx);
        }
        self.row_panel.inputs = None;
    }

    /// Stage whatever Edit mode's inputs hold that is not staged yet, and hand
    /// back the inputs, and whether any input's text was refused. A close needs
    /// this before it asks whether a tab is clean: the chip's button and the
    /// chords take no focus, so no blur comes to stage a draft before the tab
    /// is gone.
    fn stage_row_inputs(
        &mut self,
        cx: &mut Context<Self>,
    ) -> (Vec<Entity<gpui_component::input::TextareaState>>, bool) {
        let Some(inputs) = self.row_panel.inputs.as_ref() else {
            return (Vec::new(), false);
        };
        let inputs: Vec<_> = inputs
            .fields
            .iter()
            .flatten()
            .map(|field| field.input.clone())
            .collect();
        let mut refused = false;
        for input in &inputs {
            refused |= !self.commit_row_input(input, cx);
        }
        (inputs, refused)
    }

    /// Edit mode's input on a cell of the grid in front, if one is built.
    pub(crate) fn row_input(
        &self,
        (row, col): (usize, usize),
        cx: &App,
    ) -> Option<Entity<gpui_component::input::TextareaState>> {
        let results = self.profile()?.session.active_results()?;
        let inputs = self
            .row_panel
            .inputs
            .as_ref()
            .filter(|inputs| inputs.name(results, row, cx))?;
        Some(inputs.fields.get(col)?.as_ref()?.input.clone())
    }

    /// Whether one of Edit mode's inputs has the keyboard.
    pub(crate) fn row_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.row_panel.inputs.as_ref().is_some_and(|inputs| {
            inputs
                .fields
                .iter()
                .flatten()
                .any(|field| field.input.focus_handle(cx).is_focused(window))
        })
    }

    /// Show the tab in front's filter in the row panel's field, which every
    /// tab shares. From render, before the panel reads the field, for the
    /// reason `sync_page_input` gives: a tab comes to the front in more places
    /// than one.
    pub(crate) fn sync_row_panel_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.row_panel.search.clone();
        let Some((_, _, filter)) = self.active_row_panel() else {
            return;
        };
        let filter = filter.clone();
        if input.read(cx).value() != filter {
            input.update(cx, |state, cx| state.set_value(filter, window, cx));
        }
    }

    /// Every chip the strip draws, left to right: the connection's index and
    /// the tab behind the chip. In complex mode every connection's, in the
    /// order the profiles are kept; in simple mode the front one's.
    pub(crate) fn strip_chips(&self) -> Vec<(usize, Tab)> {
        let complex = self.view_mode() == ViewMode::Complex;
        self.profiles
            .iter()
            .enumerate()
            .filter(|(index, _)| complex || Some(*index) == self.active)
            .flat_map(|(index, profile)| {
                profile
                    .session
                    .strip_tabs()
                    .into_iter()
                    .map(move |tab| (index, tab))
            })
            .collect()
    }

    /// Where the tab in front sits in the strip as it is drawn: in complex
    /// mode that is after every chip of the connections listed before its
    /// own, which the strip's one scroller counts too.
    pub(crate) fn front_chip(&self) -> Option<usize> {
        let front = (self.active?, self.profile()?.session.active);
        self.strip_chips().iter().position(|chip| *chip == front)
    }

    /// The connection at `index` in front, if it is not already: what a
    /// chip's click, middle click and close button each start with, so that
    /// the gate, the mode and the engine of whatever runs next are that
    /// connection's, and so that a tab id, which two connections can share,
    /// means the tab on the connection in front.
    fn bring_forward(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.active != Some(index) {
            self.activate(index, cx);
        }
    }

    /// A chip was clicked: its connection comes to the front first, then its
    /// tab.
    pub(crate) fn activate_chip(
        &mut self,
        index: usize,
        tab: Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bring_forward(index, cx);
        // `activate` leaves the connection in front on its own tab; a click
        // on a chip of the one already in front lands here with nothing to
        // bring forward.
        if self.active == Some(index) {
            self.activate_tab(tab, window, cx);
        }
    }

    /// A chip's close: asked about on the connection that owns the tab, after
    /// bringing it forward, so `ask_before_close` looks at the right tab.
    pub(crate) fn close_chip(
        &mut self,
        index: usize,
        tab: Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bring_forward(index, cx);
        if self.active == Some(index) {
            self.ask_before_close(tab, window, cx);
        }
    }

    pub(crate) fn cycle_tab(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(front) = self.active else {
            return;
        };
        // Cycling walks the strip left to right, wherever the chips were
        // dragged to, and in complex mode across connections: stepping onto
        // another connection's chip brings that connection forward.
        let chips = self.strip_chips();
        if chips.len() < 2 {
            return;
        }
        let Some(current) = self
            .profile()
            .map(|profile| (front, profile.session.active))
        else {
            return;
        };
        let Some(at) = chips.iter().position(|chip| *chip == current) else {
            return;
        };
        let (index, tab) = chips[(at as isize + step).rem_euclid(chips.len() as isize) as usize];
        self.activate_chip(index, tab, window, cx);
    }

    /// A chip has been picked up. The chips are what the strip last drew, so
    /// their bounds are where the drag measures from and not where it draws.
    pub(crate) fn begin_tab_drag(&mut self, key: TabKey, pointer_x: f32, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let order = profile.session.strip_order();
        let bounds = self.tab_strip.bounds.borrow();
        let mut slots: Vec<Slot> = bounds
            .iter()
            .filter(|(key, _)| order.contains(key))
            .map(|(key, bounds)| Slot {
                key: key.clone(),
                left: f32::from(bounds.left()),
                width: f32::from(bounds.size.width),
            })
            .collect();
        drop(bounds);
        slots.sort_by(|a, b| a.left.total_cmp(&b.left));
        let Some(index) = slots.iter().position(|slot| slot.key == key) else {
            return;
        };
        self.tab_strip.drag = Some(Drag {
            grab: pointer_x - slots[index].left,
            key,
            slots,
            index,
            pointer_x,
        });
        cx.notify();
    }

    pub(crate) fn move_tab_drag(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        if let Some(drag) = &mut self.tab_strip.drag {
            drag.pointer_x = pointer_x;
            cx.notify();
        }
    }

    /// Dropped: the strip keeps the order the drag had made room for.
    pub(crate) fn end_tab_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.tab_strip.drag.take() else {
            return;
        };
        let keys: Vec<TabKey> = drag.slots.iter().map(|slot| slot.key.clone()).collect();
        let layout = drag.layout();
        // The strip is about to be laid out in the new order, so each chip is
        // drawn where it was and glides the rest of the way.
        for (slot, moved) in drag.slots.iter().zip(&layout.landing) {
            self.tab_strip.shift.settle_into(&slot.key, *moved);
        }
        let order = tab_drag::reordered(&keys, &drag.key, layout.target);
        if let Some(profile) = self.profile_mut() {
            profile.session.tab_order = order;
        }
        cx.notify();
    }

    pub(crate) fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, window, cx);
    }

    pub(crate) fn previous_tab(
        &mut self,
        _: &PreviousTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_tab(-1, window, cx);
    }

    pub(crate) fn move_tab_left(
        &mut self,
        _: &MoveTabLeft,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_tab(-1, cx);
    }

    pub(crate) fn move_tab_right(
        &mut self,
        _: &MoveTabRight,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_tab(1, cx);
    }

    /// The keyboard's drag: the chip in front trades places with its
    /// neighbour, and the strip keeps the order as a drop does.
    pub(crate) fn move_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(session) = self.profile_mut().map(|profile| &mut profile.session) else {
            return;
        };
        if !session.move_active_tab(step) {
            return;
        }
        if let Some(index) = self.front_chip() {
            crate::scroller::scroll_to("tab-strip", "", index, cx);
        }
        cx.notify();
    }

    pub(crate) fn select_theme(
        &mut self,
        _: &SelectTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // While the theme matches the system, the list is the side the system
        // is on, so a light theme is never filed as the dark one.
        let side = self
            .settings
            .list_side(None, theme::Appearance::of_system(window.appearance()));
        self.open_palette(PaletteMode::Theme(side), window, cx);
    }

    /// Put back the theme the picker opened over. Every way out of the palette
    /// comes through here, confirming included: the row's theme is then
    /// installed again by `set_theme`, which is the one path that saves it.
    pub(crate) fn end_theme_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The snapshot predates any opacity change made while previewing, and
        // the system may have swapped it for the other side's theme since.
        if let Some(theme) = self.theme_before_preview.take() {
            self.switch_theme(theme, window, cx);
            cx.refresh_windows();
        }
    }

    /// Written through to disk, so the palette a person picked is the one the
    /// next launch paints. `side` is set when the light or dark theme is being
    /// picked, and a side the system is not on waits until it is.
    pub(crate) fn set_theme(
        &mut self,
        side: Option<theme::Appearance>,
        theme: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `set_font_size` and `set_default_row_limit` both skip the write-through
        // when nothing changed; picking the theme already installed should not
        // rewrite `profiles.toml` or re-post the notice either.
        let system = theme::Appearance::of_system(window.appearance());
        let (slot, shows) = self.settings.pick_target(side, system);
        let unchanged = slot.is_none_or(|slot| self.settings.theme_for(slot) == theme.name)
            && (!shows || theme::theme(cx).name == theme.name);
        if unchanged {
            return;
        }
        if let Some(slot) = slot {
            self.settings.set_theme_for(slot, theme.name);
        }
        if shows {
            self.switch_theme(theme, window, cx);
        }
        // The titlebar deliberately no longer names the theme -- permanent
        // chrome should not narrate a setting -- so the switch itself says
        // where it landed.
        if self.profile().is_some() {
            let label = side.map_or("Theme", theme::Appearance::theme_label);
            self.note(format!("{label}: {}", theme.name), cx);
        }
        self.remember_profiles(cx);
        cx.refresh_windows();
    }

    fn switch_theme(&mut self, theme: Theme, window: &mut Window, cx: &mut Context<Self>) {
        let theme = theme.with_opacity(self.settings.opacity_for(&theme));
        install_theme(theme, window, cx);
        let percent = opacity_percent(theme.opacity).to_string();
        self.opacity_input
            .update(cx, |input, cx| input.set_value(percent, window, cx));
    }

    /// Turned on, the theme on screen becomes the one for its own side, so
    /// nothing changes unless the system is on the other. Turned off, the
    /// theme on screen stays.
    pub(crate) fn set_match_system_appearance(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.match_system_appearance = on;
        if on {
            let current = self.theme_before_preview.unwrap_or(*theme::theme(cx));
            self.settings
                .set_theme_for(current.appearance, current.name);
            self.follow_system_appearance(window, cx);
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Put on the theme for the side the system is on now. Behind an open
    /// theme list it is what the list hands back to, and the preview stays.
    pub(crate) fn follow_system_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let system = theme::Appearance::of_system(window.appearance());
        let Some(theme) = self.settings.system_theme(system) else {
            return;
        };
        let shown = self.theme_before_preview.unwrap_or(*theme::theme(cx));
        if theme.name == shown.name {
            return;
        }
        if let Some(before) = &mut self.theme_before_preview {
            *before = theme;
            if let Some(palette) = self.palette.clone() {
                palette.update(cx, |palette, cx| palette.set_unpreviewed(theme, window, cx));
            }
        } else {
            // A percent half-typed into Settings for the theme going away is
            // dropped: committing it could save the `5` of a `50` onto that
            // theme, and leaving it would let the next blur save it onto this.
            self.switch_theme(theme, window, cx);
            cx.refresh_windows();
        }
    }

    /// The theme on screen, or the one a preview will put back, worn again as
    /// [`theme::user::reload`] last read it. A theme whose file went away stays
    /// on, and nothing is saved: the pick is still the same name. Behind an
    /// open theme list the row being previewed is read again too, since it may
    /// be the theme that changed.
    pub(crate) fn reinstall_user_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.theme_before_preview.unwrap_or(*theme::theme(cx));
        let fresh = theme::user::current(shown).with_opacity(shown.opacity);
        if let Some(before) = &mut self.theme_before_preview {
            *before = fresh;
            if let Some(palette) = self.palette.clone() {
                palette.update(cx, |palette, cx| palette.set_unpreviewed(fresh, window, cx));
            }
        } else if fresh != shown {
            // Installed rather than switched to, as a re-import is: switching
            // would drop an opacity percent half-typed into Settings.
            install_theme(fresh, window, cx);
            cx.refresh_windows();
        }
    }

    /// The theme is written into dbdelve's own directory first, so picking it
    /// is the same pick a theme already listed would be.
    pub(crate) fn import_theme(
        &mut self,
        source: theme::user::Source,
        index: usize,
        side: Option<theme::Appearance>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match theme::user::import(source, index) {
            Ok(theme) => {
                tracing::info!(source = source.name(), theme = theme.name, "theme imported");
                // `set_theme` skips a theme already picked by name, and a
                // re-import may have brought the one on screen new colours.
                if theme.name == theme::theme(cx).name {
                    install_theme(
                        theme.with_opacity(self.settings.opacity_for(&theme)),
                        window,
                        cx,
                    );
                    cx.refresh_windows();
                }
                self.set_theme(side, theme, window, cx);
            }
            Err(message) => {
                // The message can carry a path through the user's home.
                tracing::warn!(source = source.name(), "theme import failed");
                tracing::debug!("theme import failed: {message}");
                self.note(message, cx);
            }
        }
    }

    /// Written through to disk like the zoom, and reinstalled rather than
    /// merely notified: `install_theme` snapshots the derived colours into
    /// gpui-component's globals, so a repaint alone would leave the library's
    /// half of the window on the old alpha.
    pub(crate) fn set_opacity(
        &mut self,
        opacity: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The field is rewritten before the early return, not after it: typing
        // 120 against a window already at the ceiling changes nothing, and the
        // number we refused would otherwise stay on screen as if it had taken.
        //
        // Both sides are compared in whole percents: a stepped 0.77000004
        // against a typed 0.77 would rewrite `profiles.toml` for nothing.
        let theme = *theme::theme(cx);
        let percent = opacity_percent(opacity.clamp(theme::OPACITY_MIN, theme::OPACITY_MAX));
        self.opacity_input.update(cx, |input, cx| {
            input.set_value(percent.to_string(), window, cx)
        });
        if percent == opacity_percent(self.settings.opacity_for(&theme)) {
            return;
        }
        self.settings.set_opacity_for(&theme, opacity);
        let opacity = self.settings.opacity_for(&theme);
        install_theme(theme.with_opacity(opacity), window, cx);
        self.remember_profiles(cx);
        cx.refresh_windows();
    }

    /// What the typed percentage means, once the user is done typing it. Both
    /// the way out of the field -- enter and blur -- come through here.
    pub(crate) fn commit_opacity_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.opacity_input.read(cx).value();
        let theme = theme::theme(cx);
        let current = self.settings.opacity_for(theme);
        let opacity = opacity_from_percent_input(&typed, current);
        self.set_opacity(opacity, window, cx);
    }

    /// A step is a step from what the field says, not from whatever was last
    /// committed. The click on + does blur the field, but a blur is reported
    /// only once the next frame is drawn and the click can arrive first:
    /// stepping the committed 72 would then land on 77 and drop the 79 typed
    /// on the way.
    pub(crate) fn step_opacity(&mut self, delta: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_opacity_input(window, cx);
        let theme = theme::theme(cx);
        let stepped = adjusted_opacity(theme.opacity, delta);
        self.set_opacity(stepped, window, cx);
    }

    /// Back out of whatever dbdelve has in front of the tab, one layer at a time.
    pub(crate) fn show_editor(
        &mut self,
        _: &ShowEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A menu open from a dropdown or a cell closes on its own `escape`,
        // which this unscoped binding would otherwise beat to it and take
        // the dialog or form under the menu down instead.
        if window
            .context_stack()
            .iter()
            .any(|context| context.contains("PopupMenu"))
        {
            cx.propagate();
            return;
        }
        if self.close_palette(window, cx) {
            return;
        }
        if self.reference_popup.take().is_some() {
            cx.notify();
            return;
        }
        // Ahead of the client import's report, which it is drawn over. Only
        // the question backs out. Once the copy has started the dialog
        // stays, and once it is done it is there to restart.
        if self.stable_import.is_some() {
            self.cancel_stable_import(cx);
            return;
        }
        if self.pending_import.take().is_some() {
            cx.notify();
            return;
        }
        if self.project_name.is_some() {
            self.drop_project_name();
            cx.notify();
            return;
        }
        // Drawn over the form too, and backed out of before the form is.
        if self.form.is_some() && self.render_quit(cx).is_some() {
            self.cancel_quit(cx);
            return;
        }
        // Drawn over the connection form, so it closes before the form does,
        // and before the project naming drawn in it.
        if self.form.is_some() && self.close_agent_audit(cx) {
            return;
        }
        // Backs out of naming a new project to the list, as its × does,
        // rather than out of everything typed into the form.
        if let Some(form) = &mut self.form
            && form.naming_project
        {
            form.naming_project = false;
            form.needs_focus = Some(form.name.clone());
            cx.notify();
            return;
        }
        if self.form.is_some() {
            self.form = None;
            self.refocus_front();
            cx.notify();
            return;
        }
        // Whatever is in front, in the order it is stacked: the palette paints
        // over the stale-edit prompt, the stopped-run prompt and the run
        // prompt, which paint over the export question, which paints over the
        // auto-commit prompt, which paints over the schema export, which
        // paints over the discard-close confirmation, which paints over the
        // delete confirmation, which paints over the import dialog, which
        // paints over the apply review, which paints over the surface and the
        // settings panel beside it -- so `escape` backs out of them in that
        // order, one at a time. The palette stays first so that picking a
        // font from inside settings closes the font list and leaves the panel
        // it was opened from standing. An agent's approval is drawn behind every
        // dialog, so it backs out last, and only denies; the activity dialog is
        // drawn over it, so it goes first of the two.
        if self.cancel_stale_edit(cx) {
            return;
        }
        // Gated on what is drawn, not on the state alone: `queue_failure`
        // outlives the queue it was about when a new run replaces it, and an
        // `escape` meant for something else must not stop that new queue.
        if self.render_queue_failure(cx).is_some() {
            self.stop_queue(cx);
            return;
        }
        if self.render_pending_run(cx).is_some() {
            self.cancel_pending_run(cx);
            return;
        }
        if self.cancel_pending_export(cx) {
            return;
        }
        // Gated on what is drawn, as the queue failure is: a prompt left over
        // from a transaction that ended draws nothing, and must not eat an
        // `escape` meant for something else.
        if self.render_end_transaction(cx).is_some() {
            self.cancel_auto_commit_prompt(cx);
            return;
        }
        if self.render_quit(cx).is_some() {
            self.cancel_quit(cx);
            return;
        }
        if self.cancel_forget_file(cx) {
            return;
        }
        if self.cancel_overwrite(cx) {
            return;
        }
        if self.cancel_schema_export(cx) {
            return;
        }
        if self.cancel_discard_close(cx) {
            return;
        }
        if self.cancel_agent_visibility(cx) {
            return;
        }
        if self.cancel_delete_query(cx) {
            return;
        }
        if self.cancel_query_rename(cx) {
            return;
        }
        if self.close_import(cx) {
            return;
        }
        // A Row view's dirty row is put back before the key is handed on.
        if self.close_apply_review(cx) || self.revert_active_row(window, cx) {
            return;
        }
        if self.close_role_form(cx) {
            return;
        }
        if self.close_grants_view(cx) {
            return;
        }
        if self.close_ddl_view(cx) {
            return;
        }
        if self.close_filter_picker(cx) {
            return;
        }
        if self.close_agent_audit(cx) {
            return;
        }
        if self.deny_front_approval(cx) {
            return;
        }
        // Only from inside the panel: it sits beside the workspace rather than
        // over it, and an `escape` in the editor is for the editor's
        // completions, or for the new-row form beside the grid. The search is
        // emptied before the panel closes.
        if self.settings_open && self.settings_focus.contains_focused(window, cx) {
            // The default row limit's Custom… field is put away alone, as a
            // click elsewhere would.
            if matches!(self.custom_limit, Some((LimitTarget::Default, _))) {
                self.custom_limit = None;
                self.settings_focus.focus(window, cx);
                cx.notify();
            } else if !self.clear_settings_search(window, cx) {
                self.close_settings(window, cx);
            }
            return;
        }
        // Confirming a row clears the form in the same step that opens the
        // review, so the two are never both open on one tab. `close_new_row`
        // is scoped to the active tab's own form, so one left open elsewhere
        // falls through to the steps below instead of eating this keystroke.
        if self.close_new_row(cx) {
            return;
        }
        // The tree has nothing to back out of, so `escape` there leaves it for
        // the surface in front, as leaving any field does.
        if self.explorer_focus.contains_focused(window, cx) {
            self.refocus_front();
            cx.notify();
            return;
        }
        // From the rows back to the statement above them. The grid itself, not
        // a cell's open input, which is a descendant of it and whose `escape`
        // cancels the edit.
        if let Some(session) = self.profile().map(|profile| &profile.session)
            && let Some(editor) = session.editor(session.active)
            && session
                .active_results()
                .is_some_and(|grid| grid.focus_handle(cx).is_focused(window))
        {
            editor.focus_handle(cx).focus(window, cx);
            cx.notify();
            return;
        }
        // The find field first, then the focus: each `escape` puts one away.
        if let Some(ObjectBody::Diagram { view, .. }) = self
            .profile()
            .and_then(|profile| profile.session.active_object())
            .map(|tab| &tab.body)
        {
            let (finding, focused) = (view.read(cx).finding(), view.read(cx).has_focus());
            if finding || focused {
                let view = view.clone();
                view.update(cx, |view, cx| match finding {
                    true => view.close_find(window, cx),
                    false => view.clear_focus(cx),
                });
                return;
            }
        }
        // Nothing of dbdelve's is stacked over the tab, so the keystroke is not
        // ours. Handing it on is what lets the completion popup close and a
        // grid cell's input cancel its edit; this binding is unscoped and would
        // otherwise win it at every depth. Never a tab switch, on any tab: that
        // is what used to take the `escape` meant for a cell edit.
        cx.propagate();
    }

    /// Between the buffer and the rows it ran: to whichever the keyboard is
    /// not in. On an object tab, which has no buffer, to its grid or canvas.
    pub(crate) fn toggle_results_focus(
        &mut self,
        _: &ToggleResultsFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() || self.dialog_in_front.get().is_some() || self.form.is_some() {
            return;
        }
        let Some(session) = self.profile().map(|profile| &profile.session) else {
            return;
        };
        let editor = session.editor(session.active);
        let in_rows = session
            .active_results()
            .is_some_and(|grid| grid.focus_handle(cx).contains_focused(window, cx));
        let target = match (in_rows, editor) {
            (true, Some(editor)) => Focus::Buffer(editor),
            (_, editor) => match (self.front_grid(cx), editor) {
                (Some(grid), _) => grid,
                (None, Some(editor)) => Focus::Buffer(editor),
                (None, None) => return,
            },
        };
        match target {
            Focus::Grid(grid) => grid.update(cx, |table, cx| {
                crate::result_grid::take_keyboard(table, window, cx)
            }),
            Focus::Structure(grid) => grid.focus_handle(cx).focus(window, cx),
            Focus::Diagram(handle) | Focus::Users(handle) => handle.focus(window, cx),
            Focus::Buffer(editor) => editor.focus_handle(cx).focus(window, cx),
            Focus::Window => return,
        }
        // A hand-back asked for earlier would take the keyboard straight back.
        if let Some(profile) = self.profile_mut() {
            profile.session.editor_needs_focus = false;
        }
        cx.notify();
    }

    /// The grid in front that can take the keyboard: one drawn, with a row for
    /// the ring to sit on. A plan, the run's messages, or the line standing in
    /// for rows that never came, has nothing in it to step through.
    pub(super) fn front_grid(&self, cx: &App) -> Option<Focus> {
        let session = &self.profile()?.session;
        let has_rows = |grid: &Entity<gpui_component::table::TableState<ResultGrid>>| {
            !grid.read(cx).delegate().result().rows.is_empty()
        };
        let Some(tab) = session.active_object() else {
            let tab = session.active_query_tab()?;
            let (state, grid) = tab.shown();
            let drawn = !tab.results_hidden
                && tab.plan_shown().is_none()
                && !tab.messages_shown()
                && matches!(
                    state,
                    QueryState::Running { .. } | QueryState::Complete { .. }
                );
            return (drawn && has_rows(grid)).then(|| Focus::Grid(grid.clone()));
        };
        match &tab.body {
            ObjectBody::Relation { query, results, .. } => {
                (!matches!(query, QueryState::Failed(_)) && has_rows(results))
                    .then(|| Focus::Grid(results.clone()))
            }
            ObjectBody::Structure { grid, .. } => Some(Focus::Structure(grid.clone())),
            ObjectBody::Diagram { view, .. } => {
                Some(Focus::Diagram(view.read(cx).focus_handle().clone()))
            }
            ObjectBody::Routine(_) | ObjectBody::Users { .. } => None,
        }
    }

    /// `cmd+w` on whatever surface is in front. Closing never deletes a saved
    /// query: the sidebar still lists it, and only its own Delete removes it.
    pub(crate) fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        // The palette is over the tab and holds the keyboard: a stroke that
        // reached here through it would close a tab nobody was looking at.
        if self.palette.is_some() {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let session = &profile.session;
        if session.active_query_tab().is_none() && session.active_object().is_none() {
            return;
        }
        let tab = session.active;
        self.ask_before_close(tab, window, cx);
    }

    /// Close every tab in the strip. The ones holding unapplied cell edits are
    /// left for a single question once the rest have gone, and file tabs with
    /// unsaved changes for one Save question after it.
    pub(crate) fn close_all_tabs(
        &mut self,
        _: &CloseAllTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() {
            return;
        }
        // Text the row panel could not stage stays with its row: closing now
        // would either lose it or send the row without it.
        if self.stage_row_inputs(cx).1 {
            return;
        }
        let Some(profile) = self.profile() else {
            return;
        };
        let active = profile.session.active;
        // The tab in front goes last: closing it earlier would bring each
        // neighbour forward in turn, and a tab coming forward reads its
        // snapshot and refreshes its rows only to be closed straight after.
        let mut tabs = profile.session.strip_tabs();
        tabs.sort_by_key(|tab| *tab == active);
        let (dirty, clean): (Vec<Tab>, Vec<Tab>) = tabs
            .into_iter()
            .partition(|tab| self.would_lose_on_close(*tab, cx));
        // A tab whose edits are being sent stays: closing it would cancel the
        // send, leaving whether it was written unknown, and discard the rest.
        let (busy, mut dirty): (Vec<Tab>, Vec<Tab>) = dirty.into_iter().partition(|tab| {
            self.profile()
                .is_some_and(|profile| profile.session.running(*tab))
        });
        if !busy.is_empty() {
            self.note(
                "A tab whose edits are still being sent was left open.".into(),
                cx,
            );
        }
        if dirty.is_empty() {
            self.close_or_ask_to_save(clean, window, cx);
            return;
        }
        self.look_before_closing(&clean, cx);
        let (unsaved, clean): (Vec<Tab>, Vec<Tab>) = clean
            .into_iter()
            .partition(|tab| self.has_unsaved_file(*tab));
        for tab in clean {
            self.close_now(tab, window, cx);
        }
        if let Some(profile) = self.profile_mut() {
            // The tab in front, sorted last among those with edits, is the one
            // the prompt is over; the others go with it, and the unsaved file
            // tabs wait with them for the Save question.
            let tab = dirty.pop().expect("dirty is not empty");
            let mut also = dirty;
            also.extend(unsaved);
            profile.session.pending_discard = Some(PendingDiscard {
                tab,
                then: AfterEdits::Close { deleting: None },
                row: None,
                also,
                unsaved: false,
            });
        }
        cx.notify();
    }

    /// Close `tabs`, but leave the file tabs among them with unsaved changes
    /// for one Save / Don't Save question.
    fn close_or_ask_to_save(
        &mut self,
        tabs: Vec<Tab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.look_before_closing(&tabs, cx);
        let (unsaved, clean): (Vec<Tab>, Vec<Tab>) = tabs
            .into_iter()
            .partition(|tab| self.has_unsaved_file(*tab));
        for tab in clean {
            self.close_now(tab, window, cx);
        }
        if unsaved.is_empty() {
            return;
        }
        if let Some(profile) = self.profile_mut() {
            let mut also = unsaved;
            let tab = also.remove(0);
            profile.session.pending_discard = Some(PendingDiscard {
                tab,
                then: AfterEdits::Close { deleting: None },
                row: None,
                also,
                unsaved: true,
            });
        }
        self.ask_to_save(window, cx);
        cx.notify();
    }

    /// Mark each clean file tab among `tabs` whose file has gone since it was
    /// last looked at: a tab behind another is looked at only when it comes
    /// forward, and its buffer is now the only copy of the text. One stat
    /// each. A file changed rather than gone leaves nothing of the user's
    /// in a clean buffer, so it closes unasked.
    fn look_before_closing(&mut self, tabs: &[Tab], cx: &mut Context<Self>) {
        for tab in tabs {
            let Tab::Query(id) = *tab else {
                continue;
            };
            let gone = self
                .profile()
                .and_then(|profile| profile.session.query_tab(id)?.file.as_ref())
                .is_some_and(|file| {
                    file.saved.is_some() && matches!(crate::sql_file::stamp(&file.path), Ok(None))
                });
            if gone {
                self.mark_gone(id, cx);
            }
        }
    }

    /// A file tab holding text its file does not. An untitled tab is never
    /// asked about.
    pub(crate) fn has_unsaved_file(&self, tab: Tab) -> bool {
        let Tab::Query(id) = tab else {
            return false;
        };
        self.profile()
            .and_then(|profile| profile.session.query_tab(id))
            .is_some_and(|tab| tab.dirty)
    }

    /// Close a tab, asking first if it holds cell edits nobody has applied.
    ///
    /// Every gesture that takes a tab away comes through here -- `cmd+w`, the
    /// chip's own close button, the palette -- because the edits are lost the
    /// same way whichever one it was, and a guard on one path is a guard on
    /// none.
    pub(crate) fn ask_before_close(
        &mut self,
        tab: Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Refused text stops the close: the row is neither sent without it
        // nor discarded with it.
        if self.stage_row_inputs(cx).1 {
            return;
        }
        if !self.held_for_edits(tab, AfterEdits::Close { deleting: None }, cx) {
            self.close_or_ask_to_save(vec![tab], window, cx);
        }
    }

    /// Whether `then` is held back because `tab`'s grid holds edits nobody has
    /// applied; `true` means the caller stops, having changed nothing.
    ///
    /// Asked on every target and engine alike: what a send does differs
    /// between them, but an unapplied edit is replaced by a requery the same
    /// way on all of them. The action waits on `pending_discard` as data, to be
    /// carried out by [`Self::resume_after_edits`] if the user lets the edits
    /// go.
    ///
    /// A Row view's row is one Row mode can save, so `then` goes with the row's
    /// send (`save_row`) and is carried out, from render, only when that send
    /// lands ([`Self::resume_released`]). A send that fails, or is stopped by the
    /// mode, drops it and puts back what it had changed. The user repeats the
    /// action, and a row the server has refused (it stays marked until it is
    /// edited, reverted or sent) is not sent again: the prompt asks whether to
    /// drop that row, and Discard drops it alone and carries the action out.
    /// A Draft view is asked about every edit.
    pub(crate) fn held_for_edits(
        &mut self,
        tab: Tab,
        then: AfterEdits,
        cx: &mut Context<Self>,
    ) -> bool {
        // Text not yet staged is lost only by a close: any other action keeps
        // the inputs, and the row panel stages its text when it is left.
        let held = match then {
            AfterEdits::Close { .. } => self.would_lose_on_close(tab, cx),
            _ => self.has_unapplied_edits(tab, cx),
        };
        if !held {
            return false;
        }
        if let Tab::Object(id) = tab
            && let Some(profile) = self.profile()
            && profile.session.edit_mode(tab) == EditMode::Row
            && let Some(row) = profile
                .session
                .results(tab)
                .and_then(|results| results.read(cx).delegate().row_to_send())
        {
            let refused = profile.session.results(tab).is_some_and(|results| {
                results
                    .read(cx)
                    .delegate()
                    .send_failure()
                    .is_some_and(|(marked, _)| marked == row)
            });
            if profile.session.running(tab) {
                self.note("The previous row is still being saved.".into(), cx);
                self.put_back(tab, then);
            } else if refused {
                // The server has already refused this row as it stands (an
                // edit clears the mark), so sending it again would only hear
                // the same answer. The user is asked whether to drop it.
                if let Some(profile) = self.profile_mut() {
                    profile.session.pending_discard = Some(PendingDiscard {
                        tab,
                        then,
                        row: Some(row),
                        also: Vec::new(),
                        unsaved: false,
                    });
                }
                cx.notify();
            } else if !self.save_row(id, row, None, Some(then.clone()), cx) {
                // `save_row` said why it sent nothing.
                self.put_back(tab, then);
            }
            return true;
        }
        // Edits in flight are neither the user's to discard nor ours to wait
        // on: the send decides what becomes of them, and a prompt over them
        // would answer before it has.
        if !matches!(then, AfterEdits::Close { .. })
            && self
                .profile()
                .is_some_and(|profile| profile.session.running(tab))
        {
            self.note("This tab's edits are still being sent.".into(), cx);
            // What the action changed before it asked, a filter bar, goes back:
            // the action is not going ahead.
            self.put_back(tab, then);
            return true;
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.pending_discard = Some(PendingDiscard {
                tab,
                then,
                row: None,
                also: Vec::new(),
                unsaved: false,
            });
        }
        cx.notify();
        true
    }

    /// Undoes what an action that is not going ahead changed before it asked:
    /// a filter bar the user edited goes back to what it was.
    pub(crate) fn put_back(&mut self, tab: Tab, then: AfterEdits) {
        if let Some(profile) = self.profile_mut() {
            profile.session.put_back(tab, then);
        }
    }

    /// Carries out the action a landed row send released, from render for the
    /// `Window` its handler needs (`restore_objects` is the precedent). Waits
    /// while the tab is still reading, since a requery over a read in flight
    /// would be dropped; drops the action when its tab has gone. With rows still
    /// edited, the handler meets [`Self::held_for_edits`] again and the next row
    /// is sent.
    pub(crate) fn resume_released(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.profile() else {
            return;
        };
        let Some((tab, _)) = &profile.session.resume_after else {
            return;
        };
        if profile.session.running(*tab) {
            return;
        }
        let open = profile.session.results(*tab).is_some();
        let Some((tab, then)) = self
            .profile_mut()
            .and_then(|profile| profile.session.resume_after.take())
        else {
            return;
        };
        if open {
            self.resume_after_edits(tab, then, window, cx);
        } else {
            self.put_back(tab, then);
        }
    }

    /// Whether `tab`'s grids hold staged edits nobody has applied.
    pub(crate) fn has_unapplied_edits(&self, tab: Tab, cx: &App) -> bool {
        self.profile().is_some_and(|profile| {
            profile
                .session
                .tab_grids(tab)
                .iter()
                .any(|grid| grid.read(cx).delegate().has_pending())
        })
    }

    /// Staged edits, and drafts that never became one: an open cell input,
    /// or a row panel field whose staging was refused. A close would lose
    /// either without a word.
    pub(crate) fn would_lose_on_close(&self, tab: Tab, cx: &App) -> bool {
        let Some(profile) = self.profile() else {
            return false;
        };
        let drafts = profile
            .session
            .tab_grids(tab)
            .iter()
            .any(|grid| grid.read(cx).delegate().has_draft(cx));
        let row_draft = self.row_panel.inputs.as_ref().is_some_and(|inputs| {
            inputs.tab == tab
                && inputs.profile == profile.id
                && inputs
                    .fields
                    .iter()
                    .flatten()
                    .any(|field| field.input.read(cx).value().as_str() != field.seed.as_str())
        });
        self.has_unapplied_edits(tab, cx) || drafts || row_draft
    }

    /// Carry out a close that has been decided on.
    pub(crate) fn close_now(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        match tab {
            Tab::Object(id) => self.close_object(id, window, cx),
            Tab::Query(id) => self.close_buffer(id, window, cx),
        }
    }

    /// Ask the server to stop what `tab` is running, if it is running something.
    ///
    /// For a tab on its way out: left running, its statement holds the
    /// connection while nothing is left to show it.
    pub(crate) fn stop_run(&mut self, tab: Tab, cx: &mut Context<Self>) {
        // The session the tab runs on, as `cancel_query` stops it: closing a
        // bound tab stops its statement where it is running.
        let Some(connection) = self
            .profile()
            .and_then(|profile| profile.connection_to_stop(tab))
        else {
            return;
        };
        let Some(profile) = self.profile_mut() else {
            return;
        };
        let Some((QueryState::Running { cancel, .. }, _)) = profile.session.slot(tab) else {
            return;
        };
        let mut cancels = vec![cancel.clone()];
        // A queue's statements running at once are each stopped by their own.
        if let Tab::Query(id) = tab
            && let Some(queue) = profile
                .session
                .query_tab(id)
                .and_then(|query| query.queue.as_ref())
        {
            cancels.extend(
                queue
                    .done
                    .iter()
                    .filter_map(|finished| match &finished.state {
                        QueryState::Running { cancel, .. } => Some(cancel.clone()),
                        _ => None,
                    }),
            );
        }
        let cancel_task = spawn_cancel(cx.background_executor(), move || {
            cancels
                .iter()
                .map(|cancel| connection.cancel(cancel))
                .fold(Ok(()), Result::and)
        });
        // Said as `cancel_query` says it: the tab has gone, so a statement
        // still running behind it is all the more worth knowing about.
        cx.spawn(async move |workspace, cx| {
            if let Err(error) = cancel_task.await {
                _ = workspace.update(cx, |workspace, cx| workspace.note(error.message, cx));
            }
        })
        .detach();
    }

    /// Drop the edits of the tab the discard prompt was raised over, and do
    /// what it was raised for, or, over unsaved files, Don't Save. Discarded
    /// edits still leave a file tab's unsaved text to ask about.
    pub(crate) fn confirm_discard_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(discard) = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_discard.take())
        else {
            return;
        };
        if discard.unsaved {
            for tab in discard.tabs().collect::<Vec<_>>() {
                self.close_now(tab, window, cx);
            }
            self.refocus_unless_asking();
            cx.notify();
            return;
        }
        // The grids the question read, every one of them: a Discard that left
        // one holding edits would put the action back in front of the prompt.
        let grids = self
            .profile()
            .map(|profile| {
                std::iter::once(discard.tab)
                    .chain(discard.also.iter().copied())
                    .flat_map(|tab| profile.session.tab_grids(tab))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for grid in grids {
            grid.update(cx, |table, cx| {
                match discard.row {
                    // The one row the server refused: another row's edits are
                    // not the user's to lose here, and the action sends them.
                    Some(row) => table.delegate_mut().discard_row(row),
                    None => table.delegate_mut().discard_pending(),
                }
                table.refresh(cx);
            });
        }
        if let Some(profile) = self.profile_mut() {
            profile.session.drop_apply_review_of(discard.tab);
            for tab in &discard.also {
                profile.session.drop_apply_review_of(*tab);
            }
        }
        // The row panel's text goes with the edits: left in its inputs, it
        // would raise the same question again when the action is carried out.
        let profile_id = self.profile().map(|profile| profile.id.clone());
        if self.row_panel.inputs.as_ref().is_some_and(|inputs| {
            Some(&inputs.profile) == profile_id.as_ref()
                && (inputs.tab == discard.tab || discard.also.contains(&inputs.tab))
        }) {
            self.row_panel.inputs = None;
        }
        if discard.also.is_empty() {
            self.resume_after_edits(discard.tab, discard.then, window, cx);
        } else {
            // Close All Tabs: the file tabs it held with the edits are asked
            // about in one Save question, with any whose edits just went.
            let mut tabs = discard.also;
            tabs.push(discard.tab);
            self.close_or_ask_to_save(tabs, window, cx);
        }
        self.refocus_unless_asking();
        cx.notify();
    }

    /// Save in the close question: each file written closes, and one that
    /// cannot be written stays open, saying why.
    pub(crate) fn save_and_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(discard) = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_discard.take())
        else {
            return;
        };
        let ids = discard
            .tabs()
            .filter_map(|tab| match tab {
                Tab::Query(id) => Some(id),
                Tab::Object(_) => None,
            })
            .collect();
        self.save_in_place(ids, true, window, cx);
        self.refocus_unless_asking();
    }

    /// Focus back on the tab in front, unless what was just answered raised
    /// another question, which holds the keyboard itself.
    fn refocus_unless_asking(&mut self) {
        let asking = self.profile().is_some_and(|profile| {
            profile.session.pending_discard.is_some() || profile.session.pending_overwrite.is_some()
        });
        if !asking {
            self.refocus_front();
        }
    }

    /// Carry out what was held back for `tab`'s edits, which are gone. Each
    /// arm calls the handler the guard stands in, which passes it this time.
    /// Anything but a close does nothing when `tab` is not the tab in front: a
    /// prompt never acts on another tab.
    pub(crate) fn resume_after_edits(
        &mut self,
        tab: Tab,
        then: AfterEdits,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let in_front = self.profile().map(|profile| profile.session.active) == Some(tab);
        match then {
            AfterEdits::Close {
                deleting: Some((owner, name)),
            } => self.delete_saved_query_now(owner, name, window, cx),
            // Asked again rather than closed outright: the rows a Row view
            // still holds are sent first.
            AfterEdits::Close { deleting: None } => self.ask_before_close(tab, window, cx),
            _ if !in_front => self.put_back(tab, then),
            AfterEdits::Requery {
                filter,
                sort,
                limit,
                offset,
            } => {
                if let Tab::Object(id) = tab {
                    self.requery_relation(
                        id,
                        move |tab_filter, tab_sort, tab_limit, tab_offset| {
                            *tab_filter = filter;
                            *tab_sort = sort;
                            *tab_limit = limit;
                            *tab_offset = offset;
                            true
                        },
                        cx,
                    );
                }
            }
            AfterEdits::Refresh => {
                if let Tab::Object(id) = tab {
                    self.refresh_relation(id, cx);
                }
            }
            AfterEdits::Filter { .. } => {
                if let Tab::Object(id) = tab {
                    self.apply_filter(id, None, cx);
                }
            }
            AfterEdits::ViewSorting(client) => self.set_view_sorting(client, window, cx),
            AfterEdits::Run => self.run_query(&RunQuery, window, cx),
            AfterEdits::Sort(column) => self.sort_column(&SortColumn { column }, window, cx),
            AfterEdits::Rerun { index, sql } => {
                self.rerun_queued(tab, index, sql, cx);
            }
            AfterEdits::Activate(next) => self.activate_tab(next, window, cx),
            AfterEdits::EditMode(mode) => self.set_edit_mode(mode, cx),
        }
    }

    /// Put the prompt away. A changed filter bar goes back to what it was: the
    /// bars were changed before anything could ask, and are the state the tab
    /// saves and restores from.
    pub(crate) fn cancel_discard_close(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(discard) = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_discard.take())
        else {
            return false;
        };
        self.put_back(discard.tab, discard.then);
        self.refocus_front();
        cx.notify();
        true
    }

    pub(crate) fn cancel_delete_query(&mut self, cx: &mut Context<Self>) -> bool {
        let cancelled = self
            .profile_mut()
            .and_then(|profile| profile.session.pending_delete.take())
            .is_some();
        if cancelled {
            self.refocus_front();
            cx.notify();
        }
        cancelled
    }
}

/// Hand the keyboard back to the tab in front on the next frame, for an input
/// taken away while it had it: an unmounted input keeps the window's focus,
/// and with it every keybinding.
fn refocus_front_later(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.defer_in(window, |workspace, _, cx| {
        workspace.refocus_front();
        cx.notify();
    });
}
