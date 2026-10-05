//! The main pane: what the tab in front is showing.
//!
//! Every function here was a `Workspace` associated function that never touched
//! `self` — a pure function of the profile it draws and the theme it reads. They
//! moved out whole; nothing changed but the indentation.
//!
//! `render_main_content` is the only way in. Everything else is a part of the
//! surface it assembles, which is why the rest of the module is private.

use gpui::{
    Animation, AnimationExt, AnyElement, AppContext, ClickEvent, Context, Div, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable, ElementExt, IconName, Sizable,
    button::Button,
    input::{self, Editor, EditorState, Input},
    menu::DropdownMenu,
    resizable::{ResizableState, h_resizable, resizable_panel, v_resizable},
    spinner::Spinner,
    table::{DataTable, TableDelegate, TableState},
};

use crate::{
    InsertForm, TabKey, Workspace,
    actions::{
        AddFilter, CancelQuery, ExplainQuery, FormatQuery, NewQuery, NewRow, NextPage,
        PreviousPage, RemoveFilter, ResetEditorZoom, RunQuery, SaveQuery, SetFilterColumn,
        SetFilterOperator, SetFilterRaw, SetRowLimit, ToggleFilterJoin, ToggleNextJoin,
        ToggleRowPanel, ZoomEditorIn, ZoomEditorOut,
    },
    db,
    db::{Engine, ExplainMode, RoutineKind},
    explorer::ROW_LIMITS,
    filter::{Conjunction, FilterRow, Operator},
    icons::icon,
    keybindings,
    palette::{Command, Mode as PaletteMode},
    result_grid,
    result_grid::ResultGrid,
    scroller::{SmoothScrollable, smooth, smooth_for, smooth_scoped},
    session::{
        CloseTarget, Explained, ObjectBody, ObjectTab, Profile, ProfileState, QueryState, QueryTab,
        StructureState, Tab, query_label, result_pane_is_expanded,
    },
    tab_drag::{DragTab, TabStrip},
    theme::{
        FontSlot, OPACITY_DEFAULT, OPACITY_MAX, OPACITY_MIN, OPACITY_STEP, Theme, fonts, layout,
        theme,
    },
    ui::{
        Control, Tone, button, button_label, compact_count, dialog, group_thousands, icon_button,
        key_hint, keycap_for, keycap_text, kind_color, object_icon, reconnect_button, row_icon,
        section_label,
    },
    workspace::{
        EDITOR_FONT_SIZE_MAX, EDITOR_FONT_SIZE_MIN, SettingsTab, editor_zoom_percent,
        error_in_buffer,
    },
};

/// What the row panel needs that is not part of any one tab: whether it is
/// on screen right now, and what was just copied. The fold and the split's
/// width live on the tab instead (`QueryTab`/`ObjectBody::Relation`), in
/// memory only, so a panel folded or resized away in one tab does not touch
/// another -- and neither survives a restart.
pub struct RowPanel {
    /// Whether the last frame drew the panel, folded or not. Cleared at the
    /// top of every frame and set only where the panel is built, so the toggle
    /// can ignore a fold aimed at a panel that is not there to take it.
    pub on_screen: std::cell::Cell<bool>,
    /// The (row, column) whose value was just copied, so its button can show
    /// a tick until the timer in `copy_row_field` clears it.
    pub copied: Option<(usize, usize)>,
}

pub fn render_main_content(
    profile: &Profile,
    editor_font_size: f32,
    row_panel: &RowPanel,
    plan_copied: bool,
    strip: &TabStrip,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let body = match profile.session.active_object() {
        Some(tab) => render_object(
            tab,
            &profile.id,
            profile.config.engine(),
            matches!(profile.state, ProfileState::Failed(_)),
            row_panel,
            profile
                .session
                .insert_form
                .as_ref()
                .filter(|form| form.tab == profile.session.active),
            cx,
        ),
        None => render_query_surface(profile, editor_font_size, row_panel, plan_copied, cx),
    };

    div()
        .size_full()
        .flex()
        .flex_col()
        // Chrome, so the strip reads as the frame the surfaces sit in --
        // and chrome is the frost, which is already painted beneath it.
        .child(render_tab_strip(profile, editor_font_size, strip, cx))
        .child(div().flex_1().min_h_0().child(body))
        .into_any_element()
}

/// The editor over the rows it produces. Every runnable surface is this:
/// the query buffer and an opened relation differ in where their SQL came
/// from, not in what they are.
fn render_editor_surface(
    split: gpui::ElementId,
    editor: &Entity<EditorState>,
    font_size: f32,
    query: &QueryState,
    bottom: AnyElement,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();

    // The editor is the prompt, one tone behind its results -- and one step
    // more transparent, since it is also one step further from the data.
    let top = div()
        .key_context("Editor")
        .size_full()
        .bg(t.panel_glass())
        .p(px(layout::SPACE_LG))
        .font_family(code)
        .child(
            Editor::new(editor)
                .h_full()
                .appearance(false)
                .bordered(false)
                .text_size(px(font_size))
                .line_height(px(font_size * 1.55))
                // The builder replaces the built-in menu rather than extending
                // it, so the edit items are restated to keep them. Gone with the
                // default are Go to Definition and Show Code Actions, which this
                // editor drew permanently greyed -- dbdelve registers neither
                // provider, and no language server is coming.
                .context_menu(|menu, _, cx| {
                    menu.menu("Cut", Box::new(input::Cut))
                        .menu("Copy", Box::new(input::Copy))
                        .menu_with_disabled(
                            "Paste",
                            cx.read_from_clipboard().is_none(),
                            Box::new(input::Paste),
                        )
                        .separator()
                        .menu("Select All", Box::new(input::SelectAll))
                        .separator()
                        .menu("Format Query", Box::new(FormatQuery))
                }),
        );

    let expanded = result_pane_is_expanded(query);
    let (editor_height, results_height) = if expanded {
        (
            layout::EDITOR_DEFAULT_HEIGHT,
            layout::RESULTS_DEFAULT_HEIGHT,
        )
    } else {
        (layout::EDITOR_EMPTY_HEIGHT, layout::RESULTS_EMPTY_HEIGHT)
    };

    v_resizable((split, if expanded { "expanded" } else { "compact" }))
        .child(
            resizable_panel()
                .size(px(editor_height))
                .size_range(px(layout::EDITOR_MIN_HEIGHT)..px(layout::EDITOR_MAX_HEIGHT))
                .child(top),
        )
        .child(
            resizable_panel()
                .size(px(results_height))
                .size_range(px(layout::RESULTS_MIN_HEIGHT)..gpui::Pixels::MAX)
                .child(bottom),
        )
        .into_any_element()
}

fn render_query_surface(
    profile: &Profile,
    editor_font_size: f32,
    row_panel: &RowPanel,
    plan_copied: bool,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let Some(tab) = profile.session.active_query_tab() else {
        let t = *theme(cx);
        return div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(layout::TEXT_SM))
            .text_color(t.text_faint)
            .child("Open a table from the sidebar, or start a new query.")
            .into_any_element();
    };
    // Scopes this tab's scroll state (plan, grid, row panel) to its own id, so
    // another tab reusing the same bare id -- next_query_id and
    // next_object_id each count from zero per profile -- doesn't inherit it.
    let scope = Tab::Query(tab.id).scroll_scope(&profile.id);
    // The plan stands in for the rows rather than beside them: the pane is one
    // answer about the buffer above it, and two scrolling regions in a split
    // that is already a split leaves neither enough room to read.
    let (shown_state, shown_grid) = tab.shown();
    let bottom = match tab.showing_plan.then_some(tab.plan.as_ref()).flatten() {
        Some(explained) => render_plan(explained, plan_copied, &scope, cx),
        None => render_results(
            profile.config.engine(),
            matches!(profile.state, ProfileState::Failed(_)),
            shown_state,
            shown_grid,
            Some(tab),
            tab.row_panel_folded,
            &tab.row_panel_split,
            row_panel,
            None,
            &scope,
            cx,
        ),
    };
    render_editor_surface(
        // Keyed by the buffer rather than the profile: two query tabs are two
        // splits, and sharing one id would carry the first one's drag position
        // onto the second.
        gpui::ElementId::from((
            gpui::ElementId::from("query-result-split"),
            gpui::SharedString::from(format!("{}-{}", profile.id, tab.id)),
        )),
        &tab.editor,
        editor_font_size,
        &tab.query,
        bottom,
        cx,
    )
}

/// How much of a node's label the plan pane will draw before it clips. The
/// label is the operator and its target, and a long one is a long list of
/// output columns that would push the numbers off the right edge.
const PLAN_LABEL_LIMIT: usize = 160;

/// A query plan, as a tree of what the server said it would do.
///
/// Read in two directions at once: down the indentation to see the shape of the
/// plan, and across the bars to see where the time went. So the bar is the one
/// thing aligned in a column of its own -- a reader looking for the slow node
/// scans one edge rather than comparing numbers inside sentences.
///
/// Rows rather than the grid, because a plan is a tree and a tree in a grid is
/// a column of pre-indented strings: sortable-looking, movable, resizable, and
/// wrong in every one of those. `sql::clause_anchor` refuses to sort an
/// explained statement for the same reason.
fn render_plan(
    explained: &Explained,
    plan_copied: bool,
    scope: &str,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();
    let Explained { plan, mode, sql } = explained;
    // What every bar is a share of. A plan with no timings draws none, and the
    // guard against zero is what keeps a 0ms plan from dividing by it.
    let total = plan.total_ms.filter(|total| *total > 0.0);
    // The slowest node earns the one warm colour in the pane. Scanning for it
    // is the reason most people open a plan at all.
    let slowest = total.and_then(|_| {
        plan.nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| node.self_ms.map(|ms| (index, ms)))
            .filter(|(_, ms)| *ms > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(index, _)| index)
    });

    let metric = |label: &'static str, value: String, tone: gpui::Hsla| {
        div()
            .flex()
            .gap(px(layout::SPACE_XS))
            .child(div().text_color(t.text_faint).child(label))
            .child(div().text_color(tone).child(value))
    };

    let rows = plan.nodes.iter().enumerate().map(|(index, node)| {
        let hottest = slowest == Some(index);
        let share = match (node.self_ms, total) {
            (Some(ms), Some(total)) => (ms / total).clamp(0.0, 1.0),
            _ => 0.0,
        };

        div()
            .flex()
            .items_start()
            .gap(px(layout::SPACE_MD))
            .px(px(layout::SPACE_SM))
            .py(px(layout::SPACE_XS))
            .rounded(px(layout::RADIUS_CONTROL))
            .when(hottest, |row| row.bg(t.element_hover))
            // The bar column is fixed and leads the row, so every bar starts at
            // the same x and the longest one is found by looking down an edge
            // rather than by reading. It is drawn only where the plan carried
            // timings at all -- SQLite reports none, and an empty track on every
            // row of its plans is a column of nothing to read past.
            .children(total.map(|_| {
                div()
                    .w(px(72.))
                    .min_w(px(72.))
                    .flex_shrink_0()
                    .pt(px(4.))
                    .child(
                        div()
                            .w_full()
                            .h(px(6.))
                            .rounded(px(3.))
                            .bg(t.element_active)
                            .child(
                                div()
                                    .h_full()
                                    .rounded(px(3.))
                                    .w(gpui::relative(share as f32))
                                    .bg(match hottest {
                                        true => t.danger,
                                        false => t.accent,
                                    }),
                            ),
                    )
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    // The tree's shape, paid for in indentation rather than in
                    // drawn rules: a rule per level is a lot of ink for a depth
                    // that is usually three.
                    .pl(px(node.depth as f32 * 14.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(match hottest {
                                true => t.danger,
                                false => t.text,
                            })
                            .child(clip_label(&node.label)),
                    )
                    .children(node.detail.iter().map(|line| {
                        div()
                            .text_size(px(layout::TEXT_XS))
                            .text_color(t.text_muted)
                            .child(clip_label(line))
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(layout::SPACE_MD))
                            .text_size(px(layout::TEXT_XS))
                            // The estimate and the measurement are deliberately
                            // side by side and differently coloured: the gap
                            // between what the planner expected and what it got
                            // is the thing a plan is usually read to find.
                            .children(node.actual.map(|actual| {
                                metric(
                                    "actual",
                                    format!(
                                        "{:.3} ms · {} rows · {} loops",
                                        actual.total_ms,
                                        round_count(actual.rows),
                                        round_count(actual.loops)
                                    ),
                                    t.success.into(),
                                )
                            }))
                            .children(node.estimated.map(|estimated| {
                                metric(
                                    "est",
                                    format!(
                                        "cost {:.2} · {} rows",
                                        estimated.total_cost,
                                        group_thousands(estimated.rows)
                                    ),
                                    t.syntax_number.into(),
                                )
                            }))
                            .children(node.self_ms.filter(|_| total.is_some()).map(|ms| {
                                metric("self", format!("{ms:.3} ms"), t.text_muted.into())
                            })),
                    ),
            )
    });

    div()
        .id("plan")
        .size_full()
        .min_h_0()
        .flex()
        .flex_col()
        .font_family(code)
        .text_size(px(layout::TEXT_SM))
        // The header says what was asked and of what, because a plan read an
        // hour later is otherwise a page of numbers about nothing in
        // particular -- and because `Analyze` means the statement was run.
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .px(px(layout::SPACE_LG))
                .h(px(layout::TAB_HEIGHT))
                .border_b_1()
                .border_color(t.border)
                .child(
                    icon(icon::PLAN)
                        .size(px(layout::ICON_SIZE))
                        .text_color(t.text_faint),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(FontWeight::MEDIUM)
                        .child(mode.label()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(t.text_muted)
                        .child(one_line(sql)),
                )
                .children(plan.summary.iter().map(|(label, value)| {
                    div()
                        .flex_shrink_0()
                        .flex()
                        .gap(px(layout::SPACE_XS))
                        .text_size(px(layout::TEXT_XS))
                        .child(div().text_color(t.text_faint).child(label.clone()))
                        .child(div().text_color(t.text).child(value.clone()))
                }))
                .child(match plan_copied {
                    true => div()
                        .flex_shrink_0()
                        .size(px(Control::Compact.height()))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            icon(icon::CHECK)
                                .size(px(layout::ICON_SIZE))
                                .text_color(t.success),
                        )
                        .with_animation(
                            "copied-plan",
                            Animation::new(std::time::Duration::from_millis(150)),
                            |tick, delta| tick.opacity(delta),
                        )
                        .into_any_element(),
                    false => icon_button("copy-plan", icon::COPY, Tone::Quiet, Control::Compact, t)
                        .tooltip("Copy plan")
                        .on_click(cx.listener(|workspace, _, _, cx| workspace.copy_plan(cx)))
                        .into_any_element(),
                }),
        )
        .child(
            div()
                .id("plan-nodes")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .smooth_scroll(&smooth_scoped("plan-nodes", scope, cx))
                .p(px(layout::SPACE_SM))
                .flex()
                .flex_col()
                .gap(px(2.))
                // A server that answered with nothing a plan could be read out
                // of still said something, and its own words are better than
                // dbdelve's guess at what it meant.
                .when(plan.nodes.is_empty(), |body| {
                    body.child(
                        div()
                            .p(px(layout::SPACE_MD))
                            .text_color(t.text_muted)
                            .child(plan.text.clone()),
                    )
                })
                .children(rows),
        )
        .into_any_element()
}

/// A plan line, bounded. The server will happily print every output column of a
/// wide projection onto one line, and a row that wide pushes the numbers beside
/// it off the pane.
fn clip_label(label: &str) -> String {
    match label.char_indices().nth(PLAN_LABEL_LIMIT) {
        Some((at, _)) => format!("{}…", &label[..at]),
        None => label.to_string(),
    }
}

/// A statement on one line, for the header strip that says what was explained.
fn one_line(sql: &str) -> String {
    clip_label(&sql.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// A count the server reported as a fraction, because it averaged it over the
/// loops. The fraction is real and is worth keeping when it is there.
fn round_count(value: f64) -> String {
    match value.fract() == 0.0 {
        true => group_thousands(value as u64),
        false => format!("{value:.2}"),
    }
}

/// An opened object. A relation's generated `SELECT` is an ordinary buffer
/// the user can edit and run; only a routine, which has nothing to run, is
/// read-only.
fn render_object(
    tab: &ObjectTab,
    profile_id: &str,
    engine: Engine,
    disconnected: bool,
    row_panel: &RowPanel,
    form: Option<&InsertForm>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    // Scopes this tab's scroll state to its own id -- see the same line in
    // `render_query_surface`.
    let scope = Tab::Object(tab.id).scroll_scope(profile_id);
    let ObjectBody::Relation {
        showing_structure,
        structure,
        results,
        query,
        filters,
        next_join,
        row_panel_folded,
        row_panel_split,
        ..
    } = &tab.body
    else {
        return render_routine(tab, &scope, cx);
    };

    if *showing_structure {
        return div()
            .size_full()
            .min_h_0()
            .bg(t.data_glass())
            .child(render_structure(structure, &scope, cx))
            .into_any_element();
    }

    div()
        .size_full()
        .flex()
        .flex_col()
        .child(render_filter_bar(
            filters,
            results.read(cx).delegate().columns(),
            engine,
            *next_join,
            t,
        ))
        .child(div().flex_1().min_h_0().child(render_results(
            engine,
            disconnected,
            query,
            results,
            None,
            *row_panel_folded,
            row_panel_split,
            row_panel,
            form,
            &scope,
            cx,
        )))
        .into_any_element()
}

/// The filters over a preview's rows: one bar per filter, stacked above the
/// grid the pager sits over — gated on the same one state, because a structure
/// listing has no rows to narrow.
///
/// A bar is a column, an operator, a value and the joiner to the bar above it,
/// or the user's own SQL where the column dropdown says so (spec §2.4).
fn render_filter_bar(
    filters: &[FilterRow],
    columns: &[db::Column],
    engine: Engine,
    next_join: Conjunction,
    t: Theme,
) -> AnyElement {
    let names: Vec<SharedString> = columns
        .iter()
        .map(|column| SharedString::from(column.name.clone()))
        .collect();
    div()
        .w_full()
        .flex_shrink_0()
        .flex()
        .flex_col()
        // Between the filters and the headers of the grid under them.
        .border_b_1()
        .border_color(t.border)
        .children(filters.iter().enumerate().map(|(row, filter)| {
            filter_bar_row()
                // The first bar joins to nothing above it.
                .children((row > 0).then(|| {
                    join_button(("filter-join", row), filter.conjunction, t).on_click(
                        move |_, window, cx| {
                            window.dispatch_action(Box::new(ToggleFilterJoin { row }), cx);
                        },
                    )
                }))
                .child(
                    button(
                        ("filter-column", row),
                        match filter.raw {
                            true => engine.raw_filter_label().to_string(),
                            false => filter
                                .column
                                .clone()
                                .unwrap_or_else(|| "Column…".to_string()),
                        },
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    // So it reads as a dropdown rather than as a button that
                    // does something. Its own colour, for the reason every
                    // button's content carries one.
                    .child(
                        icon(icon::CHEVRON_DOWN)
                            .size(px(layout::ICON_SIZE))
                            .text_color(t.text_faint),
                    )
                    // The grid's own column names, because the preview is
                    // dbdelve's `SELECT *` and a header is the server's word for
                    // the column rather than an alias.
                    .dropdown_menu({
                        let names = names.clone();
                        let chosen = filter.column.clone();
                        let raw = filter.raw;
                        move |menu, _, _| {
                            names
                                .iter()
                                .fold(
                                    menu.scrollable(true).max_h(px(layout::MENU_MAX_HEIGHT)),
                                    |menu, name| {
                                        menu.menu_with_check(
                                            name.clone(),
                                            !raw && chosen.as_deref() == Some(name.as_ref()),
                                            Box::new(SetFilterColumn {
                                                row,
                                                column: name.to_string(),
                                            }),
                                        )
                                    },
                                )
                                // Below the names and behind a rule, because it
                                // is not one of them: it replaces the bar with
                                // a statement of the user's own.
                                .separator()
                                .menu_with_check(
                                    engine.raw_filter_label(),
                                    raw,
                                    Box::new(SetFilterRaw { row }),
                                )
                        }
                    }),
                )
                // A raw bar is one wide input: there is no column to compare
                // and no operator to compare it with.
                .children((!filter.raw).then(|| {
                    button(
                        ("filter-operator", row),
                        filter.operator.symbol(engine),
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .child(
                        icon(icon::CHEVRON_DOWN)
                            .size(px(layout::ICON_SIZE))
                            .text_color(t.text_faint),
                    )
                    .dropdown_menu({
                        let chosen = filter.operator;
                        move |menu, _, _| {
                            Operator::ALL
                                .into_iter()
                                // An operator the engine cannot express is not
                                // offered: SQLite has no regex (spec §7).
                                .filter(|operator| operator.on(engine))
                                .fold(
                                    menu.scrollable(true).max_h(px(layout::MENU_MAX_HEIGHT)),
                                    |menu, operator| {
                                        menu.menu_with_check(
                                            operator.label(engine),
                                            operator == chosen,
                                            Box::new(SetFilterOperator { row, operator }),
                                        )
                                    },
                                )
                        }
                    })
                }))
                // An absence needs no value, and a box that cannot change what
                // runs is a box to read past.
                .children(
                    (filter.raw || filter.operator.takes_value())
                        .then(|| Input::new(&filter.value).small().min_w_0().flex_1()),
                )
                .child(
                    icon_button(
                        ("remove-filter", row),
                        icon::CLOSE,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip("Remove filter")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(RemoveFilter { row }), cx);
                    }),
                )
        }))
        .child(
            filter_bar_row()
                // The joiner the next bar will carry, so it is chosen where the
                // bar is added rather than after it lands.
                .children((!filters.is_empty()).then(|| {
                    join_button("next-join", next_join, t).on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleNextJoin), cx);
                    })
                }))
                .child(
                    button("add-filter", "Add filter", Tone::Quiet, Control::Compact, t).on_click(
                        |_, window, cx| {
                            window.dispatch_action(Box::new(AddFilter), cx);
                        },
                    ),
                ),
        )
        .into_any_element()
}

/// `AND` or `OR`, as the two-state button it is. A dropdown of two rows is a
/// menu to open for something a click already says.
fn join_button(id: impl Into<gpui::ElementId>, conjunction: Conjunction, t: Theme) -> Button {
    button(id, conjunction.as_str(), Tone::Quiet, Control::Compact, t).tooltip("AND or OR")
}

/// One line of the filter stack, at the height every other control strip is.
fn filter_bar_row() -> gpui::Div {
    div()
        .w_full()
        .h(px(layout::TAB_HEIGHT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .px(px(layout::SPACE_SM))
}

/// The "New row" form (spec §4), in the place the row panel takes beside the
/// grid rather than over it.
///
/// The buttons are Cancel and **Review SQL**: this generates the statement and
/// shows it, and running it is the review panel's ask, not this one's.
fn render_new_row_panel(
    engine: Engine,
    form: &InsertForm,
    scope: &str,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);

    let fields: Vec<AnyElement> = form
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let null_workspace = cx.entity().downgrade();
            div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(field.column.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(layout::TEXT_XS))
                                .text_color(t.text_faint)
                                .child(field.data_type.clone()),
                        )
                        .child(
                            button(
                                ("insert-null", index),
                                "NULL",
                                // Filled while it is on, because whether this
                                // field is a NULL is the only thing the chip
                                // has to say.
                                if field.nulled {
                                    Tone::Primary
                                } else {
                                    Tone::Quiet
                                },
                                Control::Inline,
                                t,
                            )
                            .on_click(move |_, _, cx| {
                                _ = null_workspace.update(cx, |workspace, cx| {
                                    workspace.toggle_insert_null(index, cx);
                                });
                            }),
                        ),
                )
                .child(Input::new(&field.input).small())
                .into_any_element()
        })
        .collect();

    let cancel_workspace = cx.entity().downgrade();
    let review_workspace = cancel_workspace.clone();

    div()
        .size_full()
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_MD))
        .p(px(layout::SPACE_MD))
        .child(section_label(t, "New row"))
        // The one line that says what an empty field means, because the
        // three-way rule is invisible otherwise.
        .child(
            div()
                .text_size(px(layout::TEXT_SM))
                .text_color(t.text_faint)
                .child("A field left blank is left out, so the column keeps its default."),
        )
        .child(
            div()
                .id("new-row-fields")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .smooth_scroll(&smooth_scoped("new-row-fields", scope, cx))
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_MD))
                .children(fields),
        )
        .child(
            div()
                .flex()
                .justify_end()
                .gap(px(layout::SPACE_SM))
                .child(
                    button(
                        "cancel-new-row",
                        "Cancel",
                        Tone::Quiet,
                        Control::Standard,
                        t,
                    )
                    .on_click(move |_, _, cx| {
                        _ = cancel_workspace.update(cx, |workspace, cx| {
                            workspace.close_new_row(cx);
                        });
                    }),
                )
                .child(
                    button(
                        "review-new-row",
                        engine.review_label(),
                        Tone::Primary,
                        Control::Standard,
                        t,
                    )
                    .on_click(move |_, _, cx| {
                        _ = review_workspace.update(cx, |workspace, cx| {
                            workspace.confirm_new_row(cx);
                        });
                    }),
                ),
        )
        .into_any_element()
}

fn render_routine(tab: &ObjectTab, scope: &str, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();
    let ObjectBody::Routine(routine) = &tab.body else {
        return div().into_any_element();
    };
    let kind = match routine.kind {
        RoutineKind::Function => "Function",
        RoutineKind::Procedure => "Procedure",
    };

    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(t.panel_glass())
        .child(
            div()
                .p(px(layout::SPACE_LG))
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_SM))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(layout::TEXT_LG))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{}.{}", tab.schema, tab.name)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(layout::SPACE_LG))
                        .text_size(px(layout::TEXT_SM))
                        .text_color(t.text_muted)
                        .child(kind)
                        .child(format!("Language: {}", routine.language))
                        .children(
                            (!routine.result_type.is_empty())
                                .then(|| div().child(format!("Returns: {}", routine.result_type))),
                        )
                        .child(div().ml_auto().child(key_hint(
                            t,
                            "escape",
                            "returns to the editor",
                        ))),
                ),
        )
        .child(
            div()
                .id("routine-definition")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .smooth_scroll(&smooth_scoped("routine-definition", scope, cx))
                .p(px(layout::SPACE_LG))
                .font_family(code)
                .child(routine.definition.clone()),
        )
        .into_any_element()
}

/// How long a sent cancel may go unanswered before the button stops claiming
/// it is cancelling and admits it is the server that has not replied.
const CANCEL_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

fn clock(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The results plane: the brightest tone, because the data is the point.
///
/// A short status is centred and set in the app face -- it is a sentence
/// about the pane, not query output. An error keeps the editor's monospace
/// and the left edge, because it quotes the server and gets read against the
/// SQL above it. The grid and every message are alternatives, not layers: a
/// full-size message beside a full-size table gets pushed off the pane
/// entirely.
///
/// `query_tab` is the buffer's tab, and `None` for an object tab's preview,
/// which has no buffer.
#[allow(clippy::too_many_arguments)]
fn render_results(
    engine: Engine,
    // A failure on a connection that is gone is answered by reconnecting,
    // not by editing the statement.
    disconnected: bool,
    query: &QueryState,
    results: &Entity<TableState<ResultGrid>>,
    query_tab: Option<&QueryTab>,
    folded: bool,
    split: &Entity<ResizableState>,
    row_panel: &RowPanel,
    form: Option<&InsertForm>,
    scope: &str,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();
    let grid = fonts(cx).grid.clone();
    let centered = |child: AnyElement| {
        div()
            .size_full()
            .p(px(layout::SPACE_LG))
            .flex()
            .items_center()
            .justify_center()
            .child(child)
            .into_any_element()
    };
    let quiet_line = |line: String| {
        div()
            .text_size(px(layout::TEXT_SM))
            .text_color(t.text_muted)
            .child(line)
            .into_any_element()
    };
    // The default `Loader` icon names a file dbdelve's asset source does not
    // serve, so the spinner has to be pointed at the one it does.
    let spinner = || {
        Spinner::new()
            .icon(IconName::LoaderCircle)
            .color(t.text_muted.into())
            .small()
            .into_any_element()
    };

    let (started, cancelling) = match query {
        QueryState::Running {
            started,
            cancelling,
            ..
        } => (Some(*started), *cancelling),
        _ => (None, None),
    };
    let cancel = move |cx: &mut Context<Workspace>| {
        // A word rather than an icon: a square or a cross beside a status line
        // reads as "close this", and the quiet tone is what keeps it from
        // competing with rows that are still coming.
        //
        // Once the request is out the label is the only acknowledgement the
        // click gets, and the statement is still running, so the button goes
        // inert rather than away.
        let label = match cancelling {
            Some(sent) if sent.elapsed() >= CANCEL_PATIENCE => "Still waiting on server…",
            Some(_) => "Cancelling…",
            None => "Cancel",
        };
        let timer = started.map(|started| quiet_line(clock(started.elapsed())));
        div()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_SM))
            .children(timer)
            .child(
                button("cancel-query", label, Tone::Quiet, Control::Compact, t)
                    .disabled(cancelling.is_some())
                    .on_click(cx.listener(|workspace, _, window, cx| {
                        workspace.cancel_query(&CancelQuery, window, cx);
                    })),
            )
    };
    // A refresh keeps the rows it is replacing (`execute_and_then`'s
    // `keep_rows`), and a centred spinner over rows the user is still reading
    // hides the data this pane is for. So every state that has rows behind it
    // falls through to the grid, and the run says so in a strip above it
    // instead of in place of it.
    let has_rows = results.read(cx).delegate().rows_count(cx) > 0;

    // The two loading states, overlaid on the grid rather than replacing it
    // (below). Opening a tab focuses the grid before its first result lands,
    // and a `message` that replaces it unmounts the very element that focus
    // handle names -- the window is left with a focused handle no element
    // tracks, and no dispatch path for anything, `secondary-w` included.
    let loading = match query {
        // A preview runs the moment its tab is shown, so an idle one is a
        // tab that is about to run rather than one waiting to be asked. It has
        // nothing to cancel yet, though, which is the whole difference here.
        QueryState::Idle if query_tab.is_none() && !has_rows => Some(centered(spinner())),
        QueryState::Running { .. } if !has_rows => Some(centered(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(layout::SPACE_MD))
                .child(spinner())
                .child(cancel(cx))
                .into_any_element(),
        )),
        _ => None,
    };

    let message = match query {
        QueryState::Idle if query_tab.is_some() => Some(centered(
            key_hint(
                t,
                "secondary-enter",
                "runs the selection or statement under the cursor",
            )
            .into_any_element(),
        )),
        QueryState::Failed(error) => {
            let at = query_tab
                .and_then(|tab| error_in_buffer(tab, cx))
                .map(|(at, _)| at);
            let position = match (at, error.position) {
                (None, Some(position)) => format!(" (at byte {position})"),
                _ => String::new(),
            };
            Some(
                div()
                    .size_full()
                    .p(px(layout::SPACE_LG))
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_MD))
                    .child(
                        div()
                            .font_family(code)
                            .text_color(t.danger)
                            .child(format!("{}{position}", error.message)),
                    )
                    .when_some(at, |pane, at| {
                        pane.child(
                            div().flex().child(
                                button(
                                    "jump-to-error",
                                    format!(
                                        "Go to line {}, column {}",
                                        at.line + 1,
                                        at.character + 1
                                    ),
                                    Tone::Quiet,
                                    Control::Compact,
                                    t,
                                )
                                .on_click(cx.listener(
                                    |workspace, _, window, cx| {
                                        workspace.jump_to_error(window, cx);
                                    },
                                )),
                            ),
                        )
                    })
                    .when(disconnected, |pane| {
                        pane.child(
                            div()
                                .flex()
                                .child(reconnect_button("reconnect-from-error", t)),
                        )
                    })
                    .into_any_element(),
            )
        }
        // A restored snapshot written before it kept a row count is `Complete`
        // over zero rows it can nonetheless show, so the count alone cannot
        // decide this.
        QueryState::Complete {
            rows,
            rows_affected,
            ..
        } if *rows == 0 && !has_rows => Some(centered(quiet_line(match rows_affected {
            Some(rows) => format!("Query completed. Server row count: {rows}."),
            None => "Query completed.".into(),
        }))),
        _ => None,
    };

    // Values are read by comparing them down a column, which only lines up in
    // a monospaced face -- and the header inherits it, so the heading of a
    // column sits in the same rhythm as its values. The library's table sets
    // no family of its own, so this is where the cells and their headings get
    // theirs.
    let grid = div()
        .size_full()
        .flex()
        .flex_col()
        .min_h_0()
        .min_w_0()
        // The loading overlay below covers this same strip's spinner while
        // there are no rows yet; past that, a refresh says so up here and
        // keeps the rows it is replacing on screen underneath.
        .children(
            (matches!(query, QueryState::Running { .. }) && has_rows).then(|| {
                div()
                    .h(px(layout::TAB_HEIGHT))
                    .flex_shrink_0()
                    .px(px(layout::SPACE_SM))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .border_b_1()
                    .border_color(t.border)
                    .child(spinner())
                    .child(quiet_line("Refreshing…".into()))
                    .child(div().ml_auto().child(cancel(cx)))
            }),
        )
        .child({
            let rows_scroll = smooth_for(
                "results",
                scope,
                results.read(cx).vertical_scroll_handle.clone(),
                results.read(cx).horizontal_scroll_handle.clone(),
                cx,
            );
            // `flex_1` rather than full height: under the "Refreshing…" strip a
            // full-height grid overruns the pane by the strip's height and
            // takes its last row and scrollbar with it.
            div()
                .id("results")
                .smooth_scroll(&rows_scroll)
                .flex_1()
                .w_full()
                .min_h_0()
                .min_w_0()
                .font_family(grid)
                // The grid's own delegate has no key hook and the
                // focused element is the table root, so `enter` is
                // caught here on its way out of the Table context.
                .on_action(cx.listener(Workspace::edit_cell))
                .on_action(cx.listener(Workspace::copy_cell))
                .on_action(cx.listener(Workspace::copy_row))
                .on_action(cx.listener(Workspace::copy_rows))
                .on_action(cx.listener(Workspace::copy_results))
                .on_action(cx.listener(Workspace::set_null))
                .on_action(cx.listener(Workspace::set_empty))
                .on_action(cx.listener(Workspace::set_default))
                .on_action(cx.listener(Workspace::request_write_mode))
                .on_action(cx.listener(Workspace::delete_row))
                .on_action(cx.listener(Workspace::follow_foreign_key))
                .on_action(cx.listener(Workspace::open_reference))
                .on_action(cx.listener(Workspace::show_references))
                .child(DataTable::new(results).bordered(false).stripe(false))
                .on_prepaint({
                    let results = results.clone();
                    move |_, _, cx| results.update(cx, result_grid::keep_active_in_view)
                })
        })
        .into_any_element();

    let content = match message {
        Some(message) => message,
        None => match loading {
            // A child rather than replacing the grid: it must stay mounted
            // for the focus a fresh tab put on it, `secondary-w` included --
            // see the comment above `loading`.
            Some(overlay) => div()
                .relative()
                .size_full()
                .min_h_0()
                .min_w_0()
                .child(grid)
                .child(
                    // Pinned to the corner: a `div` is block layout, which puts
                    // an absolute child with no insets where it would have
                    // flowed -- below the full-height grid, out of sight.
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        // Without this a header click reaches the grid
                        // mounted underneath -- empty or stale, since this is
                        // exactly the state a run has not replaced it yet.
                        // The overlay's own Cancel button still works: this
                        // only stops a click from reaching past the overlay,
                        // never from landing on it.
                        .occlude()
                        .child(overlay),
                )
                .into_any_element(),
            None => grid,
        },
    };

    // The new-row form, or the selected row's inspector, takes a slice beside
    // whatever is occupying the main area -- the grid, a message or a loading
    // overlay -- rather than just the grid, so New row stays usable on an
    // empty table and Delete's refusal is visible after a failed preview.
    let panel = match form {
        Some(form) => Some((render_new_row_panel(engine, form, scope, cx), false)),
        None => {
            render_row_inspector(results, folded, row_panel, scope, cx).map(|panel| (panel, folded))
        }
    };
    let body = div().size_full().flex().min_h_0();
    let content = match panel {
        None => body.child(content),
        // Folded, the panel keeps a strip of the edge rather than
        // vanishing: a selected row with nowhere to bring its
        // values back from is a panel the user has lost.
        Some((strip, true)) => body.child(content).child(strip),
        Some((panel, false)) => body.child(
            h_resizable("row-inspector-split")
                .with_state(split)
                .child(resizable_panel().child(content))
                .child(
                    resizable_panel()
                        .size(px(layout::INSPECTOR_WIDTH))
                        .size_range(
                            px(layout::INSPECTOR_MIN_WIDTH)..px(layout::INSPECTOR_MAX_WIDTH),
                        )
                        .child(panel),
                ),
        ),
    };

    let pane = div().size_full().min_h_0().bg(t.data_glass());
    match query_tab.and_then(|tab| result_switcher(tab, cx)) {
        None => pane.child(content),
        Some(switcher) => pane
            .flex()
            .flex_col()
            .child(switcher)
            .child(div().flex_1().min_h_0().child(content)),
    }
    .into_any_element()
}

/// One chip per statement a queue has run, and one for the statement still
/// running, above the result the selected chip is showing.
///
/// Only from the second result: one chip is nothing to switch between, and an
/// ordinary run has no queue at all.
fn result_switcher(tab: &QueryTab, cx: &mut Context<Workspace>) -> Option<AnyElement> {
    let t = *theme(cx);
    let queue = tab.queue.as_ref()?;
    if queue.done.len() < 2 {
        return None;
    }
    // The statement in flight is the tab's own slot, not one of `done`, so it
    // is the chip one past the end. Selecting it is what following the run
    // means; any other chip reads a result while that one keeps going.
    let running = queue.awaiting.then(|| {
        let label = tab
            .ran_from
            .as_ref()
            .map(|(_, sql)| query_label(sql))
            .unwrap_or_default();
        let index = queue.done.len();
        result_chip(
            tab.id,
            index,
            &label,
            &tab.query,
            index == queue.showing,
            cx,
        )
    });
    let chips: Vec<AnyElement> = queue
        .done
        .iter()
        .enumerate()
        .map(|(index, finished)| {
            result_chip(
                tab.id,
                index,
                &query_label(&finished.sql),
                &finished.state,
                index == queue.showing,
                cx,
            )
        })
        .chain(running)
        .collect();

    Some(
        div()
            .id("query-results")
            .h(px(layout::TAB_HEIGHT))
            .flex_shrink_0()
            .px(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .border_b_1()
            .border_color(t.border)
            // A long queue is scrolled through rather than allowed to push
            // the grid it labels off the pane.
            .overflow_x_scroll()
            .children(chips)
            .into_any_element(),
    )
}

/// One statement of a queue, in the row-limit chips' clothes: its place in the
/// queue, what it was, and what it came back with.
fn result_chip(
    id: u64,
    index: usize,
    label: &str,
    state: &QueryState,
    selected: bool,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let (readout, tint) = match state {
        QueryState::Running { .. } => (Some("running".to_string()), t.text_muted),
        QueryState::Complete { rows, .. } => (Some(compact_count(*rows)), t.text_faint),
        QueryState::Failed(_) => (Some("failed".to_string()), t.danger),
        _ => (None, t.text_faint),
    };
    div()
        .id(("queued-result", index))
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .h(px(24.))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::TEXT_SM))
        .whitespace_nowrap()
        .map(|chip| {
            if selected {
                chip.bg(t.element_active).text_color(t.text)
            } else {
                chip.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(
            div()
                .text_color(t.text_faint)
                .child(format!("{}.", index + 1)),
        )
        .child(label.to_string())
        .children(readout.map(|readout| div().text_color(tint).child(readout)))
        .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
            workspace.show_queued_result(Tab::Query(id), index, cx);
        }))
        .into_any_element()
}

/// The selected row, one field per line, beside the grid.
///
/// A row read across a grid is a row read against the column headings
/// twenty columns away; read down a list it is just a row. The list also
/// has room for a value the column had to clip, which is what makes this
/// the value inspector the spec asks for in §4.4.
///
/// Nothing here is state of dbdelve's own: the selected row belongs to the
/// grid, so the panel cannot disagree with the highlight in the grid, and
/// arrow keys move both.
fn render_row_inspector(
    results: &Entity<TableState<ResultGrid>>,
    folded: bool,
    row_panel: &RowPanel,
    scope: &str,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let t = *theme(cx);
    let grid = fonts(cx).grid.clone();

    let (row_ix, rows, fields) = {
        let table = results.read(cx);
        let row_ix = table.selected_row()?;
        (
            row_ix,
            table.delegate().rows_count(cx),
            table.delegate().fields(row_ix),
        )
    };
    // A selection can outlive the rows it was made against.
    if fields.is_empty() {
        return None;
    }
    row_panel.on_screen.set(true);

    let fold = |id: &'static str, tooltip: &'static str, cx: &mut Context<Workspace>| {
        icon_button(id, icon::ROW_PANEL, Tone::Quiet, Control::Compact, t)
            .tooltip(tooltip)
            .on_click(cx.listener(|workspace, _, window, cx| {
                workspace.toggle_row_panel(&ToggleRowPanel, window, cx);
            }))
    };
    if folded {
        return Some(
            div()
                .h_full()
                .flex_shrink_0()
                .px(px(layout::SPACE_XS))
                .border_l_1()
                .border_color(t.border)
                .child(
                    div()
                        .h(px(layout::TAB_HEIGHT))
                        .flex()
                        .items_center()
                        .child(fold("show-row-inspector", "Show the row panel", cx)),
                )
                .into_any_element(),
        );
    }

    let table = results.clone();
    Some(
        div()
            .size_full()
            .flex()
            .flex_col()
            // The same plane as the results, separated by the split's seam
            // rather than its tone: a second tint here reads as a slab pasted
            // over the window instead of a panel inside it.
            .child(
                div()
                    .h(px(layout::TAB_HEIGHT))
                    .px(px(layout::SPACE_SM))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .child(
                        div()
                            .text_size(px(layout::TEXT_SM))
                            .text_color(t.text_muted)
                            .child(format!(
                                "Row {} of {}",
                                group_thousands(row_ix as u64 + 1),
                                group_thousands(rows as u64)
                            )),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .flex()
                            .items_center()
                            .child(fold("hide-row-inspector", "Hide the row panel", cx))
                            .child(
                                icon_button(
                                    "close-row-inspector",
                                    icon::CLOSE,
                                    Tone::Quiet,
                                    Control::Compact,
                                    t,
                                )
                                .tooltip("Close the row panel")
                                .on_click(move |_, _, cx| {
                                    table.update(cx, |table, cx| table.clear_selection(cx));
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("row-inspector")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .smooth_scroll(&smooth_scoped("row-inspector", scope, cx))
                    .px(px(layout::SPACE_SM))
                    .pb(px(layout::SPACE_SM))
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_MD))
                    .children(fields.into_iter().enumerate().map(|(col_ix, field)| {
                        let group = format!("row-field-{col_ix}");
                        let copy = field.value.is_some().then(|| {
                            if row_panel.copied == Some((row_ix, col_ix)) {
                                return div()
                                    .size(px(Control::Compact.height()))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        icon(icon::CHECK)
                                            .size(px(layout::ICON_SIZE))
                                            .text_color(t.success),
                                    )
                                    .with_animation(
                                        ("copied-row-field", col_ix),
                                        Animation::new(std::time::Duration::from_millis(150)),
                                        |tick, delta| tick.opacity(delta),
                                    )
                                    .into_any_element();
                            }
                            div()
                                .opacity(0.)
                                .group_hover(group.clone(), |style| style.opacity(1.))
                                .child(
                                    icon_button(
                                        ("copy-row-field", col_ix),
                                        icon::COPY,
                                        Tone::Quiet,
                                        Control::Compact,
                                        t,
                                    )
                                    .tooltip("Copy value")
                                    .on_click(cx.listener(
                                        move |workspace, _, _, cx| {
                                            workspace.copy_row_field(row_ix, col_ix, cx);
                                        },
                                    )),
                                )
                                .into_any_element()
                        });
                        div()
                            .group(group)
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_XS))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(layout::SPACE_SM))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .text_size(px(layout::TEXT_SM))
                                            .text_color(t.text_muted)
                                            .child(field.name),
                                    )
                                    // Absent rather than guessed: a type
                                    // dbdelve could not learn is not shown as
                                    // one it inferred from the text.
                                    .child(
                                        div()
                                            .ml_auto()
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .gap(px(layout::SPACE_XS))
                                            .children(field.data_type.map(|data_type| {
                                                div()
                                                    .text_size(px(layout::TEXT_XS))
                                                    .text_color(t.text_faint)
                                                    .child(data_type)
                                            }))
                                            .children(copy),
                                    ),
                            )
                            .child(
                                div()
                                    .font_family(grid.clone())
                                    .text_size(px(layout::TEXT_SM))
                                    .map(|value| match field.value {
                                        Some(text) => value.text_color(t.text).child(text),
                                        None if field.missing => value,
                                        // Italic so a NULL cannot be read
                                        // as the four-letter string.
                                        None => value
                                            .text_color(t.text_faint)
                                            .italic()
                                            .child(result_grid::NULL_LABEL),
                                    }),
                            )
                    })),
            )
            .into_any_element(),
    )
}

/// One segment of the Data | Structure pair. A quiet chip rather than a
/// filled button: it selects a view of the same object, it does not act.
/// One chip of a two-way toggle over the results pane. It carries the command
/// it runs rather than deciding from its label, because there are two of these
/// toggles now -- an object tab's data and structure, and a query tab's rows
/// and plan -- and a label is not what tells them apart.
fn preview_tab(
    label: &'static str,
    path: &'static str,
    selected: bool,
    command: Command,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let t = *theme(cx);
    div()
        .id(label)
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .h(px(24.))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::TEXT_SM))
        .map(|tab| {
            if selected {
                tab.bg(t.element_active).text_color(t.text)
            } else {
                tab.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(
            icon(path).size(px(12.)).text_color(
                kind_color(path)
                    .map(crate::theme::ConnectionColor::swatch)
                    .unwrap_or(if selected { t.text } else { t.text_faint }),
            ),
        )
        .child(label)
        .on_click(cx.listener(move |workspace, _: &ClickEvent, window, cx| {
            workspace.run_command(command.clone(), window, cx);
        }))
}

/// One row-limit choice. A chip rather than a menu: four numbers fit, and a
/// number behind a popover is a number nobody checks.
fn row_limit_chip(rows: usize, selected: bool, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    div()
        .id(("row-limit", rows))
        .flex()
        .items_center()
        .h(px(24.))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::TEXT_SM))
        .map(|chip| {
            if selected {
                chip.bg(t.element_active).text_color(t.text)
            } else {
                chip.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(compact_count(rows))
        .on_click(cx.listener(move |_, _, window, cx| {
            window.dispatch_action(Box::new(SetRowLimit { rows }), cx);
        }))
        .into_any_element()
}

fn render_structure(
    state: &StructureState,
    scope: &str,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();

    let structure = match state {
        StructureState::Loading => {
            return div()
                .p(px(layout::SPACE_LG))
                .text_color(t.text_muted)
                .child("Loading structure…")
                .into_any_element();
        }
        StructureState::Failed(message) => {
            return div()
                .p(px(layout::SPACE_LG))
                .text_color(t.danger)
                .child(message.clone())
                .into_any_element();
        }
        StructureState::Loaded(structure) => structure,
    };

    let heading = |label: &'static str| {
        div()
            .pt(px(layout::SPACE_MD))
            .child(section_label(t, label))
    };
    let name_column = |name: String| {
        div()
            .w(px(220.))
            .min_w(px(220.))
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .font_weight(FontWeight::MEDIUM)
            .child(name)
    };
    let definitions = |definitions: &[db::NamedDefinition]| {
        definitions
            .iter()
            .map(|definition| {
                div()
                    .flex()
                    .gap(px(layout::SPACE_MD))
                    .child(name_column(definition.name.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(t.text_muted)
                            .child(definition.definition.clone()),
                    )
            })
            .collect::<Vec<_>>()
    };

    div()
        .id("structure")
        .size_full()
        .overflow_y_scroll()
        .smooth_scroll(&smooth_scoped("structure", scope, cx))
        .p(px(layout::SPACE_LG))
        .font_family(code)
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_XS))
        .child(heading("Columns"))
        .children(structure.columns.iter().map(|column| {
            div()
                .flex()
                .gap(px(layout::SPACE_MD))
                .child(name_column(column.name.clone()))
                .child(
                    div()
                        .w(px(200.))
                        .min_w(px(200.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        // The same colour the editor gives a type name, so
                        // structure and SQL read as one vocabulary.
                        .text_color(t.syntax_type)
                        .child(column.data_type.clone()),
                )
                .child(
                    div()
                        .w(px(80.))
                        .min_w(px(80.))
                        .text_color(t.text_muted)
                        .child(if column.nullable {
                            "nullable"
                        } else {
                            "not null"
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_color(t.text_muted)
                        .child(column.default.clone().unwrap_or_default()),
                )
        }))
        .children((!structure.indexes.is_empty()).then(|| heading("Indexes")))
        .children(definitions(&structure.indexes))
        .children((!structure.constraints.is_empty()).then(|| heading("Constraints")))
        .children(definitions(&structure.constraints))
        .into_any_element()
}

/// The preview's row limit and pager, centred in the status bar: what the
/// relation's rows were asked for, and the way to the ones after them. `None`
/// on anything but a relation's rows.
pub(crate) fn render_paging(profile: &Profile, cx: &mut Context<Workspace>) -> Option<AnyElement> {
    let t = *theme(cx);
    let session = &profile.session;
    // What the preview asked the server for, and the only control over it.
    // Shown only while the rows are: it is a property of these rows, not of
    // the window.
    let preview = session.active_object().and_then(|tab| match &tab.body {
        ObjectBody::Relation {
            limit,
            offset,
            query,
            showing_structure: false,
            ..
        } => Some((
            *limit,
            *offset,
            // A full page may have another behind it; a short one is the
            // relation's end. The same gate `turn_page` holds, read here only
            // to decide whether the button is worth drawing.
            matches!(query, QueryState::Complete { rows, .. } if *rows >= *limit),
        )),
        _ => None,
    });
    let row_limit = preview.map(|(limit, _, _)| {
        let chips: Vec<_> = ROW_LIMITS
            .into_iter()
            .map(|rows| row_limit_chip(rows, rows == limit, cx))
            .collect();
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .child(
                div()
                    .text_size(px(layout::TEXT_SM))
                    .text_color(t.text_faint)
                    .child("Rows"),
            )
            .children(chips)
    });
    // The pager appears only once there is somewhere to go: a first page
    // shorter than its limit is the whole relation, and arrows over it are
    // controls that can do nothing.
    let pager = preview.and_then(|(_, offset, full_page)| {
        (offset > 0 || full_page).then(|| {
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_XS))
                .children((offset > 0).then(|| {
                    icon_button(
                        "previous-page",
                        icon::CHEVRON_LEFT,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip("Previous page")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(PreviousPage), cx);
                    })
                }))
                .child(
                    // Dressed as the row-limit chips beside it rather than as
                    // the library's field: its own fill is the frost again,
                    // which stacks to a black slab on this strip. A wash lets
                    // the glass through and is a faint step on opaque themes.
                    div()
                        .h(px(layout::CONTROL_HEIGHT_COMPACT))
                        .w(px(44.))
                        .px(px(layout::SPACE_SM))
                        .flex()
                        .items_center()
                        .rounded(px(layout::RADIUS_CONTROL))
                        .bg(t.element_active)
                        // The strong edge is what says "type here": on glass
                        // the wash alone is close to the strip behind it.
                        .border_1()
                        .border_color(t.border_strong)
                        .text_size(px(layout::TEXT_SM))
                        .text_color(t.text)
                        .child(
                            Input::new(&session.page_input)
                                .appearance(false)
                                .px_0()
                                .h_full()
                                .text_size(px(layout::TEXT_SM)),
                        ),
                )
                .children(full_page.then(|| {
                    icon_button(
                        "next-page",
                        icon::CHEVRON_RIGHT,
                        Tone::Quiet,
                        Control::Compact,
                        t,
                    )
                    .tooltip("Next page")
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(Box::new(NextPage), cx);
                    })
                }))
        })
    });

    preview.map(|_| {
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_MD))
            .children(row_limit)
            .children(pager)
            .into_any_element()
    })
}

/// The tab strip. It sits directly above the editor and starts where the
/// editor's text does, so a tab labels the surface under it rather than the
/// window: the active one is lifted to the editor's tone, the rest are names
/// that reveal a wash on hover. No boxes, no hairlines — tone carries the
/// state.
fn render_tab_strip(
    profile: &Profile,
    editor_font_size: f32,
    strip: &TabStrip,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let workspace = cx.entity().downgrade();
    let session = &profile.session;
    let on_query_tab = matches!(session.active, Tab::Query(_));
    let runnable = session.editor(session.active).is_some();
    let engine = profile.config.engine();

    let chip = |active: bool| {
        div()
            .h(px(layout::TAB_CHIP_HEIGHT))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .rounded(px(layout::RADIUS_CONTROL))
            .map(|tab| {
                if active {
                    tab.bg(t.panel).text_color(t.text)
                } else {
                    tab.text_color(t.text_muted)
                        .hover(|style| style.bg(t.element_hover))
                }
            })
    };
    let name_label = |name: String| {
        div()
            .max_w(px(180.))
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .child(name)
    };

    // Middle-click closes the tab, as it does in every browser and editor. It
    // goes through `ask_before_close` rather than the chip's own button so the
    // gesture means what `cmd+w` means -- a saved query is still asked about
    // rather than deleted by a stray wheel press.
    let close_on_middle_click = |chip: Stateful<Div>, target: CloseTarget| {
        let workspace = workspace.clone();
        chip.on_aux_click(move |event, window, cx| {
            if !event.is_middle_click() {
                return;
            }
            _ = workspace.update(cx, |workspace, cx| {
                workspace.ask_before_close(target.clone(), window, cx);
            });
        })
    };

    // One chip per unsaved buffer, numbered in strip order. There used to be
    // exactly one, because there used to be exactly one editor.
    // A chip that follows the pointer while it is being dragged, and reports
    // where it sits for the drag that may start from it. The others slide by
    // however far the drag has made room.
    let drag_layout = strip.drag.as_ref().map(|drag| (drag, drag.layout()));
    let draggable = |chip: Stateful<Div>, key: TabKey| {
        let target = drag_layout.as_ref().and_then(|(drag, layout)| {
            let at = drag.slots.iter().position(|slot| slot.key == key)?;
            Some((layout.offsets[at], drag.key == key))
        });
        let held = target.is_some_and(|(_, held)| held);
        // The chip in hand is where the pointer has it; the rest glide to
        // where the drag has made room, or back to nothing once it is over.
        let offset = match target {
            Some((offset, true)) => {
                strip.shift.place(&key, offset);
                offset
            }
            Some((offset, false)) => strip.shift.glide(&key, offset),
            None => strip.shift.glide(&key, 0.),
        };
        let begin = workspace.clone();
        let (begin_key, bounds_key) = (key.clone(), key);
        let bounds = strip.bounds.clone();
        chip.relative()
            .left(px(offset))
            // Lifted: a chip with no fill of its own would be a name sliding
            // over the names it passes.
            .when(held, |chip| chip.bg(t.overlay))
            .on_drag(DragTab, move |_, _, window, cx| {
                let pointer = f32::from(window.mouse_position().x);
                _ = begin.update(cx, |workspace, cx| {
                    workspace.begin_tab_drag(begin_key.clone(), pointer, cx);
                });
                cx.new(|_| gpui::Empty)
            })
            .on_prepaint(move |chip, _, _| {
                let mut bounds = bounds.borrow_mut();
                match bounds.iter_mut().find(|(key, _)| *key == bounds_key) {
                    Some(entry) => entry.1 = chip,
                    None => bounds.push((bounds_key, chip)),
                }
            })
    };

    let mut tabs = session
        .queries
        .iter()
        .filter(|tab| tab.open_query.is_none())
        .enumerate()
        .map(|(index, tab)| {
            let id = tab.id;
            let group = format!("unsaved-query-tab-{id}");
            let open_workspace = workspace.clone();
            let close_workspace = workspace.clone();
            let label = match index {
                0 => "New Query".to_string(),
                _ => format!("New Query {}", index + 1),
            };
            chip(session.active == Tab::Query(id))
                .id(("unsaved-query-tab", id as usize))
                .group(group.clone())
                .px(px(layout::SPACE_SM))
                // A pen, not a file: an unsaved buffer is a place to write, and
                // the distinction is what makes the saved tabs read as files.
                .child(row_icon(t, icon::SCRATCH_QUERY))
                .child(label)
                .child(
                    div()
                        .opacity(0.)
                        .group_hover(group, |style| style.opacity(1.))
                        .child(
                            icon_button(
                                ("close-unsaved-query", id as usize),
                                icon::CLOSE,
                                Tone::Quiet,
                                Control::Inline,
                                t,
                            )
                            .tooltip("Close tab")
                            .on_click(move |_, window, cx| {
                                // Or the chip underneath activates the tab
                                // this just closed, in the same click.
                                cx.stop_propagation();
                                _ = close_workspace.update(cx, |workspace, cx| {
                                    workspace.ask_before_close(CloseTarget::Buffer(id), window, cx);
                                });
                            }),
                        ),
                )
                .on_click(move |_, window, cx| {
                    _ = open_workspace.update(cx, |workspace, cx| {
                        workspace.activate_tab(Tab::Query(id), window, cx);
                    });
                })
                .map(|chip| {
                    draggable(
                        close_on_middle_click(chip, CloseTarget::Buffer(id)),
                        TabKey::Unsaved(id),
                    )
                })
                .into_any_element()
        })
        .collect::<Vec<_>>();

    tabs.extend(
        session
            .saved_queries
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let open_name = name.clone();
                let delete_name = name.clone();
                let middle_name = name.clone();
                let open_workspace = workspace.clone();
                let delete_workspace = workspace.clone();
                let pending = session.pending_delete.as_deref() == Some(name);
                let active = session
                    .tab_holding(name)
                    .is_some_and(|id| session.active == Tab::Query(id));
                chip(active)
                    .id(("saved-query", index))
                    .group(format!("query-tab-{index}"))
                    .px(px(layout::SPACE_SM))
                    .child(row_icon(t, icon::SAVED_QUERY))
                    .child(name_label(name.clone()))
                    .child(
                        // Revealed by its own tab, so the strip reads as names
                        // rather than a row of delete buttons.
                        div()
                            .when(!pending, |delete| {
                                delete
                                    .opacity(0.)
                                    .group_hover(format!("query-tab-{index}"), |style| {
                                        style.opacity(1.)
                                    })
                            })
                            .child(
                                // Armed, it says the word and takes the danger
                                // fill: the icon alone asks, the red confirms.
                                icon_button(
                                    ("delete-query", index),
                                    icon::DELETE,
                                    if pending { Tone::Danger } else { Tone::Quiet },
                                    Control::Inline,
                                    t,
                                )
                                .when(pending, |armed| {
                                    armed.w_auto().px(px(layout::SPACE_XS)).child(button_label(
                                        "Delete?",
                                        Tone::Danger,
                                        Control::Inline,
                                        t,
                                    ))
                                })
                                .tooltip("Delete query")
                                .on_click(move |_, window, cx| {
                                    // Or the chip underneath opens the query in
                                    // the same click, and the confirmation this
                                    // arms is cleared before it can be seen.
                                    cx.stop_propagation();
                                    _ = delete_workspace.update(cx, |workspace, cx| {
                                        workspace.arm_delete_saved_query(
                                            delete_name.clone(),
                                            window,
                                            cx,
                                        );
                                    });
                                }),
                            ),
                    )
                    .on_click(move |_, window, cx| {
                        _ = open_workspace.update(cx, |workspace, cx| {
                            workspace.open_saved_query(open_name.clone(), window, cx);
                        });
                    })
                    .map(|chip| {
                        draggable(
                            close_on_middle_click(
                                chip,
                                CloseTarget::SavedQuery(middle_name.clone()),
                            ),
                            TabKey::Saved(name.clone()),
                        )
                    })
                    .into_any_element()
            }),
    );

    // Opened objects sit after the queries, in the order they were opened.
    // Closing one is not destructive, so it gets a plain × rather than the
    // saved queries' confirmed delete.
    tabs.extend(session.objects.iter().map(|object| {
        let id = object.id;
        let group = format!("object-tab-{id}");
        let open_workspace = workspace.clone();
        let close_workspace = workspace.clone();
        chip(session.active == Tab::Object(id))
            .id(("object-tab", id as usize))
            .group(group.clone())
            .px(px(layout::SPACE_SM))
            .child(row_icon(t, object_icon(object.kind)))
            .child(name_label(object.name.clone()))
            // One relation can have as many tabs as it has filters (spec §6.3),
            // so a strip that labelled them all `customers` would cost a click
            // each to tell apart. Bounded and ellipsized: a filter can be long.
            .children((!object.filter().is_empty()).then(|| {
                div()
                    .max_w(px(120.))
                    .px(px(layout::SPACE_XS))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .bg(t.element_active)
                    .text_size(px(layout::TEXT_XS))
                    .text_color(t.text_muted)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(object.filter().to_string())
            }))
            .child(
                div()
                    .opacity(0.)
                    .group_hover(group, |style| style.opacity(1.))
                    .child(
                        icon_button(
                            ("close-object", id as usize),
                            icon::CLOSE,
                            Tone::Quiet,
                            Control::Inline,
                            t,
                        )
                        .tooltip("Close tab")
                        .on_click(move |_, window, cx| {
                            // Or the chip underneath activates the tab
                            // this just closed, in the same click.
                            cx.stop_propagation();
                            _ = close_workspace.update(cx, |workspace, cx| {
                                workspace.ask_before_close(CloseTarget::Object(id), window, cx);
                            });
                        }),
                    ),
            )
            .on_click(move |_, window, cx| {
                _ = open_workspace.update(cx, |workspace, cx| {
                    workspace.activate_tab(Tab::Object(id), window, cx);
                });
            })
            .map(|chip| {
                draggable(
                    close_on_middle_click(chip, CloseTarget::Object(id)),
                    TabKey::Object(id),
                )
            })
            .into_any_element()
    }));

    // Left to right as the user dragged them; a chip they have not placed yet
    // follows the placed ones.
    let default_keys = session
        .queries
        .iter()
        .filter(|tab| tab.open_query.is_none())
        .map(|tab| TabKey::Unsaved(tab.id))
        .chain(session.saved_queries.iter().cloned().map(TabKey::Saved))
        .chain(session.objects.iter().map(|tab| TabKey::Object(tab.id)));
    let order = session.strip_order();
    let mut placed: Vec<(TabKey, AnyElement)> = default_keys.zip(tabs).collect();
    placed.sort_by_key(|(key, _)| order.iter().position(|placed| placed == key));
    let tabs: Vec<AnyElement> = placed.into_iter().map(|(_, chip)| chip).collect();

    let confirm_workspace = workspace.clone();
    let naming_a_rename = on_query_tab && session.open_query().is_some();
    let naming = session.naming.then(|| {
        div()
            .w(px(240.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            // The input and the button share one size so the pair sits on a
            // single centreline instead of jostling.
            .child(Input::new(&session.save_name).small().flex_1())
            .child(
                icon_button(
                    "confirm-save-query",
                    if naming_a_rename {
                        icon::RENAME
                    } else {
                        icon::SAVE
                    },
                    Tone::Primary,
                    Control::Compact,
                    t,
                )
                .tooltip(if naming_a_rename {
                    "Rename query"
                } else {
                    "Save query"
                })
                .on_click(move |_, window, cx| {
                    _ = confirm_workspace.update(cx, |workspace, cx| {
                        workspace.confirm_save(window, cx);
                    });
                }),
            )
    });

    // A relation's tab shows the two views of an object from the strip: a
    // header of its own would be a second bar saying what this one already
    // says.
    let structure_toggle = session.active_object().and_then(|tab| match &tab.body {
        ObjectBody::Relation {
            showing_structure, ..
        } => Some(
            div()
                .flex_shrink_0()
                .flex()
                .gap(px(layout::SPACE_XS))
                .child(preview_tab(
                    "Data",
                    icon::TABLE,
                    !showing_structure,
                    Command::ShowStructure(false),
                    cx,
                ))
                .child(preview_tab(
                    "Structure",
                    icon::STRUCTURE,
                    *showing_structure,
                    Command::ShowStructure(true),
                    cx,
                )),
        ),
        ObjectBody::Routine(_) => None,
    });

    // Drawn only once there is a plan to turn to. Before that the pair would be
    // a control with one working half, which is the same as no control at all.
    let plan_toggle = session
        .active_query_tab()
        .filter(|tab| tab.plan.is_some())
        .map(|tab| {
            div()
                .flex_shrink_0()
                .flex()
                .gap(px(layout::SPACE_XS))
                .child(preview_tab(
                    "Data",
                    icon::TABLE,
                    !tab.showing_plan,
                    Command::ShowPlan(false),
                    cx,
                ))
                .child(preview_tab(
                    "Plan",
                    icon::PLAN,
                    tab.showing_plan,
                    Command::ShowPlan(true),
                    cx,
                ))
        });

    // On every relation tab, the structure view included: there it takes you
    // back to the data before opening the form.
    let new_row = session
        .active_object()
        .is_some_and(|tab| tab.takes_inserts(engine))
        .then(|| {
            div().flex_shrink_0().child(
                button("new-row", "New row", Tone::Quiet, Control::Compact, t).on_click(
                    |_, window, cx| {
                        window.dispatch_action(Box::new(NewRow), cx);
                    },
                ),
            )
        });

    let zoom = editor_zoom_percent(editor_font_size);
    let named = on_query_tab && session.open_query().is_some();
    let new_workspace = workspace.clone();
    let save_workspace = workspace.clone();
    let rename_workspace = workspace.clone();
    let run_workspace = workspace.clone();

    div()
        .h(px(layout::TAB_HEIGHT))
        .w_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .px(px(layout::SPACE_SM))
        // Between the tabs and whatever is under them, the filters or the grid.
        .border_b_1()
        .border_color(t.border)
        .text_size(px(layout::TEXT_SM))
        .child(
            div()
                .id("query-tabs-scroll")
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .overflow_x_scroll()
                .smooth_scroll(&smooth("tab-strip", cx))
                .on_drag_move::<DragTab>({
                    let workspace = workspace.clone();
                    move |event, _, cx| {
                        let pointer = f32::from(event.event.position.x);
                        _ = workspace.update(cx, |workspace, cx| {
                            workspace.move_tab_drag(pointer, cx);
                        });
                    }
                })
                .on_mouse_up(gpui::MouseButton::Left, {
                    let workspace = workspace.clone();
                    move |_, _, cx| {
                        _ = workspace.update(cx, |workspace, cx| workspace.end_tab_drag(cx));
                    }
                })
                .on_mouse_up_out(gpui::MouseButton::Left, {
                    let workspace = workspace.clone();
                    move |_, _, cx| {
                        _ = workspace.update(cx, |workspace, cx| workspace.end_tab_drag(cx));
                    }
                })
                .children(tabs),
        )
        .child(
            // Outside the scrolling strip and shrink-proof, so a strip full
            // enough to scroll never scrolls this out of reach with it.
            div().flex_shrink_0().child(
                icon_button(
                    "new-query-tab",
                    icon::PLUS,
                    Tone::Quiet,
                    Control::Compact,
                    t,
                )
                .tooltip_with_action("New query", &NewQuery, None)
                .on_click(move |_, window, cx| {
                    _ = new_workspace.update(cx, |workspace, cx| {
                        workspace.new_query(&NewQuery, window, cx);
                    });
                }),
            ),
        )
        .children(structure_toggle)
        .children(plan_toggle)
        .children(new_row)
        // 100% is not information; the readout appears only once the zoom
        // has somewhere to return to.
        .children((runnable && zoom != 100).then(|| {
            div()
                .flex_shrink_0()
                .text_color(t.text_faint)
                .child(format!("{zoom}% · {} resets", keycap_text("secondary-0")))
        }))
        .children(naming)
        // A named query is already written to disk on every swap, so there
        // is nothing for a save button to do that has not been done. What
        // it can still do is change the name.
        .children((runnable && !session.naming && named).then(|| {
            icon_button(
                "rename-query",
                icon::RENAME,
                Tone::Quiet,
                Control::Compact,
                t,
            )
            .tooltip("Rename query")
            .on_click(move |_, window, cx| {
                _ = rename_workspace.update(cx, |workspace, cx| {
                    workspace.rename_query(window, cx);
                });
            })
        }))
        .children((runnable && !session.naming && !named).then(|| {
            icon_button("save-query", icon::SAVE, Tone::Quiet, Control::Compact, t)
                .tooltip_with_action("Save query", &SaveQuery, None)
                .on_click(move |_, window, cx| {
                    _ = save_workspace.update(cx, |workspace, cx| {
                        workspace.save_query(&SaveQuery, window, cx);
                    });
                })
        }))
        // Beside Run, because it asks about the same statement Run would run.
        // A menu rather than a button: the two modes differ by whether the
        // statement is executed, and a single button would have to pick one of
        // those on the user's behalf. Absent on an engine with no mode at all,
        // rather than a menu with nothing in it.
        .children(
            (runnable
                && !session.naming
                && ExplainMode::ALL
                    .into_iter()
                    .any(|mode| engine.explain_prefix(mode).is_some()))
            .then(|| {
                icon_button(
                    "explain-query",
                    icon::PLAN,
                    Tone::Quiet,
                    Control::Compact,
                    t,
                )
                .tooltip_with_action(
                    "Explain",
                    &ExplainQuery {
                        mode: ExplainMode::Plan,
                    },
                    None,
                )
                .dropdown_menu(move |menu, _, _| {
                    ExplainMode::ALL
                        .into_iter()
                        // A mode the engine does not have is not offered, the
                        // same way a filter operator it cannot express is not.
                        .filter(|mode| engine.explain_prefix(*mode).is_some())
                        .fold(menu, |menu, mode| {
                            menu.menu(
                                format!("{} — {}", mode.label(), mode.caption()),
                                Box::new(ExplainQuery { mode }),
                            )
                        })
                })
            }),
        )
        .children(runnable.then(|| {
            // Filled where its neighbours are ghosts: running the buffer is
            // what the surface is for, and the fill is the only hierarchy
            // available without spending a colour on it.
            icon_button("run-query", icon::RUN, Tone::Primary, Control::Compact, t)
                .tooltip_with_action("Run", &RunQuery, None)
                .on_click(move |_, window, cx| {
                    _ = run_workspace.update(cx, |workspace, cx| {
                        workspace.run_query(&RunQuery, window, cx);
                    });
                })
        }))
        .into_any_element()
}

/// The app-wide settings, on the card every other modal is drawn on.
///
/// There is no Cancel and no OK. Every control here calls the same method the
/// keystroke or the palette row calls, and each of those has already written
/// the change to `profiles.toml` by the time this repaints — so Cancel would
/// have to undo a file, and Done only takes the card away.
pub fn render_settings(workspace: &Workspace, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let tab = workspace.settings_tab;
    let tabs = div()
        .flex()
        .gap(px(layout::SPACE_XS))
        .child(settings_chip(
            "settings-tab-general",
            "General",
            tab == SettingsTab::General,
            cx,
            |workspace, _, cx| workspace.set_settings_tab(SettingsTab::General, cx),
        ))
        .child(settings_chip(
            "settings-tab-keybindings",
            "Keybindings",
            tab == SettingsTab::Keybindings,
            cx,
            |workspace, _, cx| workspace.set_settings_tab(SettingsTab::Keybindings, cx),
        ));

    let body = match tab {
        SettingsTab::General => render_general_settings(workspace, cx),
        SettingsTab::Keybindings => render_keybindings_settings(workspace, cx),
    };

    div()
        .id("settings-modal")
        .absolute()
        .inset_0()
        // The modal takes the mouse as well as the keyboard: without this the
        // card floats over a workspace whose buttons still click through.
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .child(
            dialog(t)
                .child(section_label(t, "Settings"))
                .child(tabs)
                .child(body)
                .child(div().flex().justify_end().child(
                    button("settings-done", "Done", Tone::Primary, Control::Standard, t).on_click(
                        cx.listener(|workspace, _: &ClickEvent, window, cx| {
                            workspace.close_settings(window, cx);
                        }),
                    ),
                )),
        )
        .into_any_element()
}

/// The Theme / Editor zoom / Fonts / Default limit sections -- unchanged from
/// before the Keybindings tab existed, just no longer the whole modal.
fn render_general_settings(workspace: &Workspace, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let families = fonts(cx).clone();
    let font_size = workspace.settings.editor_font_size;
    let preview_rows = workspace.settings.preview_rows;
    let check_for_updates = workspace.settings.check_for_updates;
    let color_titlebar = workspace.settings.color_titlebar;

    // Like the font rows below: the palette lists the themes, and moving
    // through it previews each one on this card.
    let theme_picker = settings_chip("theme", t.name, true, cx, |workspace, window, cx| {
        workspace.open_palette(PaletteMode::Theme, window, cx);
    });

    // Disabled at the ends rather than clamped again here: `adjust_editor_zoom`
    // already refuses to go past them, and a button that looks live and does
    // nothing is worse than one that says it cannot.
    let zoom = div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .child(
            button("zoom-out", "−", Tone::Quiet, Control::Compact, t)
                .disabled(font_size <= EDITOR_FONT_SIZE_MIN)
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.zoom_editor_out(&ZoomEditorOut, window, cx);
                })),
        )
        .child(
            div()
                .min_w(px(40.))
                .text_size(px(layout::TEXT_SM))
                .child(format!("{}%", editor_zoom_percent(font_size))),
        )
        .child(
            button("zoom-in", "+", Tone::Quiet, Control::Compact, t)
                .disabled(font_size >= EDITOR_FONT_SIZE_MAX)
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.zoom_editor_in(&ZoomEditorIn, window, cx);
                })),
        )
        .child(
            button("zoom-reset", "Reset", Tone::Quiet, Control::Compact, t).on_click(cx.listener(
                |workspace, _: &ClickEvent, window, cx| {
                    workspace.reset_editor_zoom(&ResetEditorZoom, window, cx);
                },
            )),
        );

    // Disabled wholesale on an opaque theme rather than hidden: the setting is
    // still remembered, it is just that a theme which paints its own chrome
    // has no desktop behind it for this to let through.
    let opacity = workspace.settings.opacity;
    let transparency = div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .child(
            button("opacity-down", "−", Tone::Quiet, Control::Compact, t)
                .disabled(!t.is_glass || opacity <= OPACITY_MIN)
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.step_opacity(-OPACITY_STEP, window, cx);
                })),
        )
        .child(
            // The sign sits beside the field rather than in it. `suffix` lays
            // out inside the width declared here and pads again on its own, so
            // a small input carrying one leaves under twenty points for the
            // text: at 56 the committed 95 rendered as a clipped 9 and a 5.
            // This is the readout's own width instead -- three typed digits
            // and the caret, since 100 is reachable on the way to a value that
            // clamps, inside the small size's 8-point padding.
            Input::new(&workspace.opacity_input)
                .small()
                .w(px(52.))
                .disabled(!t.is_glass),
        )
        .child(
            div()
                .text_size(px(layout::TEXT_SM))
                .text_color(t.text_faint)
                .child("%"),
        )
        .child(
            button("opacity-up", "+", Tone::Quiet, Control::Compact, t)
                .disabled(!t.is_glass || opacity >= OPACITY_MAX)
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.step_opacity(OPACITY_STEP, window, cx);
                })),
        )
        .child(
            button("opacity-reset", "Reset", Tone::Quiet, Control::Compact, t)
                .disabled(!t.is_glass)
                .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
                    workspace.set_opacity(OPACITY_DEFAULT, window, cx);
                })),
        );

    // The palette rather than a dropdown of our own: it already lists every
    // family the text system resolved and marks the one in use. It opens over
    // this card and leaves it standing, so a pick lands back here.
    let font_rows: Vec<AnyElement> = [
        ("Chrome", FontSlot::Chrome),
        ("Editor", FontSlot::Editor),
        ("Grid", FontSlot::Grid),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (label, slot))| {
        let family = families.family(slot).clone();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(layout::SPACE_MD))
            .child(
                div()
                    .text_size(px(layout::TEXT_SM))
                    .text_color(t.text_muted)
                    .child(label),
            )
            .child(settings_chip(
                ("font", index),
                family,
                true,
                cx,
                move |workspace, window, cx| {
                    workspace.open_palette(PaletteMode::Font(slot), window, cx);
                },
            ))
            .into_any_element()
    })
    .collect();

    let limits: Vec<AnyElement> = ROW_LIMITS
        .into_iter()
        .map(|rows| {
            settings_chip(
                ("preview-rows", rows),
                compact_count(rows),
                rows == preview_rows,
                cx,
                move |workspace, _, cx| workspace.set_preview_rows(rows, cx),
            )
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_MD))
        .child(settings_section(
            t,
            "Theme",
            div().flex().child(theme_picker),
        ))
        .child(settings_section(
            t,
            "Opacity",
            div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(transparency)
                .child(
                    div()
                        .text_size(px(layout::TEXT_XS))
                        .text_color(t.text_faint)
                        .child(
                            "Everything else is relative to this. The chrome, \
                             the editor and the results grid are tints over \
                             the frost set here, so they move with it rather \
                             than being set apiece.",
                        ),
                ),
        ))
        .child(settings_section(t, "Editor zoom", zoom))
        .child(settings_section(
            t,
            "Fonts",
            div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .children(font_rows),
        ))
        .child(settings_section(
            t,
            "Default limit",
            div().flex().gap(px(layout::SPACE_XS)).children(limits),
        ))
        .child(settings_section(
            t,
            "Check for updates",
            div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(div().flex().gap(px(layout::SPACE_XS)).children(
                    [(true, "On"), (false, "Off")].map(|(check, label)| {
                        settings_chip(
                            ("check-for-updates", check as usize),
                            label,
                            check == check_for_updates,
                            cx,
                            move |workspace, _, cx| workspace.set_check_for_updates(check, cx),
                        )
                    }),
                ))
                .child(
                    div()
                        .text_size(px(layout::TEXT_XS))
                        .text_color(t.text_faint)
                        .child(
                            "One request to GitHub at launch to see whether a \
                             newer release exists. Nothing is sent about you \
                             or your databases.",
                        ),
                ),
        ))
        .child(settings_section(
            t,
            "Color the titlebar",
            div()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(div().flex().gap(px(layout::SPACE_XS)).children(
                    [(true, "On"), (false, "Off")].map(|(color, label)| {
                        settings_chip(
                            ("color-titlebar", color as usize),
                            label,
                            color == color_titlebar,
                            cx,
                            move |workspace, _, cx| workspace.set_color_titlebar(color, cx),
                        )
                    }),
                ))
                .child(
                    div()
                        .text_size(px(layout::TEXT_XS))
                        .text_color(t.text_faint)
                        .child("Paint the titlebar in the connection's color."),
                ),
        ))
        .into_any_element()
}

/// Every rebindable action, in registry order, with its current chord and
/// the controls to change it.
fn render_keybindings_settings(workspace: &Workspace, cx: &mut Context<Workspace>) -> AnyElement {
    let overrides = &workspace.settings.custom_keybindings;
    let rebinding = workspace.rebinding;
    let rows: Vec<AnyElement> = keybindings::REGISTRY
        .iter()
        .map(|spec| render_keybinding_row(spec, overrides, rebinding, cx))
        .collect();

    div()
        .id("keybindings-list")
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_XS))
        .max_h(px(360.))
        .overflow_y_scroll()
        .smooth_scroll(&smooth("keybindings-list", cx))
        .children(rows)
        .into_any_element()
}

/// What is bound to `spec` today, as the keycaps the rest of the app draws a
/// shortcut with -- a chord is a chord whether the hint sits in a panel or in
/// this list. Multi-stroke chords get a cap each, in order.
fn chord_caps(
    spec: &keybindings::KeybindingSpec,
    overrides: &std::collections::HashMap<String, String>,
    t: Theme,
) -> AnyElement {
    let chords = keybindings::chords_for(spec, overrides);
    if chords.is_empty() {
        return div()
            .text_size(px(layout::TEXT_XS))
            .text_color(t.text_faint)
            .child("Unbound")
            .into_any_element();
    }
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .children(
            chords
                .iter()
                .flat_map(|chord| chord.split_whitespace())
                .filter_map(keycap_for),
        )
        .into_any_element()
}

/// One action: its label, its current chord, and either an Edit/Reset pair
/// or -- while it is the row [`Workspace::rebinding`] names -- the prompt for
/// the next keystroke. The keystroke itself is taken by the interceptor
/// `Workspace::new` installs; nothing here listens for keys.
fn render_keybinding_row(
    spec: &'static keybindings::KeybindingSpec,
    overrides: &std::collections::HashMap<String, String>,
    rebinding: Option<&'static str>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let id = spec.id;
    let has_override = overrides.contains_key(id);

    let trailing = if rebinding == Some(id) {
        div()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_SM))
            .text_size(px(layout::TEXT_XS))
            .text_color(t.text_faint)
            .child("Press any key… (Esc to cancel)")
            .into_any_element()
    } else {
        div()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_SM))
            .child(chord_caps(spec, overrides, t))
            .child(
                button(
                    SharedString::from(format!("keybind-edit-{id}")),
                    "Edit",
                    Tone::Quiet,
                    Control::Compact,
                    t,
                )
                .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                    workspace.start_rebind(id, cx);
                })),
            )
            .children(has_override.then(|| {
                button(
                    SharedString::from(format!("keybind-reset-{id}")),
                    "Reset",
                    Tone::Quiet,
                    Control::Compact,
                    t,
                )
                .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                    workspace.reset_keybinding(id, cx);
                }))
            }))
            .into_any_element()
    };

    div()
        .id(SharedString::from(format!("keybind-row-{id}")))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(layout::SPACE_MD))
        .child(
            div()
                .text_size(px(layout::TEXT_SM))
                .text_color(t.text)
                .child(spec.label),
        )
        .child(trailing)
        .into_any_element()
}

/// One setting: its label over whatever sets it.
fn settings_section(t: Theme, label: &str, controls: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_XS))
        .child(section_label(t, label))
        .child(controls)
}

/// One choice in the settings modal, in the row-limit chips' clothes: a handful
/// of values, all of them on screen, the one in force filled in.
fn settings_chip(
    id: impl Into<gpui::ElementId>,
    label: impl Into<gpui::SharedString>,
    selected: bool,
    cx: &mut Context<Workspace>,
    apply: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> AnyElement {
    let t = *theme(cx);
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(24.))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::TEXT_SM))
        .whitespace_nowrap()
        .map(|chip| {
            if selected {
                chip.bg(t.element_active).text_color(t.text)
            } else {
                chip.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(label.into())
        .on_click(cx.listener(move |workspace, _: &ClickEvent, window, cx| {
            apply(workspace, window, cx);
        }))
        .into_any_element()
}
