//! Drawing the Show Grants dialog and the role form.

use gpui::{SharedString, relative};
use gpui_component::Icon;

use super::*;
use crate::ui::{raised_card, segmented, segmented_choice};

use crate::grants::{
    Mark, MatrixRow, Who, column_label, object_rows, role_columns, role_grants_empty, role_rows,
};
use crate::theme::ConnectionColor;
use crate::users_view::Loaded;

/// The Show Grants dialog's size, as drawn on the design canvas.
const GRANTS_WIDTH: f32 = 840.;
const GRANTS_HEIGHT: f32 = 600.;
/// How wide a privilege's column is.
const PRIVILEGE_COLUMN: f32 = 74.;

impl Workspace {
    pub(crate) fn render_show_grants(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.grants_view_in_front()?;
        let t = popup_theme(cx);
        let mono = fonts(cx).editor.clone();
        let filter = view.filter.read(cx).value().to_string();
        let (columns, rows): (Vec<String>, Vec<MatrixRow>) = match &view.data {
            Loaded::Ready(GrantsOf::Object(grants)) => (
                grants.privileges.clone(),
                object_rows(grants, view.with_members, &filter),
            ),
            Loaded::Ready(GrantsOf::Role(grants)) => {
                let columns = role_columns(grants);
                let rows = role_rows(grants, &columns, &filter);
                (columns, rows)
            }
            _ => (Vec::new(), Vec::new()),
        };
        let close_workspace = cx.entity().downgrade();
        let header_close = close_workspace.clone();
        let roles_workspace = close_workspace.clone();
        let members_workspace = close_workspace.clone();
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
            .child(
                div()
                    .px(px(layout::SPACE_SM))
                    .py(px(3.))
                    .rounded_full()
                    .bg(t.element_hover)
                    .text_color(t.text_muted)
                    .text_size(px(layout::chrome(11.5)))
                    .child("Read-only"),
            )
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

        let first_column = if view.of_object { "Role" } else { "Object" };
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
                    (true, GrantsOf::Role(grants)) => role_grants_empty(grants),
                    (true, GrantsOf::Object(_)) => "Nobody holds anything on it.".to_string(),
                })
                .into_any_element(),
            Loaded::Ready(_) => div()
                .id("show-grants-rows")
                .debug_selector(|| "show-grants-rows".into())
                .flex_1()
                .overflow_y_scroll()
                .child(table_head)
                .children(
                    rows.iter()
                        .map(|row| grants_row(row, &columns, t, mono.clone())),
                )
                .into_any_element(),
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
            .child(div().flex_1().min_w(px(160.)).line_height(px(layout::chrome(15.))).child(match view.of_object {
                true => "Members listed where the engine has role membership. Nothing here runs.",
                false => "Nothing here runs.",
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
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(gpui::black().opacity(0.5))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .occlude()
                        .debug_selector(|| "show-grants".into())
                        .w(px(GRANTS_WIDTH))
                        .max_w(relative(0.96))
                        .h(px(GRANTS_HEIGHT))
                        .max_h(relative(0.92))
                        .flex()
                        .flex_col()
                        .border_1()
                        .border_color(t.border_strong)
                        .rounded(px(layout::RADIUS_PANEL))
                        .overflow_hidden()
                        .text_size(px(layout::chrome(layout::TEXT_MD)))
                        .map(|card| raised_card(card, t))
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
                        .child(footer),
                )
                .into_any_element(),
        )
    }

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
fn grants_row(
    row: &MatrixRow,
    columns: &[String],
    t: Theme,
    mono: SharedString,
) -> impl IntoElement {
    let label = if row.member {
        div().text_color(t.text_muted)
    } else {
        div().text_color(t.text)
    };
    div()
        .flex()
        .items_center()
        .h(px(36.))
        .border_t_1()
        .border_color(t.border)
        .when(row.member, |line| line.bg(t.surface))
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
                .pl(px(if row.member { 46. } else { 18. }))
                .pr(px(18.))
                .child(
                    Icon::default()
                        .path(icon::CHEVRON_DOWN)
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
                                    .child(row.via.clone()),
                            )
                        }),
                ),
        )
        .children(columns.iter().zip(&row.marks).map(|(privilege, mark)| {
            let privilege = column_label(privilege);
            let said = match mark {
                Mark::Granted => "granted",
                Mark::Grantable => "granted, can grant to others",
                Mark::Through => "through a role it belongs to",
                Mark::None => "not granted",
            };
            div()
                .id(SharedString::from(format!("grant-{}-{privilege}", row.key)))
                .w(px(PRIVILEGE_COLUMN))
                .flex_none()
                .flex()
                .justify_center()
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(format!("{privilege}: {said}"))
                        .build(window, cx)
                })
                .child(mark_chip(*mark, 22., t, SharedString::default()))
        }))
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
    };
    div()
        .id(SharedString::from(format!("who-{}-{row}", who.label())))
        .flex_none()
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(who.label()).build(window, cx)
        })
        .child(Icon::default().path(path).size(px(size)).text_color(ink))
}
