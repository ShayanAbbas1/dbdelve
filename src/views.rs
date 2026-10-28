//! The main pane: what the tab in front is showing.
//!
//! Every function here was a `Workspace` associated function that never touched
//! `self` — a pure function of the profile it draws and the theme it reads. They
//! moved out whole; nothing changed but the indentation.
//!
//! `render_main_content` is the only way in. Everything else is a part of the
//! surface it assembles, which is why the rest of the module is private.

use gpui::{
    Animation, AnimationExt, AnyElement, App, AppContext, ClickEvent, ClipboardItem, Context, Div,
    Entity, Focusable, FontWeight, HighlightStyle, Hsla, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, RenderOnce, SharedString, Stateful,
    StatefulInteractiveElement, Styled, StyledText, Window, div, prelude::FluentBuilder, px,
};
use gpui_base::{
    SelectableText,
    actions::{SelectNextColumn, SelectPrevColumn},
};
use gpui_component::{
    ActiveTheme, Disableable, ElementExt, FocusableExt, IconName, RopeExt, Sizable, ThemeStyled,
    button::Button,
    highlighter::{HighlightTheme, SyntaxHighlighter},
    input::{self, Editor, EditorState, Input, InputState, Rope, Textarea, TextareaState},
    menu::DropdownMenu,
    resizable::{ResizableState, h_resizable, resizable_panel, v_resizable},
    spinner::Spinner,
    table::{DataTable, TableDelegate, TableState},
};

use crate::{
    InsertForm, TabKey, Workspace,
    actions::{
        AddFilter, CancelQuery, CopyCell, CustomRowLimit, ExplainQuery, FormatQuery, LimitTarget,
        NewQuery, NewRow, NextPage, PreviousPage, RemoveFilter, RunQuery, SaveQuery,
        SetDefaultRowLimit, SetFilterColumn, SetFilterOperator, SetFilterRaw, SetQueryLimit,
        SetRowLimit, ToggleComment, ToggleFilterJoin, ToggleNextJoin, ToggleRowPanel,
    },
    db,
    db::{Engine, ExplainMode, RoutineKind},
    explain::{self, LabelPart},
    explorer::ROW_LIMITS,
    filter::{Conjunction, FilterRow, Operator},
    icons::icon,
    palette::Command,
    result_grid,
    result_grid::{NewValue, ResultGrid},
    scroller::{SmoothScrollable, scroll_to, smooth, smooth_for, smooth_scoped},
    session::{
        CloseTarget, Explained, ObjectBody, ObjectTab, Profile, ProfileState, QueryState, QueryTab,
        Said, Session, StructureState, Tab, query_label, result_pane_is_expanded,
    },
    structure_grid::{Section, StructureGrid},
    tab_drag::{DragTab, TabStrip},
    theme::{Theme, fonts, layout, theme},
    ui::{
        Code, Control, Tone, button, button_label, code_highlights, compact_count, group_thousands,
        icon_button, key_hint, keycap_text, kind_color, object_icon, reconnect_button, row_icon,
        section_label,
    },
    workspace::error_in_buffer,
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
    /// Narrows the fields to those whose column name or value contains it.
    /// One field for the window, showing the filter of the tab in front
    /// (`row_panel_filter`), so one tab's filter never hides another's
    /// columns. Kept across rows: a column found by name stays found while the
    /// selection moves; one found by value comes and goes with the rows that
    /// hold it.
    pub search: Entity<InputState>,
    /// Whether the last frame drew `search`, and with it Edit mode's inputs,
    /// cleared and set as `on_screen` is. A field unmounted while focused
    /// keeps the window's focus, and with it every keybinding; render hands
    /// focus back when this comes up false.
    pub searched: std::cell::Cell<bool>,
    /// Edit mode's inputs for the selected row, built before the frame by
    /// `sync_row_panel_inputs`, which has the window an input needs.
    pub inputs: Option<RowInputs>,
    /// Put the keyboard in an input once Edit mode has built them, so turning
    /// it on from the keyboard leaves the hands there.
    pub focus_input: bool,
    /// The column whose input has the keyboard, kept through the filter so a
    /// value typed out of the filter's reach does not take the focused input
    /// off screen with it.
    pub typing: Option<usize>,
    /// The tab and grid cell the panel last scrolled to, so it scrolls once
    /// per click or arrow key and then leaves the panel to the wheel. Cleared
    /// whenever the panel is folded or off screen, so reopening it scrolls
    /// again.
    pub revealed: std::cell::RefCell<Option<(String, usize, usize)>>,
}

/// One row's inputs, rebuilt whenever the grid or the row they were made for
/// is no longer the one selected.
pub struct RowInputs {
    pub grid: Entity<TableState<ResultGrid>>,
    /// Whose grid `grid` is. A blur from switching tabs or profiles commits
    /// after the switch, when the one in front is no longer the owner.
    pub profile: String,
    pub tab: Tab,
    pub row: usize,
    /// The grid's `rows_id` when these were built: a refresh or a sort puts
    /// another record at `row`, and text typed against the old one must not
    /// land on it.
    pub rows: u64,
    /// One slot per column, empty where the cell cannot be edited.
    pub fields: Vec<Option<RowInput>>,
}

impl RowInputs {
    /// Whether these are still the inputs for `row` of `results` as it holds
    /// its rows now.
    pub fn name(
        &self,
        results: &Entity<TableState<ResultGrid>>,
        row: usize,
        cx: &gpui::App,
    ) -> bool {
        self.grid == *results
            && self.row == row
            && self.rows == results.read(cx).delegate().rows_id()
    }
}

pub struct RowInput {
    pub input: Entity<TextareaState>,
    /// What the input was last filled with, so only text the user changed is
    /// staged, and a cell changed elsewhere refills an input nobody is typing
    /// in.
    pub seed: SharedString,
}

#[allow(clippy::too_many_arguments)]
pub fn render_main_content(
    profile: &Profile,
    editor_font_size: f32,
    zoom: (u32, u32, u32),
    row_panel: &RowPanel,
    plan_copied: bool,
    strip: &TabStrip,
    custom_limit: Option<&(LimitTarget, Entity<InputState>)>,
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
        None => render_query_surface(
            profile,
            editor_font_size,
            row_panel,
            plan_copied,
            custom_limit,
            cx,
        ),
    };

    div()
        .size_full()
        .flex()
        .flex_col()
        // Chrome, so the strip reads as the frame the surfaces sit in --
        // and chrome is the frost, which is already painted beneath it.
        .child(render_tab_strip(profile, zoom, strip, cx))
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
    toolbar: AnyElement,
    bottom: AnyElement,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let code = fonts(cx).editor.clone();

    // The editor is the prompt, one tone behind its results -- and one step
    // more transparent, since it is also one step further from the data.
    let buffer = div()
        .key_context("Editor")
        .flex_1()
        .min_h_0()
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
                        .menu("Toggle Line Comment", Box::new(ToggleComment))
                }),
        );
    let top = div()
        .size_full()
        .flex()
        .flex_col()
        .child(buffer)
        .child(toolbar);

    let expanded = result_pane_is_expanded(query);
    let (editor_height, results_height) = if expanded {
        (
            layout::EDITOR_DEFAULT_HEIGHT,
            layout::RESULTS_DEFAULT_HEIGHT,
        )
    } else {
        (layout::EDITOR_EMPTY_HEIGHT, layout::RESULTS_EMPTY_HEIGHT)
    };

    // The panel holds the toolbar as well, so its bounds grow by the bar and
    // the buffer keeps the room the constants were tuned for.
    let bar = layout::chrome(layout::TAB_HEIGHT);
    v_resizable((split, if expanded { "expanded" } else { "compact" }))
        .child(
            resizable_panel()
                .size(px(editor_height + bar))
                .size_range(
                    px(layout::EDITOR_MIN_HEIGHT + bar)..px(layout::EDITOR_MAX_HEIGHT + bar),
                )
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
    custom_limit: Option<&(LimitTarget, Entity<InputState>)>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let Some(tab) = profile.session.active_query_tab() else {
        let t = *theme(cx);
        return div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(layout::chrome(layout::TEXT_SM)))
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
    let bottom = match tab.plan_shown() {
        // Under the same switcher as the rows, or a typed `EXPLAIN`, which has
        // no Data to flip to, would hide what its statement raised.
        Some(explained) => {
            let plan = render_plan(explained, plan_copied, &scope, cx);
            match result_switcher(tab, cx) {
                None => plan,
                Some(switcher) => div()
                    .size_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(switcher)
                    .child(div().flex_1().min_h_0().child(plan))
                    .into_any_element(),
            }
        }
        None => render_results(
            profile.config.engine(),
            matches!(profile.state, ProfileState::Failed(_)),
            shown_state,
            shown_grid,
            Some(tab),
            tab.row_panel_folded,
            tab.row_panel_editing,
            &tab.row_panel_split,
            row_panel,
            profile
                .session
                .insert_form
                .as_ref()
                .filter(|form| form.tab == Tab::Query(tab.id)),
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
        render_query_toolbar(&profile.session, profile.config.engine(), custom_limit, cx),
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
    let syntax = cx.theme().highlight_theme.clone();
    let Explained {
        plan, mode, sql, ..
    } = explained;
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
                            .child(plan_label(&node.label, &t)),
                    )
                    .children(node.detail.iter().map(|line| {
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_XS)))
                            .text_color(t.text_muted)
                            .child(plan_detail(line, node, &t, &syntax))
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(layout::SPACE_MD))
                            .text_size(px(layout::chrome(layout::TEXT_XS)))
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
                            .children(plan.misestimate(index).map(|factor| {
                                div().text_color(t.danger).child(format!(
                                    "×{} misestimate",
                                    group_thousands(factor.round() as u64)
                                ))
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
        .text_size(px(layout::chrome(layout::TEXT_SM)))
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
                .h(px(layout::chrome(layout::TAB_HEIGHT)))
                .border_b_1()
                .border_color(t.border)
                .child(
                    icon(icon::PLAN)
                        .size(px(layout::chrome(layout::ICON_SIZE)))
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
                        .text_size(px(layout::chrome(layout::TEXT_XS)))
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
                                .size(px(layout::chrome(layout::ICON_SIZE)))
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

fn tint(color: impl Into<Hsla>) -> HighlightStyle {
    HighlightStyle {
        color: Some(color.into()),
        ..HighlightStyle::default()
    }
}

/// A node's label with what it reads, and through which index, told apart
/// from the operator. The operator itself keeps the row's own colour, so the
/// slowest node's stays red.
fn plan_label(label: &str, t: &Theme) -> StyledText {
    let label = clip_label(label);
    let highlights = explain::label_parts(&label)
        .into_iter()
        .filter_map(|(range, part)| {
            let style = match part {
                LabelPart::Operator => return None,
                LabelPart::Relation => tint(t.syntax_type),
                LabelPart::Index => tint(t.syntax_function),
                LabelPart::Keyword => tint(t.text_faint),
                LabelPart::Aside => tint(t.text_muted),
            };
            Some((range, style))
        })
        .collect::<Vec<_>>();
    StyledText::new(label).with_highlights(highlights)
}

/// A qualifier line: its key quiet, a condition highlighted as SQL, numbers
/// picked out of anything else, and a filter that threw away far more than it
/// kept in the warning colour.
fn plan_detail(
    line: &str,
    node: &explain::PlanNode,
    t: &Theme,
    syntax: &HighlightTheme,
) -> StyledText {
    let wasteful = explain::wasteful(line, node.actual);
    let line = clip_label(line);
    let (key, value_start) = match line.split_once(": ") {
        Some((key, _)) => (key, key.len() + 2),
        None => ("", 0),
    };
    let value = &line[value_start..];
    let shifted =
        |range: std::ops::Range<usize>| range.start + value_start..range.end + value_start;

    let mut highlights = vec![(0..key.len(), tint(t.text_faint))];
    if wasteful {
        highlights.push((value_start..line.len(), tint(t.danger)));
    } else if explain::is_expression(key) {
        highlights.extend(
            code_highlights(value, db::Syntax::Sql, Code::Expression, syntax)
                .into_iter()
                .map(|(range, style)| (shifted(range), style)),
        );
    } else {
        highlights.extend(
            explain::numbers(value)
                .into_iter()
                .map(|range| (shifted(range), tint(t.syntax_number))),
        );
    }
    highlights.retain(|(range, _)| !range.is_empty());
    StyledText::new(line).with_highlights(highlights)
}

/// A statement on one line, for the header strip that says what was explained.
fn one_line(sql: &str) -> String {
    clip_label(&crate::ui::one_line(sql))
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
    if let ObjectBody::Structure {
        structure, grid, ..
    } = &tab.body
    {
        // No New row here: the form belongs to the rows, in the other tab.
        return div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                control_row()
                    .border_b_1()
                    .border_color(t.border)
                    .child(div().flex_1())
                    .child(relation_controls(true, false, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .bg(t.data_glass())
                    .child(render_structure(structure, grid, &scope, cx)),
            )
            .into_any_element();
    }
    let ObjectBody::Relation {
        results,
        query,
        filters,
        next_join,
        row_panel_folded,
        row_panel_editing,
        row_panel_split,
        ..
    } = &tab.body
    else {
        return render_routine(tab, &scope, cx);
    };

    let takes_inserts = tab.takes_inserts(engine);
    let controls = relation_controls(false, takes_inserts, cx).into_any_element();
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(render_filter_bar(
            filters,
            results.read(cx).delegate().columns(),
            engine,
            *next_join,
            controls,
            t,
        ))
        .child(div().flex_1().min_h_0().child(render_results(
            engine,
            disconnected,
            query,
            results,
            None,
            *row_panel_folded,
            *row_panel_editing,
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
    controls: AnyElement,
    t: Theme,
) -> AnyElement {
    let names: Vec<SharedString> = columns
        .iter()
        .filter(|column| crate::filter::filters_on(engine, &column.name))
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
            control_row()
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
                            .size(px(layout::chrome(layout::ICON_SIZE)))
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
                            .size(px(layout::chrome(layout::ICON_SIZE)))
                            .text_color(t.text_faint),
                    )
                    .dropdown_menu({
                        let chosen = filter.operator;
                        let column = filter.column.clone();
                        move |menu, _, _| {
                            Operator::ALL
                                .into_iter()
                                // An operator the engine cannot express is not
                                // offered: SQLite has no regex (spec §7).
                                .filter(|operator| operator.offered(engine, column.as_deref()))
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
                    (filter.raw || filter.operator.takes_value()).then(|| FilterValue {
                        editor: filter.value.clone(),
                        raw: filter.raw,
                    }),
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
            control_row()
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
                )
                .child(div().flex_1())
                .child(controls),
        )
        .into_any_element()
}

/// `AND` or `OR`, as the two-state button it is. A dropdown of two rows is a
/// menu to open for something a click already says.
fn join_button(id: impl Into<gpui::ElementId>, conjunction: Conjunction, t: Theme) -> Button {
    button(id, conjunction.as_str(), Tone::Quiet, Control::Compact, t).tooltip("AND or OR")
}

/// A filter bar's value box: a code editor, dressed as the small one-line
/// input it sits among.
///
/// The editor reserves two rows whatever it holds, so the box is cut to one
/// and clips the other; the completion popup is drawn deferred, outside the
/// clip. The library draws no focus ring on an editor, so this draws the one
/// an input would have.
#[derive(IntoElement)]
struct FilterValue {
    editor: Entity<EditorState>,
    raw: bool,
}

impl RenderOnce for FilterValue {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focused = self.editor.read(cx).focus_handle(cx).is_focused(window);
        let font = match self.raw {
            true => fonts(cx).editor.clone(),
            false => fonts(cx).chrome.clone(),
        };
        // A small input's height. The extra-small size puts no inset above
        // the text, so a line as tall as the space inside the border centres.
        let height = window.rem_size() * 1.5;
        let editor = self.editor.clone();
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .rounded(cx.theme().radius)
            // Plain Enter applies the bar; Shift+Enter would still break the
            // line, onto the row the box clips.
            .capture_action(|enter: &input::Enter, _, cx| {
                if enter.shift {
                    cx.stop_propagation();
                }
            })
            .when(focused, |this| this.focus_ring_style(window, cx))
            .child(
                Editor::new(&self.editor)
                    .with_size(gpui_component::Size::XSmall)
                    .h(height)
                    .overflow_hidden()
                    .bg(cx.theme().input_background())
                    .when(focused, |this| this.border_color(cx.theme().ring))
                    .font_family(font)
                    .text_sm()
                    .line_height(height - px(2.))
                    .on_paste(move |item, window, cx| {
                        let Some(text) = item.text().as_deref().and_then(crate::filter::one_line)
                        else {
                            return false;
                        };
                        editor.update(cx, |editor, cx| editor.replace(text, window, cx));
                        true
                    })
                    // The query editor's menu, less what a filter has no use for.
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
                    }),
            )
    }
}

/// One line of controls, at the height every other control strip is.
fn control_row() -> gpui::Div {
    div()
        .w_full()
        .h(px(layout::chrome(layout::TAB_HEIGHT)))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .px(px(layout::SPACE_SM))
}

/// The "Insert" form (spec §4), in the place the row panel takes beside the
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
                                .text_size(px(layout::chrome(layout::TEXT_XS)))
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
        .child(section_label(t, "Insert"))
        // The one line that says what an empty field means, because the
        // three-way rule is invisible otherwise.
        .child(
            div()
                .text_size(px(layout::chrome(layout::TEXT_SM)))
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
                    .on_click(move |_, window, cx| {
                        _ = review_workspace.update(cx, |workspace, cx| {
                            workspace.confirm_new_row(window, cx);
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
                        .text_size(px(layout::chrome(layout::TEXT_LG)))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{}.{}", tab.schema, tab.name)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(layout::SPACE_LG))
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(t.text_muted)
                        .child(kind)
                        .child(format!("Language: {}", routine.language))
                        .children(
                            (!routine.result_type.is_empty())
                                .then(|| div().child(format!("Returns: {}", routine.result_type))),
                        ),
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
    editing: bool,
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
            .text_size(px(layout::chrome(layout::TEXT_SM)))
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
    // falls through to the grid, and the status bar says the run is going.
    // The spinner is only for the wait before the first row.
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

    // The run's messages stand in for the selected result, form and inspector
    // included: both describe rows that are not on screen.
    let run_messages = query_tab.filter(|tab| tab.messages_shown()).map(|tab| {
        let said = tab.messages();
        render_messages(&said, None, "run-messages", scope, cx)
    });
    let message = match query {
        _ if run_messages.is_some() => run_messages,
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
            messages,
            ..
        } if *rows == 0 && !has_rows => {
            let line = quiet_line(match rows_affected {
                Some(rows) => format!("Query completed. Server row count: {rows}."),
                None => "Query completed.".into(),
            });
            Some(match messages.is_empty() {
                true => centered(line),
                // Read top to bottom like the log it is, the outcome last,
                // where it would have been printed.
                false => render_messages(
                    &[Said {
                        index: 0,
                        sql: "",
                        messages,
                    }],
                    Some(line),
                    "result-messages",
                    scope,
                    cx,
                ),
            })
        }
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
        .child({
            results.update(cx, result_grid::fit_to_face);
            let rows_scroll = smooth_for(
                "results",
                scope,
                results.read(cx).vertical_scroll_handle.clone(),
                results.read(cx).horizontal_scroll_handle.clone(),
                cx,
            );
            div()
                .id("results")
                .smooth_scroll(&rows_scroll)
                .flex_1()
                .w_full()
                .min_h_0()
                .min_w_0()
                .font_family(grid)
                .text_size(px(layout::grid(layout::BODY_FONT_SIZE)))
                // The grid's own delegate has no key hook and the
                // focused element is the table root, so `enter` is
                // caught here on its way out of the Table context.
                .on_action(cx.listener(Workspace::edit_cell))
                .on_action(cx.listener(Workspace::copy_cell))
                // Captured, so the ring steps before the library's own
                // handler steps from its stale column.
                .capture_action({
                    let results = results.clone();
                    move |_: &SelectNextColumn, _, cx| {
                        results.update(cx, |table, cx| result_grid::step_col(table, 1, cx))
                    }
                })
                .capture_action({
                    let results = results.clone();
                    move |_: &SelectPrevColumn, _, cx| {
                        results.update(cx, |table, cx| result_grid::step_col(table, -1, cx))
                    }
                })
                .on_action(cx.listener(Workspace::hide_column))
                .on_action(cx.listener(Workspace::toggle_pin_column))
                .on_action(cx.listener(Workspace::show_all_columns))
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
                // The library's medium row, held in proportion to the grid's text.
                .child(
                    DataTable::new(results)
                        // The active cell's ring is where the keyboard is; a
                        // second ring around the whole grid says nothing more.
                        .focus_ring(false)
                        .bordered(false)
                        .stripe(false)
                        .with_size(gpui_component::Size::Size(px(layout::grid(
                            gpui_component::Size::Medium.table_row_height().into(),
                        )))),
                )
                .on_prepaint({
                    let (results, rows_scroll) = (results.clone(), rows_scroll.clone());
                    move |_, window, cx| {
                        if let Some(x) = results.update(cx, result_grid::keep_active_in_view) {
                            rows_scroll.glide_across(x, window);
                        }
                    }
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
        _ if query_tab.is_some_and(QueryTab::messages_shown) => None,
        Some(form) => Some((render_new_row_panel(engine, form, scope, cx), false)),
        None => render_row_inspector(results, folded, editing, row_panel, scope, cx)
            .map(|panel| (panel, folded)),
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

/// One chip per statement of a queue, from the moment it starts: those that
/// have run, the one running, and those still to go, above the result the
/// selected chip is showing. Then, once the server has said anything during
/// the run, one for what it said.
///
/// Only from the second chip: one is nothing to switch between. An ordinary
/// run has no queue, and a chip for its one result only to stand beside its
/// messages.
fn result_switcher(tab: &QueryTab, cx: &mut Context<Workspace>) -> Option<AnyElement> {
    let t = *theme(cx);
    let said = tab.messages();
    let messages_shown = tab.showing_messages && !said.is_empty();
    let mut chips: Vec<AnyElement> = match tab.queue.as_ref() {
        Some(queue) => {
            // The statement in flight is the tab's own slot, not one of
            // `done`, so it is the chip one past the end. Selecting it is what
            // following the run means; any other chip reads a result while
            // that one keeps going.
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
                    index == queue.showing && !messages_shown,
                    cx,
                )
            });
            let mut chips: Vec<AnyElement> = queue
                .done
                .iter()
                .enumerate()
                .map(|(index, finished)| {
                    result_chip(
                        tab.id,
                        index,
                        &query_label(&finished.sql),
                        tab.state_of(finished),
                        index == queue.showing && !messages_shown,
                        cx,
                    )
                })
                .chain(running)
                .collect();
            for (offset, range) in queue.remaining.iter().enumerate() {
                let index = queue.done.len() + usize::from(queue.awaiting) + offset;
                let label = queue.sql.get(range.clone()).map(query_label);
                chips.push(result_chip(
                    tab.id,
                    index,
                    &label.unwrap_or_default(),
                    &QueryState::Idle,
                    false,
                    cx,
                ));
            }
            chips
        }
        None if !said.is_empty() => {
            // A restored tab no longer knows which statement it ran.
            let label = said
                .first()
                .map(|said| query_label(said.sql))
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| "Result".into());
            vec![result_chip(
                tab.id,
                0,
                &label,
                &tab.query,
                !messages_shown,
                cx,
            )]
        }
        None => return None,
    };
    if !said.is_empty() {
        let count = said.iter().map(|said| said.messages.len()).sum::<usize>();
        let id = tab.id;
        chips.push(
            switcher_chip("messages-chip", messages_shown, false, t)
                .child("Messages")
                .child(div().text_color(t.text_faint).child(compact_count(count)))
                .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                    workspace.show_messages(Tab::Query(id), cx);
                }))
                .into_any_element(),
        );
    }
    if chips.len() < 2 {
        return None;
    }

    Some(
        div()
            .id("query-results")
            .h(px(layout::chrome(layout::TAB_HEIGHT)))
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

/// A switcher chip's clothes, the row-limit chips': what the result and
/// Messages chips share.
fn switcher_chip(
    id: impl Into<gpui::ElementId>,
    selected: bool,
    queued: bool,
    t: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .h(px(layout::chrome(24.)))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .whitespace_nowrap()
        .map(|chip| {
            if selected {
                chip.bg(t.element_active).text_color(t.text)
            } else if queued {
                // Nothing to show yet, so nothing to offer on hover.
                chip.text_color(t.text_faint)
            } else {
                chip.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
}

/// One statement of a queue: its place in the queue, what it was, and what
/// it came back with.
fn result_chip(
    id: u64,
    index: usize,
    label: &str,
    state: &QueryState,
    selected: bool,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let queued = matches!(state, QueryState::Idle);
    let (readout, tint) = match state {
        QueryState::Idle => (Some("queued".to_string()), t.text_faint),
        QueryState::Running { .. } => (Some("running".to_string()), t.text_muted),
        QueryState::Complete { rows, .. } => (Some(compact_count(*rows)), t.text_faint),
        QueryState::Failed(_) => (Some("failed".to_string()), t.danger),
        _ => (None, t.text_faint),
    };
    switcher_chip(("queued-result", index), selected, queued, t)
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

/// What the server said running a run's statements, as it said it, read top
/// to bottom like the log it is. In the editor's face, as an error is: it
/// quotes the server. Under a heading per statement only when more than one
/// said something, the heading reading as that statement's chip does.
///
/// Each message is plain selectable text, never Markdown: a `PRINT` is the
/// server's words and its `*` and `#` are not formatting.
fn render_messages(
    said: &[Said],
    tail: Option<AnyElement>,
    id: &'static str,
    scope: &str,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let (code, chrome) = (fonts(cx).editor.clone(), fonts(cx).chrome.clone());
    let headed = said.len() > 1;
    // The window's selection is a map of runs, so a copy across several only
    // comes out in reading order, a line apiece, if each says where it sits.
    let mut order: usize = 0;
    let groups: Vec<_> = said
        .iter()
        .map(|said| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .when(headed, |group| {
                    group.child(
                        div()
                            .flex()
                            .gap(px(layout::SPACE_XS))
                            .pb(px(layout::SPACE_XS))
                            .font_family(chrome.clone())
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(t.text_muted)
                            .child(
                                div()
                                    .text_color(t.text_faint)
                                    .child(format!("{}.", said.index + 1)),
                            )
                            .child(query_label(said.sql)),
                    )
                })
                .children(said.messages.iter().map(|message| {
                    order += 1;
                    div().text_color(t.text).child(
                        SelectableText::new(("message", order), message.clone())
                            .document_order(order as u64),
                    )
                }))
        })
        .collect();
    div()
        .id(id)
        .size_full()
        .min_h_0()
        .overflow_y_scroll()
        .smooth_scroll(&smooth_scoped(id, scope, cx))
        .p(px(layout::SPACE_LG))
        .flex()
        .flex_col()
        .gap(px(layout::SPACE_MD))
        .font_family(code)
        // Copy is `Root`'s, which reads the window's selection, and a focused
        // buffer or grid would take the keystroke for a copy of its own
        // first. The window's floor has no copy of its own to take it with.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|workspace, _: &MouseDownEvent, window, cx| {
                workspace.focus.focus(window, cx)
            }),
        )
        .children(groups)
        .children(tail.map(|tail| div().font_family(chrome).child(tail)))
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
    editing: bool,
    row_panel: &RowPanel,
    scope: &str,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let t = *theme(cx);
    let grid = fonts(cx).grid.clone();

    let (row_ix, rows, fields, active, row_editable, chips) = {
        let table = results.read(cx);
        let delegate = table.delegate();
        let row_ix = delegate.selected_row()?;
        // What each editable field offers to stage besides typed text, gated
        // as the cell menu is; `None` for a field Edit mode leaves as text.
        let chips: Vec<Option<Vec<(&'static str, NewValue)>>> = (0..delegate.columns().len())
            .filter(|_| editing)
            .map(|col| {
                delegate.editable(row_ix, col).then(|| {
                    [
                        (delegate.offers_null(col), "NULL", NewValue::Null),
                        (
                            delegate.offers_empty(col),
                            "Empty",
                            NewValue::Value(SharedString::default()),
                        ),
                        (delegate.offers_default(col), "DEFAULT", NewValue::Default),
                    ]
                    .into_iter()
                    .filter_map(|(offered, label, value)| offered.then_some((label, value)))
                    .collect()
                })
            })
            .collect();
        (
            row_ix,
            delegate.rows_count(cx),
            delegate.fields(row_ix),
            delegate.active(),
            delegate.row_editable(row_ix),
            chips,
        )
    };
    let inputs = row_panel
        .inputs
        .as_ref()
        .filter(|inputs| editing && inputs.name(results, row_ix, cx));
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
        row_panel.revealed.take();
        return Some(
            div()
                .h_full()
                .flex_shrink_0()
                .px(px(layout::SPACE_XS))
                .border_l_1()
                .border_color(t.border)
                .child(
                    div()
                        .h(px(layout::chrome(layout::TAB_HEIGHT)))
                        .flex()
                        .items_center()
                        .child(fold("show-row-inspector", "Show the row panel", cx)),
                )
                .into_any_element(),
        );
    }

    let query = row_panel.search.read(cx).value().trim().to_lowercase();
    let fields: Vec<_> = fields
        .into_iter()
        .enumerate()
        .filter(|(col, field)| {
            // Checked before lowercasing anything: a value can be a whole
            // JSON document, and this runs every frame.
            query.is_empty()
                || inputs.is_some() && row_panel.typing == Some(*col)
                || field.name.to_lowercase().contains(query.as_str())
                || field
                    .value
                    .as_ref()
                    .is_some_and(|value| value.to_lowercase().contains(query.as_str()))
        })
        .collect();

    let reveal = active.map(|(row, col)| (scope.to_owned(), row, col));
    if *row_panel.revealed.borrow() != reveal {
        if let Some(ix) = active.and_then(|(_, col)| fields.iter().position(|(c, _)| *c == col)) {
            scroll_to("row-inspector", scope, ix, cx);
        }
        row_panel.revealed.replace(reveal);
    }

    // The inputs in the order they are drawn, each with its place among the
    // scroll's children, for `up` and `down` to step through.
    let order: Vec<(usize, Entity<TextareaState>)> = fields
        .iter()
        .enumerate()
        .filter_map(|(ix, (col, _))| Some((ix, inputs?.fields.get(*col)?.as_ref()?.input.clone())))
        .collect();

    row_panel.searched.set(true);
    let table = results.clone();
    let edit_toggle = row_editable.then(|| {
        icon_button(
            "edit-row-inspector",
            icon::EDIT,
            match editing {
                true => Tone::Primary,
                false => Tone::Quiet,
            },
            Control::Compact,
            t,
        )
        .tooltip(match editing {
            true => "Done editing",
            false => "Edit this row",
        })
        .on_click(cx.listener(|workspace, _, _, cx| {
            workspace.toggle_row_panel_edit(cx);
        }))
    });
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
                    .h(px(layout::chrome(layout::TAB_HEIGHT)))
                    .px(px(layout::SPACE_SM))
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_SM))
                    .child(
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
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
                            .children(edit_toggle)
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
                    .h(px(layout::chrome(layout::TAB_HEIGHT)))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_XS))
                    .px(px(layout::SPACE_SM))
                    .border_y_1()
                    .border_color(t.border)
                    .child(row_icon(t, icon::SEARCH))
                    // Clipped by a box of its own, as the explorer's filter
                    // is: the placeholder is laid out at the input's full width.
                    .child(
                        div().flex_1().min_w_0().overflow_hidden().child(
                            Input::new(&row_panel.search)
                                .w_full()
                                .min_w_0()
                                .appearance(false)
                                .cleanable(true),
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
                    .pb(px(layout::SPACE_SM))
                    .flex()
                    .flex_col()
                    .when(fields.is_empty(), |list| {
                        list.child(
                            div()
                                .p(px(layout::SPACE_SM))
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child("No matching columns"),
                        )
                    })
                    .children(fields.into_iter().enumerate().map(|(ix, (col_ix, field))| {
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
                                            .size(px(layout::chrome(layout::ICON_SIZE)))
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
                        let current = active.is_some_and(|(_, col)| col == col_ix);
                        let input = inputs
                            .and_then(|inputs| inputs.fields.get(col_ix)?.as_ref())
                            .map(|field| field.input.clone());
                        let chips = input
                            .is_some()
                            .then(|| chips.get(col_ix).cloned().flatten())
                            .flatten()
                            .unwrap_or_default();
                        // Which chip names what the cell holds, filled as the
                        // new-row form's chip is: an empty input cannot say
                        // whether it is a NULL, an empty string or a DEFAULT.
                        let held = match (&field.edit, &field.value) {
                            (Some(NewValue::Default), _) => Some(NewValue::Default),
                            (_, None) if !field.missing => Some(NewValue::Null),
                            (_, Some(value)) if value.is_empty() => {
                                Some(NewValue::Value(SharedString::default()))
                            }
                            _ => None,
                        };
                        div()
                            .group(group)
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_XS))
                            .p(px(layout::SPACE_SM))
                            // A hairline under every field, so a name reads as
                            // the heading of the value below it rather than as
                            // the line after the value above.
                            .border_b_1()
                            .border_color(t.border)
                            // The grid's wash for a staged cell wins over the
                            // active field's: the name's accent still says
                            // which one is active.
                            .map(|row| match (field.edit.is_some(), current) {
                                (true, _) => row.bg(t.edited),
                                (false, true) => row.bg(t.element_active),
                                (false, false) => row,
                            })
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
                                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                                            .text_color(match current {
                                                true => t.accent,
                                                false => t.text_muted,
                                            })
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
                                                    .text_size(px(layout::chrome(layout::TEXT_XS)))
                                                    .text_color(t.text_faint)
                                                    .child(data_type)
                                            }))
                                            .children(copy),
                                    ),
                            )
                            .map(|row| match input {
                                Some(input) => {
                                    let at = order.binary_search_by_key(&ix, |(at, _)| *at).ok();
                                    let beside = |by: isize| {
                                        at.and_then(|at| at.checked_add_signed(by))
                                            .and_then(|to| order.get(to).cloned())
                                    };
                                    let (up, down) = (beside(-1), beside(1));
                                    let (up_input, down_input) = (input.clone(), input.clone());
                                    let (up_scope, down_scope) =
                                        (scope.to_string(), scope.to_string());
                                    // Lines as typed, not as wrapped: a long
                                    // value on one line steps off at once.
                                    row.child(
                                        div()
                                            .capture_action(cx.listener(
                                                move |workspace, _: &input::MoveUp, window, cx| {
                                                    if up_input.read(cx).cursor_position().line == 0
                                                    {
                                                        workspace.step_row_input(
                                                            &up_input,
                                                            up.clone(),
                                                            &up_scope,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                },
                                            ))
                                            .capture_action(cx.listener(
                                                move |workspace, _: &input::MoveDown, window, cx| {
                                                    let state = down_input.read(cx);
                                                    if state.cursor_position().line as usize + 1
                                                        >= state.text().lines_len()
                                                    {
                                                        workspace.step_row_input(
                                                            &down_input,
                                                            down.clone(),
                                                            &down_scope,
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                },
                                            ))
                                            .child(
                                                Textarea::new(&input)
                                                    .font_family(grid.clone())
                                                    .text_size(px(layout::grid(layout::TEXT_SM))),
                                            ),
                                    )
                                }
                                None => row.child(
                                    div()
                                        .font_family(grid.clone())
                                        .text_size(px(layout::grid(layout::TEXT_SM)))
                                        .map(|value| match field.value {
                                            Some(text) if field.json => {
                                                value.text_color(t.text).child(
                                                    StyledText::new(text.clone()).with_highlights(
                                                        json_highlights(
                                                            &text,
                                                            &cx.theme().highlight_theme,
                                                        ),
                                                    ),
                                                )
                                            }
                                            Some(text) => value.text_color(t.text).child(text),
                                            None if field.missing => value,
                                            // Italic so a NULL cannot be read
                                            // as the four-letter string.
                                            None => value.text_color(t.text_faint).italic().child(
                                                match field.edit {
                                                    Some(NewValue::Default) => {
                                                        result_grid::DEFAULT_LABEL
                                                    }
                                                    _ => result_grid::NULL_LABEL,
                                                },
                                            ),
                                        }),
                                ),
                            })
                            .when(!chips.is_empty(), |row| {
                                row.child(div().flex().gap(px(layout::SPACE_XS)).children(
                                    chips.into_iter().map(|(label, value)| {
                                        let tone = match held.as_ref() == Some(&value) {
                                            true => Tone::Primary,
                                            false => Tone::Quiet,
                                        };
                                        button((label, col_ix), label, tone, Control::Inline, t)
                                            .on_click(cx.listener(
                                                move |workspace, _, window, cx| {
                                                    workspace.stage_row_field(
                                                        row_ix,
                                                        col_ix,
                                                        value.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ))
                                    }),
                                ))
                            })
                    })),
            )
            .into_any_element(),
    )
}

thread_local! {
    // ponytail: this saves only the query compile. Every repaint still
    // reparses every JSON field, and because one highlighter is shared the
    // same-text skip only hits when a row has a single JSON field. If a
    // release build ever feels slow, compute highlights once per (row, col)
    // next to the reformatted value, or keep one highlighter per column.
    static JSON_HIGHLIGHTER: std::cell::RefCell<SyntaxHighlighter> =
        std::cell::RefCell::new(SyntaxHighlighter::new("json"));
}

fn json_highlights(
    text: &str,
    syntax: &HighlightTheme,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    JSON_HIGHLIGHTER.with_borrow_mut(|highlighter| {
        let new = Rope::from_str(text);
        let old = highlighter.text();
        let point = |rope: &Rope| {
            let end = rope.offset_to_point(rope.len());
            tree_sitter::Point::new(end.row, end.column)
        };
        let edit = tree_sitter::InputEdit {
            start_byte: 0,
            old_end_byte: old.len(),
            new_end_byte: new.len(),
            start_position: tree_sitter::Point::new(0, 0),
            old_end_position: point(old),
            new_end_position: point(&new),
        };
        highlighter.update(Some(edit), &new, None);
        highlighter.styles(&(0..text.len()), syntax)
    })
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
        .h(px(layout::chrome(24.)))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .map(|tab| {
            if selected {
                tab.bg(t.element_active).text_color(t.text)
            } else {
                tab.text_color(t.text_muted)
                    .hover(|style| style.bg(t.element_hover))
            }
        })
        .child(
            icon(path).size(px(layout::chrome(12.))).text_color(
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

/// A row limit, and the menu that changes it: none at all, the sizes on offer,
/// and a number of one's own. While `editing` is the field Custom… opened, it
/// stands where the button was.
pub(crate) fn row_limit_menu(
    target: LimitTarget,
    limit: Option<usize>,
    editing: Option<&Entity<InputState>>,
    t: Theme,
) -> AnyElement {
    if let Some(input) = editing {
        // Dressed as the pager's page field, for the reason given there.
        return div()
            .h(px(layout::chrome(layout::CONTROL_HEIGHT_COMPACT)))
            .w(px(layout::chrome(88.)))
            .px(px(layout::SPACE_SM))
            .flex()
            .items_center()
            .rounded(px(layout::RADIUS_CONTROL))
            .bg(t.element_active)
            .border_1()
            .border_color(t.border_strong)
            .text_size(px(layout::chrome(layout::TEXT_SM)))
            .text_color(t.text)
            .child(
                Input::new(input)
                    .appearance(false)
                    .px_0()
                    .h_full()
                    .text_size(px(layout::chrome(layout::TEXT_SM))),
            )
            .into_any_element();
    }
    let rows_label = |rows: usize| format!("{} rows", group_thousands(rows as u64));
    let label = limit.map_or_else(|| "No limit".to_string(), rows_label);
    let pick = move |rows: Option<usize>| -> Box<dyn gpui::Action> {
        match target {
            LimitTarget::Query(_) => Box::new(SetQueryLimit { rows }),
            LimitTarget::Relation(_) => Box::new(SetRowLimit { rows }),
            LimitTarget::Default => Box::new(SetDefaultRowLimit { rows }),
        }
    };
    let id = match target {
        LimitTarget::Query(id) => gpui::ElementId::from(("query-limit", id)),
        LimitTarget::Relation(id) => ("relation-limit", id).into(),
        LimitTarget::Default => "default-limit".into(),
    };
    button(id, label, Tone::Quiet, Control::Compact, t)
        .child(
            icon(icon::CHEVRON_DOWN)
                .size(px(layout::chrome(layout::ICON_SIZE)))
                .text_color(t.text_faint),
        )
        .dropdown_menu(move |menu, _, _| {
            let menu = menu
                .menu_with_check("No limit", limit.is_none(), pick(None))
                .separator();
            let custom = limit.filter(|rows| !ROW_LIMITS.contains(rows));
            ROW_LIMITS
                .into_iter()
                .fold(menu, |menu, rows| {
                    menu.menu_with_check(rows_label(rows), limit == Some(rows), pick(Some(rows)))
                })
                .separator()
                .menu_with_check(
                    match custom {
                        Some(rows) => format!("Custom ({})…", group_thousands(rows as u64)),
                        None => "Custom…".to_string(),
                    },
                    custom.is_some(),
                    Box::new(CustomRowLimit { target }),
                )
        })
        .into_any_element()
}

fn render_structure(
    state: &StructureState,
    grid: &Entity<TableState<StructureGrid>>,
    scope: &str,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
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

    let shown = grid.read(cx).delegate().section();
    let chips = control_row().border_b_1().border_color(t.border).children(
        Section::ALL
            .into_iter()
            .filter(|section| section.offered(structure))
            .map(|section| {
                let selected = section == shown;
                div()
                    .id(section.label())
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_XS))
                    .h(px(layout::chrome(24.)))
                    .px(px(layout::SPACE_SM))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .map(|chip| match selected {
                        true => chip.bg(t.element_active).text_color(t.text),
                        false => chip
                            .text_color(t.text_muted)
                            .hover(|style| style.bg(t.element_hover)),
                    })
                    .child(section.label())
                    .child(
                        div()
                            .text_color(t.text_faint)
                            .child(section.count(structure).to_string()),
                    )
                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                        workspace.show_structure_section(section, cx);
                    }))
            }),
    );

    let rows_scroll = smooth_for(
        "structure",
        scope,
        grid.read(cx).vertical_scroll_handle.clone(),
        grid.read(cx).horizontal_scroll_handle.clone(),
        cx,
    );
    let copy_from = grid.clone();
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(chips)
        .child(
            div()
                .id("structure")
                .smooth_scroll(&rows_scroll)
                .flex_1()
                .w_full()
                .min_h_0()
                .min_w_0()
                .font_family(fonts(cx).grid.clone())
                .text_size(px(layout::grid(layout::BODY_FONT_SIZE)))
                // The data grid's Copy Cell keystroke, answered from this grid's
                // selection rather than the hidden rows'.
                .on_action(move |_: &CopyCell, _, cx| {
                    if let Some(text) = copy_from.read(cx).delegate().selection_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                })
                .child(
                    DataTable::new(grid)
                        .focus_ring(false)
                        .bordered(false)
                        .stripe(false)
                        .with_size(gpui_component::Size::Size(px(layout::grid(
                            gpui_component::Size::Medium.table_row_height().into(),
                        )))),
                ),
        )
        .into_any_element()
}

/// What the preview in front asked the server for: its limit, its offset, and
/// whether a page may follow. Only while its rows are shown: it is a property
/// of these rows, not of the window.
fn relation_preview(session: &Session) -> Option<(Option<usize>, usize, bool)> {
    session.active_object().and_then(|tab| match &tab.body {
        ObjectBody::Relation {
            limit,
            offset,
            query,
            ..
        } => Some((
            *limit,
            *offset,
            // A full page may have another behind it; a short one is the
            // relation's end. The same gate `turn_page` holds, read here only
            // to decide whether the button is worth drawing.
            matches!(
                query,
                QueryState::Complete { rows, .. } if limit.is_some_and(|limit| *rows >= limit)
            ),
        )),
        _ => None,
    })
}

/// A run in flight, centred in the status bar in place of the pager: how long
/// it has taken, the rows that have streamed in so far, and the way to stop it.
pub(crate) fn render_running(
    query: &QueryState,
    results: Option<&Entity<TableState<ResultGrid>>>,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let QueryState::Running {
        started,
        cancelling,
        ..
    } = query
    else {
        return None;
    };
    let t = *theme(cx);
    let (rows, streaming, filled) = results.map_or((0, false, None), |results| {
        let grid = results.read(cx).delegate();
        (
            grid.rows_count(cx),
            grid.streamed_set().is_some(),
            grid.filled(),
        )
    });
    // Stopped where the limit filled: the rows on screen are all the run will
    // keep, and the time spent stopping or draining the statement after them
    // is not time spent fetching them.
    let elapsed = filled.map_or_else(|| started.elapsed(), |at| at.duration_since(*started));
    let what = match (streaming, rows) {
        // The rows are all in, and the statement is being stopped or drained
        // behind them: still running, but no longer fetching anything shown.
        (true, rows) if filled.is_some() => format!(
            "{} \u{b7} {} \u{b7} finishing\u{2026}",
            crate::ui::row_readout(rows, rows),
            clock(elapsed)
        ),
        (true, rows) => format!("{} fetched", crate::ui::row_readout(rows, rows)),
        // A refresh keeps the rows it is replacing on screen until it lands.
        (false, 1..) => "Refreshing".to_string(),
        (false, 0) => "Running".to_string(),
    };
    // A word rather than an icon: a square or a cross beside a status line
    // reads as "close this". Once the request is out the label is the only
    // acknowledgement the click gets, and the statement is still running, so
    // the button goes inert rather than away.
    let label = match cancelling {
        Some(sent) if sent.elapsed() >= CANCEL_PATIENCE => "Still waiting on server…",
        Some(_) => "Cancelling…",
        None => "Cancel",
    };
    Some(
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_SM))
            .child(
                div()
                    .whitespace_nowrap()
                    .text_color(t.text_muted)
                    .child(match filled {
                        Some(_) => what,
                        None => format!("{what} \u{b7} {}", clock(elapsed)),
                    }),
            )
            .child(
                button("cancel-query", label, Tone::Quiet, Control::Compact, t)
                    .disabled(cancelling.is_some())
                    .on_click(cx.listener(|workspace, _, window, cx| {
                        workspace.cancel_query(&CancelQuery, window, cx);
                    })),
            )
            .into_any_element(),
    )
}

/// The preview's row limit and pager, centred in the status bar: what the
/// relation's rows were asked for, and the way to the ones after them. `None`
/// where there are no rows to page.
pub(crate) fn render_paging(
    profile: &Profile,
    custom_limit: Option<&(LimitTarget, Entity<InputState>)>,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let t = *theme(cx);
    let session = &profile.session;
    let preview = relation_preview(session);
    let row_limit = preview.map(|(limit, _, _)| {
        let target = LimitTarget::Relation(session.active_object().map_or(0, |tab| tab.id));
        let editing = custom_limit
            .filter(|(editing, _)| *editing == target)
            .map(|(_, input)| input);
        row_limit_menu(target, limit, editing, t)
    });
    // The pager appears only once there is somewhere to go: a first page
    // shorter than its limit is the whole relation, and arrows over it are
    // controls that can do nothing. So is one over a statement with no page
    // after its first to ask for.
    let pages = profile.config.engine().pages();
    let pager = preview
        .filter(|_| pages)
        .and_then(|(_, offset, full_page)| {
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
                            .h(px(layout::chrome(layout::CONTROL_HEIGHT_COMPACT)))
                            .w(px(layout::chrome(44.)))
                            .px(px(layout::SPACE_SM))
                            .flex()
                            .items_center()
                            .rounded(px(layout::RADIUS_CONTROL))
                            .bg(t.element_active)
                            // The strong edge is what says "type here": on glass
                            // the wash alone is close to the strip behind it.
                            .border_1()
                            .border_color(t.border_strong)
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(t.text)
                            .child(
                                Input::new(&session.page_input)
                                    .appearance(false)
                                    .px_0()
                                    .h_full()
                                    .text_size(px(layout::chrome(layout::TEXT_SM))),
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

/// How many of the grid's columns are hidden, and the way to get them back.
/// `None` while every column is drawn.
pub(crate) fn render_hidden_columns(
    profile: &Profile,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let hidden = profile
        .session
        .active_results()?
        .read(cx)
        .delegate()
        .hidden_columns();
    (hidden > 0).then(|| {
        let t = *theme(cx);
        button(
            "show-hidden-columns",
            format!(
                "{hidden} hidden {} · Show",
                if hidden == 1 { "column" } else { "columns" }
            ),
            Tone::Quiet,
            Control::Compact,
            t,
        )
        .on_click(cx.listener(|workspace, _: &ClickEvent, window, cx| {
            workspace.show_all_columns(&crate::ShowAllColumns, window, cx);
        }))
        .into_any_element()
    })
}

/// Who orders the view in front, on any grid with columns to click. `None`
/// where there is nothing to sort.
pub(crate) fn render_view_sorting(
    profile: &Profile,
    cx: &mut Context<Workspace>,
) -> Option<AnyElement> {
    let t = *theme(cx);
    let session = &profile.session;
    // Memory is the only choice there, so there is no choice to show.
    if !profile.config.engine().sorts_on_server() {
        return None;
    }
    let client = match session.active {
        Tab::Object(_) => relation_preview(session).and(session.sorting(session.active)),
        Tab::Query(_) => session
            .active_results()
            .filter(|results| !results.read(cx).delegate().columns().is_empty())
            .and(session.sorting(session.active)),
    }
    .map(|sorting| sorting.client_keys().is_some());
    client.map(|client| {
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .child(
                div()
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .text_color(t.text_faint)
                    .child("Sort"),
            )
            .children(
                [(false, "Server"), (true, "Client")].map(|(choice, label)| {
                    choice_chip(
                        ("view-sorting", choice as usize),
                        label,
                        choice == client,
                        cx,
                        move |workspace, window, cx| workspace.set_view_sorting(choice, window, cx),
                    )
                }),
            )
            .into_any_element()
    })
}

/// The tab strip. It sits directly above the tab's surface and starts where the
/// editor's text does, so a tab labels the surface under it rather than the
/// window: the active one is lifted to the editor's tone, the rest are names
/// that reveal a wash on hover. No boxes, no hairlines — tone carries the
/// state.
fn render_tab_strip(
    profile: &Profile,
    (chrome_zoom, editor_zoom, grid_zoom): (u32, u32, u32),
    strip: &TabStrip,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let workspace = cx.entity().downgrade();
    let session = &profile.session;
    let runnable = session.editor(session.active).is_some();

    let chip = |active: bool| {
        div()
            .h(px(layout::chrome(layout::TAB_CHIP_HEIGHT)))
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
            .child(row_icon(
                t,
                match object.body {
                    ObjectBody::Structure { .. } => icon::STRUCTURE,
                    _ => object_icon(object.kind),
                },
            ))
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
                    .text_size(px(layout::chrome(layout::TEXT_XS)))
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

    // 100% is not information; a pane's readout appears only once its zoom
    // has somewhere to return to.
    let zoom: Vec<String> = [
        ("Chrome", chrome_zoom, true),
        ("Editor", editor_zoom, runnable),
        ("Grid", grid_zoom, session.active_results().is_some()),
    ]
    .into_iter()
    .filter(|&(_, percent, shown)| shown && percent != 100)
    .map(|(pane, percent, _)| format!("{pane} {percent}%"))
    .collect();
    let new_workspace = workspace.clone();

    div()
        .h(px(layout::chrome(layout::TAB_HEIGHT)))
        .w_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .px(px(layout::SPACE_SM))
        // Between the tabs and whatever is under them: the editor, the
        // filters, or the Structure bar.
        .border_b_1()
        .border_color(t.border)
        .text_size(px(layout::chrome(layout::TEXT_SM)))
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
        .children((!zoom.is_empty()).then(|| {
            div()
                .flex_shrink_0()
                .text_color(t.text_faint)
                .child(format!(
                    "{} · {} resets",
                    zoom.join(" · "),
                    keycap_text("secondary-0")
                ))
        }))
        .into_any_element()
}

/// What acts on a query's buffer, between the editor and the rows it produces:
/// the Data/Plan pair on the left, saving and running it on the right.
fn render_query_toolbar(
    session: &Session,
    engine: Engine,
    custom_limit: Option<&(LimitTarget, Entity<InputState>)>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let t = *theme(cx);
    let workspace = cx.entity().downgrade();
    let confirm_workspace = workspace.clone();
    let naming_a_rename = session.open_query().is_some();
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

    // Drawn only once there is a plan to turn to. Before that the pair would be
    // a control with one working half, which is the same as no control at all.
    let plan_toggle = session
        .active_query_tab()
        .filter(|tab| tab.flips_to_plan())
        .map(|tab| {
            div()
                .flex_shrink_0()
                .flex()
                .gap(px(layout::SPACE_XS))
                .child(preview_tab(
                    "Data",
                    icon::TABLE,
                    tab.plan_shown().is_none() && !tab.messages_shown(),
                    Command::ShowPlan(false),
                    cx,
                ))
                .child(preview_tab(
                    "Plan",
                    icon::PLAN,
                    tab.plan_shown().is_some(),
                    Command::ShowPlan(true),
                    cx,
                ))
        });

    // Only over rows read from one table with its key: what the grid
    // writes its edits back to is what the new row is inserted into. Named,
    // since a queue's results can each be a different table.
    let new_row = (!session.naming)
        .then(|| session.query_insert_target(cx))
        .flatten()
        .map(|(_, table, _)| {
            let label = format!("Insert in {table}");
            button("new-row", label, Tone::Quiet, Control::Compact, t).on_click(|_, window, cx| {
                window.dispatch_action(Box::new(NewRow), cx);
            })
        });

    let named = session.open_query().is_some();
    let save_workspace = workspace.clone();
    let rename_workspace = workspace.clone();
    let run_workspace = workspace.clone();

    control_row()
        .gap(px(layout::SPACE_SM))
        // Between the buffer and this bar; the split's handle is under it.
        .border_t_1()
        .border_color(t.border)
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .children(plan_toggle)
        .children(new_row)
        .child(div().flex_1())
        .children(naming)
        // A named query is already written to disk on every swap, so there
        // is nothing for a save button to do that has not been done. What
        // it can still do is change the name.
        .children((!session.naming && named).then(|| {
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
        .children((!session.naming && !named).then(|| {
            icon_button("save-query", icon::SAVE, Tone::Quiet, Control::Compact, t)
                .tooltip_with_action("Save query", &SaveQuery, None)
                .on_click(move |_, window, cx| {
                    _ = save_workspace.update(cx, |workspace, cx| {
                        workspace.save_query(&SaveQuery, window, cx);
                    });
                })
        }))
        .children(
            session
                .active_query_tab()
                .filter(|_| !session.naming)
                .map(|tab| {
                    let target = LimitTarget::Query(tab.id);
                    let editing = custom_limit
                        .filter(|(editing, _)| *editing == target)
                        .map(|(_, input)| input);
                    row_limit_menu(target, tab.row_limit, editing, t)
                }),
        )
        // Beside Run, because it asks about the same statement Run would run.
        // A menu rather than a button: the two modes differ by whether the
        // statement is executed, and a single button would have to pick one of
        // those on the user's behalf. Absent on an engine with no mode at all,
        // rather than a menu with nothing in it.
        .children(
            (!session.naming
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
        .child({
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
        })
        .into_any_element()
}

/// A relation's two views, each a tab of its own, and its New row: on the right
/// of whatever bar sits over the view, the filters' last line or a structure
/// tab's bar of its own.
fn relation_controls(
    structure: bool,
    takes_inserts: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let t = *theme(cx);
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .child(
            div()
                .flex()
                .gap(px(layout::SPACE_XS))
                .child(preview_tab(
                    "Data",
                    icon::TABLE,
                    !structure,
                    Command::ShowStructure(false),
                    cx,
                ))
                .child(preview_tab(
                    "Structure",
                    icon::STRUCTURE,
                    structure,
                    Command::ShowStructure(true),
                    cx,
                )),
        )
        .children(takes_inserts.then(|| {
            button("new-row", "Insert", Tone::Quiet, Control::Compact, t).on_click(
                |_, window, cx| {
                    window.dispatch_action(Box::new(NewRow), cx);
                },
            )
        }))
}

/// One choice of a handful, in the row-limit chips' clothes: all of them on
/// screen, the one in force filled in.
fn choice_chip(
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
        .h(px(layout::chrome(24.)))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::chrome(layout::TEXT_SM)))
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
