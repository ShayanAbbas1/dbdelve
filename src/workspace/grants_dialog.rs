//! Drawing the Show Grants dialog and the role form.

use gpui::{
    CursorStyle, MouseButton, MouseDownEvent, Pixels, SharedString, Size, relative, size,
    uniform_list,
};
use gpui_component::Icon;

use super::*;
use crate::ui::{raised_card, segmented, segmented_choice};

use super::users::{GrantsEdge, GrantsResize};
use crate::db::roles::GrantAction;
use crate::grants::{Edit, Mark, MatrixRow, Staged, Who, column_label};
use crate::theme::ConnectionColor;
use crate::users_view::Loaded;

/// The Show Grants dialog's size, as drawn on the design canvas.
const GRANTS_WIDTH: f32 = 840.;
/// A role's access lists up to eleven privileges, a database's, a schema's
/// and a relation's in one row of columns.
const ROLE_GRANTS_WIDTH: f32 = 1040.;
const GRANTS_HEIGHT: f32 = 600.;
/// The share of the window the dialog can take, across and down, however far
/// it is dragged.
const GRANTS_MAX_WIDTH_SHARE: f32 = 0.96;
const GRANTS_MAX_HEIGHT_SHARE: f32 = 0.92;
/// How tall a row of the table is. The list draws only the rows in view and
/// needs them all the same height, notes included.
const GRANTS_ROW_HEIGHT: f32 = 36.;
/// How wide a privilege's column is.
const PRIVILEGE_COLUMN: f32 = 74.;
/// How far in each level of a role's access is drawn.
const ROLE_INDENT: f32 = 18.;

impl Workspace {
    pub(crate) fn render_show_grants(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.grants_view_in_front()?;
        let t = popup_theme(cx);
        let mono = fonts(cx).editor.clone();
        let filter = view.filter.read(cx).value().to_string();
        let (columns, rows) = (&view.matrix.columns, &view.matrix.rows);
        let close_workspace = cx.entity().downgrade();
        let header_close = close_workspace.clone();
        let roles_workspace = close_workspace.clone();
        let members_workspace = close_workspace.clone();
        let editable = self.grants_editable();
        let editing = view.editing && editable;
        let header = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(18.))
            .pt(px(layout::SPACE_LG))
            .pb(px(14.))
            .border_b_1()
            .border_color(t.border)
            .child(
                Icon::default()
                    .path(icon::SHIELD)
                    .size(px(16.))
                    .text_color(t.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .debug_selector(|| "show-grants-title".into())
                            .text_size(px(layout::chrome(15.)))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text)
                            .child(view.title.clone()),
                    )
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(layout::chrome(11.5)))
                            .text_color(t.text_muted)
                            .child(view.subtitle.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(9.))
                    .h(px(layout::chrome(layout::CONTROL_HEIGHT_COMPACT)))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .bg(t.element_hover)
                    .child(
                        Icon::default()
                            .path(icon::SEARCH)
                            .size(px(12.))
                            .text_color(t.text_muted),
                    )
                    .child(
                        div().w(px(120.)).child(
                            Input::new(&view.filter)
                                .appearance(false)
                                .p_0()
                                .text_size(px(layout::chrome(layout::TEXT_SM))),
                        ),
                    ),
            )
            .when(view.of_object, |header| {
                header.child(
                    segmented(t)
                        .debug_selector(|| "show-grants-members".into())
                        .child(
                            segmented_choice("grants-roles", "Roles", !view.with_members, true, t)
                                .on_click(move |_, _, cx| {
                                    _ = roles_workspace.update(cx, |workspace, cx| {
                                        workspace.set_grants_members(false, cx)
                                    });
                                }),
                        )
                        .child(
                            segmented_choice(
                                "grants-with-members",
                                "With members",
                                view.with_members,
                                true,
                                t,
                            )
                            .on_click(move |_, _, cx| {
                                _ = members_workspace.update(cx, |workspace, cx| {
                                    workspace.set_grants_members(true, cx)
                                });
                            }),
                        ),
                )
            })
            .child({
                let sticky = close_workspace.clone();
                // Filled while on, an outline while off: the button's own
                // fill stays the hover's, so hovering never reads as on.
                let pin = match view.sticky {
                    true => icon::PIN_FILLED,
                    false => icon::PIN,
                };
                icon_button("grants-sticky", pin, Tone::Quiet, Control::Compact, t)
                    .debug_selector(|| "show-grants-sticky".into())
                    .tooltip(match view.sticky {
                        true => "Stop pinning the database, schema or role in view",
                        false => "Pin the database, schema or role in view while scrolling",
                    })
                    .on_click(move |_, _, cx| {
                        _ = sticky.update(cx, |workspace, cx| workspace.toggle_grants_sticky(cx));
                    })
            })
            .child(match editable {
                true => self.grants_mode_switch(editing, view.staged.len(), t, cx),
                false => div()
                    .debug_selector(|| "show-grants-read-only".into())
                    .px(px(layout::SPACE_SM))
                    .py(px(3.))
                    .rounded_full()
                    .bg(t.element_hover)
                    .text_color(t.text_muted)
                    .text_size(px(layout::chrome(11.5)))
                    .child("Read-only")
                    .into_any_element(),
            })
            .child(
                icon_button(
                    "close-grants",
                    icon::CLOSE,
                    Tone::Quiet,
                    Control::Compact,
                    t,
                )
                .tooltip("Close")
                .on_click(move |_, _, cx| {
                    _ = header_close.update(cx, |workspace, cx| {
                        workspace.close_grants_view(cx);
                    });
                }),
            );

        let first_column = if view.of_object { "Role" } else { "Database" };
        let head = |label: String| {
            div()
                .text_size(px(layout::chrome(layout::TEXT_XS)))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.text_muted)
                .child(label.to_uppercase())
        };
        let table_head = div()
            .flex()
            .items_center()
            .h(px(34.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(18.))
                    .child(head(first_column.to_string())),
            )
            .children(columns.iter().map(|privilege| {
                div()
                    .w(px(PRIVILEGE_COLUMN))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(head(column_label(privilege)))
            }));
        let body: AnyElement = match &view.data {
            Loaded::Loading => div()
                .p(px(18.))
                .debug_selector(|| "show-grants-loading".into())
                .text_color(t.text_muted)
                .child("Reading the grants…")
                .into_any_element(),
            Loaded::Failed(message) => div()
                .p(px(18.))
                .debug_selector(|| "show-grants-failed".into())
                .text_color(t.danger)
                .child(message.clone())
                .into_any_element(),
            Loaded::Ready(grants) if rows.is_empty() => div()
                .p(px(18.))
                .debug_selector(|| "show-grants-empty".into())
                .text_color(t.text_muted)
                .child(match (filter.trim().is_empty(), grants) {
                    (false, _) => "Nothing matches the filter.".to_string(),
                    (true, GrantsOf::Role(_)) => "The server lists no database.".to_string(),
                    (true, GrantsOf::Object(_)) => "Nobody holds anything on it.".to_string(),
                })
                .into_any_element(),
            Loaded::Ready(_) => {
                let (row_theme, row_font) = (t, mono.clone());
                div()
                    .debug_selector(|| "show-grants-rows".into())
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(table_head)
                    .child(
                        uniform_list(
                            "show-grants-rows",
                            rows.len(),
                            // Only the rows in view are built: a login with
                            // thousands of grants would otherwise build them
                            // all on every frame.
                            cx.processor(move |workspace, range: std::ops::Range<usize>, _, cx| {
                                let Some(view) = workspace.grants_view_in_front() else {
                                    return Vec::new();
                                };
                                let matrix = &view.matrix;
                                let folds = cx.entity().downgrade();
                                let clicks = folds.clone();
                                range
                                    .filter_map(|ix| matrix.rows.get(ix))
                                    .map(|row| {
                                        let shut = view.collapsed.contains(&row.key);
                                        let drawn = grants_row(
                                            row,
                                            &matrix.columns,
                                            shut,
                                            editing.then_some(CellEditing {
                                                staged: &view.staged,
                                                clicks: Some(&clicks),
                                            }),
                                            row_theme,
                                            row_font.clone(),
                                        );
                                        match row.opened {
                                            false => drawn,
                                            true => foldable(drawn, &row.key, folds.clone()),
                                        }
                                    })
                                    .collect()
                            }),
                        )
                        .track_scroll(&view.scroll)
                        .when(view.sticky, |list| {
                            list.with_decoration(StickyParents {
                                rows: view.matrix.rows.clone(),
                                columns: view.matrix.columns.clone(),
                                staged: editing.then(|| view.staged.clone()),
                                scroll: view.scroll.clone(),
                                t,
                                mono: mono.clone(),
                            })
                        })
                        .w_full()
                        .flex_1()
                        .min_h_0(),
                    )
                    .into_any_element()
            }
        };
        let legend = |mark: AnyElement, label: &'static str| {
            // Each label wraps at a width of its own, two lines at most, as
            // the artboard's legend does.
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().flex_none().child(mark))
                .child(
                    div()
                        .max_w(px(layout::chrome(84.)))
                        .line_height(px(layout::chrome(15.)))
                        .child(label),
                )
        };
        let footer = div()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_MD))
            .px(px(18.))
            .py(px(10.))
            .border_t_1()
            .border_color(t.border)
            .text_size(px(layout::chrome(layout::TEXT_SM)))
            .text_color(t.text_muted)
            .child(legend(
                mark_chip(Mark::Granted, 18., t, mono.clone()),
                "granted",
            ))
            .child(legend(
                mark_chip(Mark::Grantable, 18., t, mono.clone()),
                "can grant to others",
            ))
            .when(view.of_object, |footer| {
                footer
                    .child(legend(
                        who_icon(Who::NoLogin, "legend", 14., t).into_any_element(),
                        "cannot log in",
                    ))
                    .child(legend(
                        who_icon(Who::Superuser, "legend", 14., t).into_any_element(),
                        "superuser",
                    ))
            })
            .child(legend(
                mark_chip(Mark::Through, 18., t, mono.clone()),
                "through a role it belongs to",
            ))
            .when(!view.of_object, |footer| {
                footer
                    .child(legend(
                        mark_chip(Mark::Public, 18., t, mono.clone()),
                        "granted to PUBLIC",
                    ))
                    .child(legend(
                        mark_chip(Mark::Blocked, 18., t, mono.clone()),
                        "granted, but the schema is closed",
                    ))
            })
            .child(div().flex_1().min_w(px(160.)).line_height(px(layout::chrome(15.))).child(match (editing, view.of_object) {
                (true, _) => "A click stages a change. Nothing runs before Run, in the review of every statement.",
                (false, true) => "Members listed where the engine has role membership. Nothing here runs.",
                (false, false) => "Effective access, as the server answers for it. Nothing here runs.",
            }))
            .child(
                self.choice(
                    "show-grants",
                    0,
                    button(
                        "close-shown-grants",
                        "Close",
                        Tone::Primary,
                        Control::Standard,
                        t,
                    )
                    .on_click(move |_, _, cx| {
                        _ = close_workspace.update(cx, |workspace, cx| {
                            workspace.close_grants_view(cx);
                        });
                    }),
                    t,
                ),
            );
        let resizing = view
            .resize
            .map(|drag| drag.edge)
            .filter(|_| cx.has_active_drag());
        let (width, height) = match view.size {
            Some(size) => (size.width, size.height),
            None => default_grants_size(view.of_object),
        };
        let grip = |edge: GrantsEdge, cx: &mut Context<Self>| {
            let (id, cursor) = match edge {
                GrantsEdge::Right => ("grants-resize-right", CursorStyle::ResizeLeftRight),
                GrantsEdge::Bottom => ("grants-resize-bottom", CursorStyle::ResizeUpDown),
                GrantsEdge::Corner => ("grants-resize-corner", CursorStyle::ResizeUpLeftDownRight),
            };
            let edge_width = px(layout::RESIZE_EDGE);
            let corner = px(layout::RESIZE_GRIP);
            div()
                .id(id)
                .debug_selector(|| id.into())
                .absolute()
                .cursor(cursor)
                .map(|handle| match edge {
                    GrantsEdge::Right => handle.top_0().right_0().bottom(corner).w(edge_width),
                    GrantsEdge::Bottom => handle.left_0().bottom_0().right(corner).h(edge_width),
                    GrantsEdge::Corner => handle.right_0().bottom_0().size(corner),
                })
                .when(edge == GrantsEdge::Corner, |handle| {
                    // The mark of a grip, in the corner it sits in.
                    handle.child(
                        div()
                            .absolute()
                            .right(px(layout::SPACE_XS))
                            .bottom(px(layout::SPACE_XS))
                            .size(px(layout::SPACE_SM + 2.))
                            .border_r_1()
                            .border_b_1()
                            .border_color(t.text_faint),
                    )
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |workspace, event: &MouseDownEvent, window, cx| {
                        workspace.begin_grants_resize(edge, event.position, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .on_drag(DragGrantsResize, |_, _, _, cx| cx.new(|_| DragGrantsResize))
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(t.scrim())
                .flex()
                .items_center()
                .justify_center()
                .when_some(resizing, |overlay, edge| {
                    overlay.cursor(grants_cursor(edge))
                })
                .on_drag_move::<DragGrantsResize>(cx.listener(
                    |workspace, event: &gpui::DragMoveEvent<DragGrantsResize>, window, cx| {
                        workspace.drag_grants_resize(event.event.position, window, cx);
                    },
                ))
                .child(
                    div()
                        .occlude()
                        .debug_selector(|| "show-grants".into())
                        .w(width)
                        .max_w(relative(GRANTS_MAX_WIDTH_SHARE))
                        .h(height)
                        .max_h(relative(GRANTS_MAX_HEIGHT_SHARE))
                        .flex()
                        .flex_col()
                        .border_1()
                        .border_color(t.border_strong)
                        .rounded(px(layout::RADIUS_PANEL))
                        .overflow_hidden()
                        .text_size(px(layout::chrome(layout::TEXT_MD)))
                        .map(|card| raised_card(card, t))
                        .when_some(resizing, |card, edge| card.cursor(grants_cursor(edge)))
                        .child(header)
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .flex_col()
                                .py(px(6.))
                                .child(body),
                        )
                        .children(editing.then(|| self.grants_edit_bar(t, mono.clone(), cx)))
                        .child(footer)
                        .child(grip(GrantsEdge::Right, cx))
                        .child(grip(GrantsEdge::Bottom, cx))
                        .child(grip(GrantsEdge::Corner, cx)),
                )
                .into_any_element(),
        )
    }

    /// View or Edit, in the header, where the engine writes grant changes.
    /// Back to View drops what was staged, as its tooltip says.
    fn grants_mode_switch(
        &self,
        editing: bool,
        staged: usize,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (view_workspace, edit_workspace) = (cx.entity().downgrade(), cx.entity().downgrade());
        segmented(t)
            .debug_selector(|| "show-grants-mode".into())
            .child(
                segmented_choice("grants-view", "View", !editing, true, t)
                    .debug_selector(|| "show-grants-view".into())
                    .when(staged > 0, |choice| {
                        choice.tooltip(match staged {
                            1 => "Back to viewing: drops the change staged".to_string(),
                            n => format!("Back to viewing: drops the {n} changes staged"),
                        })
                    })
                    .on_click(move |_, _, cx| {
                        _ = view_workspace
                            .update(cx, |workspace, cx| workspace.set_grants_editing(false, cx));
                    }),
            )
            .child(
                segmented_choice("grants-edit", "Edit", editing, true, t)
                    .debug_selector(|| "show-grants-edit".into())
                    .tooltip("Stage grants and revokes, to review before any runs")
                    .on_click(move |_, _, cx| {
                        _ = edit_workspace
                            .update(cx, |workspace, cx| workspace.set_grants_editing(true, cx));
                    }),
            )
            .into_any_element()
    }

    /// The strip over the footer in Edit mode: what a staged cell looks like,
    /// why the last click staged nothing, the count, Discard and Review.
    fn grants_edit_bar(&self, t: Theme, mono: SharedString, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.grants_view_in_front() else {
            return div().into_any_element();
        };
        let staged = view.staged.len();
        let (discard_workspace, review_workspace) =
            (cx.entity().downgrade(), cx.entity().downgrade());
        let key = |chip: AnyElement, label: &'static str| {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(chip)
                .child(label)
        };
        div()
            .debug_selector(|| "show-grants-edit-bar".into())
            .flex()
            .items_center()
            .gap(px(layout::SPACE_MD))
            .px(px(18.))
            .py(px(8.))
            .border_t_1()
            .border_color(t.border)
            .text_size(px(layout::chrome(layout::TEXT_SM)))
            .text_color(t.text_muted)
            .child(key(
                staged_chip(GrantAction::Grant, 18., t),
                "to be granted",
            ))
            .child(key(
                staged_chip(GrantAction::Revoke, 18., t),
                "to be revoked",
            ))
            .when(view.of_object, |bar| bar.child(self.grant_to_button(t, cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .children(view.refusal.clone().map(|refusal| {
                        div()
                            .debug_selector(|| "show-grants-refusal".into())
                            .text_color(t.danger)
                            .child(refusal)
                    })),
            )
            .child(
                div()
                    .debug_selector(|| "show-grants-staged-count".into())
                    .font_family(mono)
                    .child(match staged {
                        1 => "1 change".to_string(),
                        n => format!("{n} changes"),
                    }),
            )
            .child(
                button(
                    "grants-discard",
                    "Discard",
                    Tone::Quiet,
                    Control::Compact,
                    t,
                )
                .debug_selector(|| "show-grants-discard".into())
                .disabled(staged == 0)
                .on_click(move |_, _, cx| {
                    _ = discard_workspace
                        .update(cx, |workspace, cx| workspace.discard_grant_changes(cx));
                }),
            )
            .child(
                button(
                    "grants-review",
                    "Review changes…",
                    if staged == 0 {
                        Tone::Quiet
                    } else {
                        Tone::Primary
                    },
                    Control::Compact,
                    t,
                )
                .debug_selector(|| "show-grants-review".into())
                .disabled(staged == 0)
                .on_click(move |_, window, cx| {
                    _ = review_workspace.update(cx, |workspace, cx| {
                        workspace.review_grant_changes(window, cx)
                    });
                }),
            )
            .into_any_element()
    }

    /// Grant to…, in an object view's Edit mode: a role not listed yet, picked
    /// from a menu, is listed holding nothing so a grant can be staged on it.
    ///
    /// ponytail: a plain scrolling menu of every role. A server with
    /// thousands wants the menu filtered as it is typed into.
    fn grant_to_button(&self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let candidates: Vec<String> = match self.grants_view_in_front() {
            Some(view) => match &view.data {
                Loaded::Ready(GrantsOf::Object(grants)) => {
                    crate::grants::grant_candidates(grants, &view.added)
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                }
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        let workspace = cx.entity().downgrade();
        button(
            "grants-grant-to",
            "Grant to…",
            Tone::Quiet,
            Control::Compact,
            t,
        )
        .debug_selector(|| "show-grants-grant-to".into())
        .disabled(candidates.is_empty())
        .tooltip(match candidates.is_empty() {
            true => "Every role is listed already",
            false => "List a role that holds nothing here, to grant it something",
        })
        .child(
            Icon::default()
                .path(icon::CHEVRON_DOWN)
                .size(px(layout::chrome(layout::ICON_SIZE)))
                .text_color(t.text_faint),
        )
        .dropdown_menu(move |menu, _, _| {
            candidates.iter().fold(
                menu.scrollable(true).max_h(px(layout::MENU_MAX_HEIGHT)),
                |menu, role| {
                    let (workspace, role) = (workspace.clone(), role.clone());
                    menu.item(
                        gpui_component::menu::PopupMenuItem::new(role.clone()).on_click(
                            move |_, _, cx| {
                                _ = workspace.update(cx, |workspace, cx| {
                                    workspace.add_grant_role(&role, cx)
                                });
                            },
                        ),
                    )
                },
            )
        })
        .into_any_element()
    }

    /// The dialog's size now, as drawn: what the user dragged it to or its
    /// default, within the window.
    fn grants_size(&self, window: &Window) -> Option<Size<Pixels>> {
        let view = self.grants_view_in_front()?;
        let (width, height) = match view.size {
            Some(size) => (size.width, size.height),
            None => default_grants_size(view.of_object),
        };
        Some(clamp_grants_size(width, height, window.viewport_size()))
    }

    /// A press on one of the dialog's grips: remember where, and how big.
    fn begin_grants_resize(
        &mut self,
        edge: GrantsEdge,
        origin: gpui::Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(size) = self.grants_size(window) else {
            return;
        };
        if let Some(view) = self.grants_view.as_mut() {
            view.resize = Some(GrantsResize { edge, origin, size });
            cx.notify();
        }
    }

    /// The pointer moved with a grip held. The dialog stays centred, so each
    /// pixel the pointer travels moves the opposite edge too: twice as much
    /// size keeps the corner under the pointer.
    fn drag_grants_resize(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.grants_view.as_mut() else {
            return;
        };
        let Some(drag) = view.resize else {
            return;
        };
        let moved = position - drag.origin;
        let (mut width, mut height) = (drag.size.width, drag.size.height);
        if drag.edge != GrantsEdge::Bottom {
            width += moved.x * 2.;
        }
        if drag.edge != GrantsEdge::Right {
            height += moved.y * 2.;
        }
        let next = clamp_grants_size(width, height, window.viewport_size());
        if view.size != Some(next) {
            view.size = Some(next);
            cx.notify();
        }
    }
}

/// What a grip drags the dialog by, empty: the dialog itself is the preview.
struct DragGrantsResize;

impl Render for DragGrantsResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn grants_cursor(edge: GrantsEdge) -> CursorStyle {
    match edge {
        GrantsEdge::Right => CursorStyle::ResizeLeftRight,
        GrantsEdge::Bottom => CursorStyle::ResizeUpDown,
        GrantsEdge::Corner => CursorStyle::ResizeUpLeftDownRight,
    }
}

/// The size the dialog opens at.
fn default_grants_size(of_object: bool) -> (Pixels, Pixels) {
    let width = match of_object {
        true => GRANTS_WIDTH,
        false => ROLE_GRANTS_WIDTH,
    };
    (px(width), px(GRANTS_HEIGHT))
}

/// A size held between the least the dialog's content fits in and the share
/// of the window it can take, the window's share winning in a small window.
fn clamp_grants_size(width: Pixels, height: Pixels, window: Size<Pixels>) -> Size<Pixels> {
    let widest = window.width * GRANTS_MAX_WIDTH_SHARE;
    let tallest = window.height * GRANTS_MAX_HEIGHT_SHARE;
    size(
        width.max(px(layout::GRANTS_MIN_WIDTH)).min(widest),
        height.max(px(layout::GRANTS_MIN_HEIGHT)).min(tallest),
    )
}

impl Workspace {
    pub(crate) fn render_role_form(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let form = self.role_form.as_ref()?;
        if self
            .profile()
            .is_none_or(|profile| profile.id != form.profile)
        {
            return None;
        }
        let t = popup_theme(cx);
        let field =
            |label: &'static str, input: &Entity<InputState>, note: Option<&'static str>| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_XS))
                    .child(
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(t.text_muted)
                            .child(label),
                    )
                    .child(Input::new(input).text_size(px(layout::chrome(layout::TEXT_MD))))
                    .children(note.map(|note| {
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_XS)))
                            .text_color(t.text_faint)
                            .child(note)
                    }))
            };
        let (new, login, password) = match &form.kind {
            RoleFormKind::New { login } => (true, *login, *login),
            RoleFormKind::Edit(_) => (false, false, false),
            RoleFormKind::Password(_) => (false, false, true),
        };
        let editing = matches!(form.kind, RoleFormKind::Edit(_));
        let close_workspace = cx.entity().downgrade();
        let submit_workspace = close_workspace.clone();
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
                        .debug_selector(|| "role-form".into())
                        .child(section_label(t, &form.title()))
                        .when(new, |card| card.child(field("Name", &form.name, None)))
                        .when(password, |card| {
                            card.child(field(
                                "Password",
                                &form.password,
                                Some("Sent as its SCRAM-SHA-256 hash, never as typed."),
                            ))
                            .child(field(
                                "Confirm password",
                                &form.confirm,
                                None,
                            ))
                        })
                        .when(login || editing, |card| {
                            card.child(field("Connection limit", &form.limit, None))
                                .child(field("Password expires", &form.expires, None))
                        })
                        .when(new, |card| {
                            card.child(field("Member of", &form.member_of, None))
                        })
                        .children(form.error.clone().map(|error| {
                            div()
                                .debug_selector(|| "role-form-error".into())
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.danger)
                                .child(error)
                        }))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    self.choice(
                                        "role-form",
                                        1,
                                        button(
                                            "role-form-cancel",
                                            "Cancel",
                                            Tone::Quiet,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, _, cx| {
                                                _ = close_workspace.update(cx, |workspace, cx| {
                                                    workspace.close_role_form(cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                )
                                .child(
                                    self.choice(
                                        "role-form",
                                        0,
                                        button(
                                            "role-form-review",
                                            "Review…",
                                            Tone::Primary,
                                            Control::Standard,
                                            t,
                                        )
                                        .on_click(
                                            move |_, window, cx| {
                                                _ = submit_workspace.update(cx, |workspace, cx| {
                                                    workspace.submit_role_form(window, cx);
                                                });
                                            },
                                        ),
                                        t,
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// One row of the grants table.
/// The rows holding the first one in view, pinned over the top of the list
/// ([`crate::grants::sticky_parents`]). A click on one scrolls to it.
struct StickyParents {
    rows: std::sync::Arc<[MatrixRow]>,
    columns: Vec<String>,
    /// What Edit mode has staged, drawn on the pinned rows too; their cells
    /// take no click, the row's click scrolls to it.
    staged: Option<Staged>,
    scroll: gpui::UniformListScrollHandle,
    t: Theme,
    mono: SharedString,
}

impl gpui::UniformListDecoration for StickyParents {
    fn compute(
        &self,
        visible_range: std::ops::Range<usize>,
        _bounds: gpui::Bounds<Pixels>,
        scroll_offset: gpui::Point<Pixels>,
        _item_height: Pixels,
        _item_count: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        let pinned = crate::grants::sticky_parents(&self.rows, visible_range.start);
        // The list places a decoration where the top of its scrolled content
        // is, made for marks that scroll with the rows; padded down by as far
        // as the rows have scrolled, the pinned ones stay at the list's top.
        let stack = div()
            .debug_selector(|| "show-grants-sticky-rows".into())
            .w_full()
            .flex()
            .flex_col()
            .when(!pinned.is_empty(), |stack| {
                stack.border_b_1().border_color(self.t.border)
            })
            .children(pinned.into_iter().map(|at| {
                let row = &self.rows[at];
                let scroll = self.scroll.clone();
                div()
                    .id(SharedString::from(format!(
                        "show-grants-pinned-{}",
                        row.key
                    )))
                    .debug_selector({
                        let key = row.key.clone();
                        move || format!("show-grants-pinned-{key}")
                    })
                    .w_full()
                    .occlude()
                    .cursor_pointer()
                    .bg(self.t.overlay)
                    .on_click(move |_, _, _| {
                        scroll.scroll_to_item(at, gpui::ScrollStrategy::Top);
                    })
                    .child(grants_row(
                        row,
                        &self.columns,
                        false,
                        self.staged.as_ref().map(|staged| CellEditing {
                            staged,
                            clicks: None,
                        }),
                        self.t,
                        self.mono.clone(),
                    ))
            }));
        div()
            .w_full()
            .pt(-scroll_offset.y)
            .child(stack)
            .into_any_element()
    }
}

/// A row with rows under it, which a click folds shut or opens again.
fn foldable(drawn: AnyElement, key: &str, folds: gpui::WeakEntity<Workspace>) -> AnyElement {
    let key = key.to_string();
    div()
        .id(SharedString::from(format!("show-grants-fold-{key}")))
        .w_full()
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            folds
                .update(cx, |workspace, cx| {
                    if let Some(view) = workspace.grants_view.as_mut() {
                        view.toggle_fold(&key);
                    }
                    cx.notify();
                })
                .ok();
        })
        .child(drawn)
        .into_any_element()
}

/// What Edit mode adds to a row's cells: what is staged, and where a click
/// goes (nowhere on a pinned row).
#[derive(Clone, Copy)]
struct CellEditing<'a> {
    staged: &'a Staged,
    clicks: Option<&'a gpui::WeakEntity<Workspace>>,
}

fn grants_row(
    row: &MatrixRow,
    columns: &[String],
    shut: bool,
    editing: Option<CellEditing>,
    t: Theme,
    mono: SharedString,
) -> AnyElement {
    if row.who == Who::Note {
        return div()
            .debug_selector({
                let key = row.key.clone();
                move || format!("show-grants-note-{key}")
            })
            .w_full()
            .flex()
            .items_center()
            .h(px(GRANTS_ROW_HEIGHT))
            .pl(px(18. + f32::from(row.depth) * ROLE_INDENT + 22.))
            .pr(px(18.))
            .text_size(px(layout::chrome(layout::TEXT_XS)))
            .text_color(t.text_faint)
            .child(row.name.clone())
            .into_any_element();
    }
    let label = if row.member {
        div().text_color(t.text_muted)
    } else {
        div().text_color(t.text)
    };
    let header = row.who == Who::Database;
    // The list lays each row out on its own, as wide as its content unless
    // told otherwise, and the privilege columns line up only when every row
    // spans the list.
    div()
        .w_full()
        .flex()
        .items_center()
        .h(px(GRANTS_ROW_HEIGHT))
        .border_t_1()
        .border_color(t.border)
        // `frost`, not `surface`: the dialog's card lets the window through on
        // glass, and an opaque band on it reads as a hole in the frost. On a
        // solid theme it is `surface` itself.
        .when(row.member || header, |line| line.bg(t.frost()))
        .debug_selector({
            let name = row.name.clone();
            move || format!("show-grants-row-{name}")
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(layout::SPACE_SM))
                .pl(px(match row.member {
                    true => 46.,
                    false => 18. + f32::from(row.depth) * ROLE_INDENT,
                }))
                .pr(px(18.))
                .child(
                    Icon::default()
                        .path(match shut {
                            true => icon::CHEVRON_RIGHT,
                            false => icon::CHEVRON_DOWN,
                        })
                        .size(px(11.))
                        .text_color(t.text_muted)
                        .when(!row.opened, |chevron| {
                            chevron.text_color(gpui::transparent_black())
                        }),
                )
                .child(who_icon(row.who, &row.key, 15., t))
                .child(
                    div()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .child(
                            label
                                .font_family(mono)
                                .font_weight(if row.member {
                                    FontWeight::NORMAL
                                } else {
                                    FontWeight::MEDIUM
                                })
                                .text_size(px(layout::chrome(12.5)))
                                .line_height(px(layout::chrome(16.)))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(row.name.clone()),
                        )
                        .when(!row.via.is_empty(), |name| {
                            name.child(
                                div()
                                    .text_size(px(layout::chrome(layout::TEXT_XS)))
                                    .line_height(px(layout::chrome(14.)))
                                    .text_color(t.text_faint)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(row.via.clone()),
                            )
                        }),
                ),
        )
        .children((columns.iter().zip(&row.marks).enumerate()).map(
            |(column, (privilege, mark))| match editing {
                Some(editing) => edit_cell(row, column, privilege, *mark, editing, t),
                None => {
                    let (label, said) = (column_label(privilege), mark.said());
                    div()
                        .id(SharedString::from(format!("grant-{}-{label}", row.key)))
                        .w(px(PRIVILEGE_COLUMN))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(format!("{label}: {said}"))
                                .build(window, cx)
                        })
                        .child(mark_chip(*mark, 22., t, SharedString::default()))
                        .into_any_element()
                }
            },
        ))
        .into_any_element()
}

/// One cell in Edit mode: a staged change drawn in its place, every other
/// cell with what a click on it does, a cell that cannot change faded. Every
/// cell takes the click, so one that cannot change says why in the dialog.
fn edit_cell(
    row: &MatrixRow,
    column: usize,
    privilege: &str,
    mark: Mark,
    editing: CellEditing,
    t: Theme,
) -> AnyElement {
    let pending = editing.staged.pending(row, privilege);
    let edit = row.edits.get(column);
    let tip = crate::grants::edit_tooltip(
        privilege,
        mark,
        edit,
        pending,
        row.target.as_ref().map(|target| &target.object),
    );
    let changes = matches!(edit, Some(Edit::Revoke | Edit::Grant(_)));
    let selector = format!("show-grants-cell-{}-{privilege}", row.key);
    let chip = match pending {
        Some(action) => staged_chip(action, 22., t),
        None => mark_chip(mark, 22., t, SharedString::default()),
    };
    let key = row.key.clone();
    div()
        .id(SharedString::from(format!(
            "grant-edit-{}-{privilege}",
            row.key
        )))
        .debug_selector(move || selector.clone())
        .w(px(PRIVILEGE_COLUMN))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
        })
        .when(changes, |cell| {
            cell.cursor_pointer().hover(|cell| cell.bg(t.element_hover))
        })
        .when_some(editing.clicks.cloned(), |cell, clicks| {
            cell.on_click(move |_, _, cx| {
                cx.stop_propagation();
                _ = clicks.update(cx, |workspace, cx| {
                    workspace.toggle_grant_cell(&key, column, cx)
                });
            })
        })
        .child(
            div()
                .when(!changes && pending.is_none(), |chip| chip.opacity(0.4))
                .child(chip),
        )
        .into_any_element()
}

/// A staged change, drawn where the cell's mark was: a grant in the colour
/// an added row has, a revoke in a deleted row's, each outlined dashed, so
/// neither reads as something the server holds.
fn staged_chip(action: GrantAction, size: f32, t: Theme) -> AnyElement {
    let (wash, ink, glyph) = match action {
        GrantAction::Grant => (t.added, t.success, icon::PLUS),
        GrantAction::Revoke => (t.deleted, t.danger, icon::MINUS),
    };
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(if size > 20. { 5. } else { 4. }))
        .bg(wash)
        .border_1()
        .border_dashed()
        .border_color(ink)
        .child(
            Icon::default()
                .path(glyph)
                .size(px(size * 0.55))
                .text_color(ink),
        )
        .into_any_element()
}

/// A cell's mark, drawn `size` square.
fn mark_chip(mark: Mark, size: f32, t: Theme, mono: SharedString) -> AnyElement {
    let (fill, ink) = match mark {
        Mark::Granted | Mark::Grantable => (
            Some(ConnectionColor::Green.fill()),
            ConnectionColor::Green.ink(t),
        ),
        Mark::Through => (
            Some(ConnectionColor::Blue.fill()),
            ConnectionColor::Blue.ink(t),
        ),
        Mark::Public => (
            Some(ConnectionColor::Purple.fill()),
            ConnectionColor::Purple.ink(t),
        ),
        Mark::Blocked => (
            Some(ConnectionColor::Orange.fill()),
            ConnectionColor::Orange.ink(t),
        ),
        Mark::None => (None, t.text_faint),
    };
    let glyph = |path: &'static str| {
        Icon::default()
            .path(path)
            .size(px(size * 0.5))
            .text_color(ink)
    };
    let chip = div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(if size > 20. { 5. } else { 4. }))
        .when_some(fill, |chip, fill| chip.bg(fill))
        .text_color(ink)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(size * 0.5))
        .when(!mono.is_empty(), |chip| chip.font_family(mono));
    match mark {
        Mark::Granted => chip.child(glyph(icon::CHECK)),
        Mark::Grantable => chip
            .gap(px(0.5))
            .child(glyph(icon::CHECK))
            .child(div().line_height(px(size * 0.5)).child("+")),
        Mark::Through => chip.child(glyph(icon::THROUGH)),
        Mark::Public => chip.child(glyph(icon::PUBLIC)),
        Mark::Blocked => chip.child(glyph(icon::LOCK)),
        Mark::None => chip.child("·"),
    }
    .into_any_element()
}

/// What a row is, as an icon: a superuser's in gold.
fn who_icon(who: Who, row: &str, size: f32, t: Theme) -> impl IntoElement {
    let (path, ink) = match who {
        Who::Login => (icon::USER, t.text_muted),
        Who::NoLogin => (icon::LOCK, t.text_muted),
        Who::Superuser => (icon::SHIELD, ConnectionColor::Yellow.ink(t)),
        Who::Public => (icon::PUBLIC, t.text_muted),
        Who::Object => (icon::TABLE, t.text_muted),
        Who::Database => (icon::DATABASE, t.text_muted),
        Who::Schema => (icon::SCHEMA, t.text_muted),
        Who::Note => (icon::TABLE, t.text_faint),
    };
    div()
        .id(SharedString::from(format!("who-{}-{row}", who.label())))
        .flex_none()
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(who.label()).build(window, cx)
        })
        .child(Icon::default().path(path).size(px(size)).text_color(ink))
}
