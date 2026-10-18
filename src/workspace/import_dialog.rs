//! The Import Rows dialog: a file's header, what its check found, and the
//! progress of the run that sends it.

use gpui::relative;

use super::*;
use crate::session::{ImportDialog, ImportPhase};

impl Workspace {
    pub(crate) fn render_import_rows(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = *theme(cx);
        let profile = self.profile()?;
        let dialog = profile.session.import_rows.as_ref()?;
        if Tab::Object(dialog.tab) != profile.session.active {
            return None;
        }
        let loading = profile.session.objects.iter().any(|tab| {
            tab.id == dialog.tab
                && matches!(
                    tab.body,
                    ObjectBody::Relation {
                        structure: StructureState::Loading,
                        ..
                    }
                )
        });
        let footer = match &dialog.phase {
            ImportPhase::Editing => import_footer(dialog, cx),
            ImportPhase::Running { .. } => import_progress(dialog, cx),
        };

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    dialog_card(t)
                        .child(import_header(dialog, cx))
                        .child(import_body(dialog, loading, cx))
                        .child(footer),
                )
                .into_any_element(),
        )
    }
}

fn dialog_card(t: Theme) -> gpui::Div {
    dialog(t)
        .w(px(layout::IMPORT_DIALOG_WIDTH))
        .h(px(layout::IMPORT_DIALOG_HEIGHT))
        .max_w(relative(0.95))
        .max_h(relative(0.92))
}

fn import_header(dialog: &ImportDialog, cx: &mut Context<Workspace>) -> impl IntoElement {
    let t = *theme(cx);
    let workspace = cx.entity().downgrade();
    let tab = dialog.tab;
    let name = dialog
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dialog.path.display().to_string());
    let rows = match &dialog.prepared {
        Some(Ok(prepared)) => Some(row_readout(
            prepared.checked.rows.len(),
            prepared.checked.rows.len(),
        )),
        _ => None,
    };
    let running = matches!(dialog.phase, ImportPhase::Running { .. });
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
                .font_weight(FontWeight::MEDIUM)
                .child(name),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(t.text_muted)
                .child(human_bytes(dialog.bytes.len() as u64)),
        )
        .child(
            button(
                "import-change-file",
                "Change file…",
                Tone::Quiet,
                Control::Standard,
                t,
            )
            .disabled(running)
            .on_click(move |_, window, cx| {
                _ = workspace.update(cx, |workspace, cx| {
                    workspace.prompt_import_file(tab, window, cx);
                });
            }),
        )
        .child(div().flex_1())
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(t.text_muted)
                .child(format!("{}.{}", dialog.schema, dialog.table)),
        )
        .children(rows.map(|rows| div().flex_shrink_0().child(rows)))
}

/// Filled in by the options, mapping and preview; until then, what stands in
/// for them.
fn import_body(dialog: &ImportDialog, loading: bool, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let body = div()
        .id("import-body")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .text_size(px(layout::chrome(layout::TEXT_SM)));
    match &dialog.prepared {
        Some(Err(message)) => body
            .text_color(t.danger)
            .children(lines(message))
            .into_any_element(),
        None if loading => body
            .text_color(t.text_muted)
            .child(format!(
                "Reading the columns of `{}.{}`…",
                dialog.schema, dialog.table
            ))
            .into_any_element(),
        _ => body.into_any_element(),
    }
}

/// One child per line: a single child carrying newlines is one run of text to
/// the layout.
fn lines(text: &str) -> impl Iterator<Item = gpui::Div> + '_ {
    text.lines().map(|line| div().child(line.to_string()))
}

/// What stops the import, if anything: the reason the button is disabled.
fn import_blocked(dialog: &ImportDialog) -> Option<String> {
    if dialog.checking {
        return Some("Checking the file…".into());
    }
    match &dialog.prepared {
        None => Some("Checking the file…".into()),
        Some(Err(message)) => Some(message.clone()),
        Some(Ok(prepared)) => prepared
            .checked
            .problems
            .first()
            .map(|problem| problem.message.clone()),
    }
}

fn import_footer(dialog: &ImportDialog, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let cancel_workspace = cx.entity().downgrade();
    let import_workspace = cancel_workspace.clone();
    let summary = match &dialog.prepared {
        Some(Ok(prepared)) => {
            let problems = &prepared.checked.problems;
            Some(match problems.first() {
                None => "No problems found".to_string(),
                Some(first) => match first.line {
                    Some(line) => format!(
                        "{} {}: first at line {}",
                        group_thousands(problems.len() as u64),
                        if problems.len() == 1 {
                            "problem"
                        } else {
                            "problems"
                        },
                        group_thousands(line as u64)
                    ),
                    None => first.message.clone(),
                },
            })
        }
        _ => None,
    };
    let rows = dialog
        .plan
        .as_ref()
        .map(|plan| plan.rows())
        .unwrap_or_default();
    let blocked = import_blocked(dialog);
    let import = button(
        "start-import",
        format!("Import {}", row_readout(rows, rows)),
        // Quiet while disabled, for `run-apply`'s reason: the library's
        // disabled fill dims nothing of a pinned label.
        if blocked.is_some() {
            Tone::Quiet
        } else {
            Tone::Primary
        },
        Control::Standard,
        t,
    )
    .disabled(blocked.is_some())
    .on_click(move |_, _, cx| {
        _ = import_workspace.update(cx, |workspace, cx| workspace.start_import(cx));
    });

    div()
        .flex()
        .items_end()
        .gap(px(layout::SPACE_MD))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .text_size(px(layout::chrome(layout::TEXT_SM)))
                .child(
                    div()
                        .flex()
                        .gap(px(layout::SPACE_SM))
                        .children(summary)
                        .children(
                            dialog
                                .checking
                                .then(|| div().text_color(t.text_faint).child("Checking…")),
                        ),
                )
                .children(dialog.error.as_ref().map(|error| {
                    div()
                        .id("import-error")
                        .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .text_color(t.danger)
                        .children(lines(error))
                })),
        )
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .gap(px(layout::SPACE_SM))
                .child(
                    button("cancel-import", "Cancel", Tone::Quiet, Control::Standard, t).on_click(
                        move |_, _, cx| {
                            _ = cancel_workspace.update(cx, |workspace, cx| {
                                workspace.close_import(cx);
                            });
                        },
                    ),
                )
                .child(match blocked {
                    Some(why) => import.tooltip(why),
                    None => import,
                }),
        )
        .into_any_element()
}

/// Drawn by hand: gpui-component's progress bar tweens its value.
fn import_progress(dialog: &ImportDialog, cx: &mut Context<Workspace>) -> AnyElement {
    let t = *theme(cx);
    let workspace = cx.entity().downgrade();
    let ImportPhase::Running {
        plan,
        run,
        started,
        cancelling,
        ..
    } = &dialog.phase
    else {
        return div().into_any_element();
    };
    let (inserted, total) = (run.inserted(), plan.rows());
    let elapsed = started.elapsed().as_secs();
    let fraction = match total {
        0 => 0.,
        total => inserted as f32 / total as f32,
    };
    div()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_MD))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(
                    div().h(px(4.)).rounded_full().bg(t.element_active).child(
                        div()
                            .h_full()
                            .w(relative(fraction))
                            .rounded_full()
                            .bg(t.accent),
                    ),
                )
                .child(
                    div()
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(t.text_muted)
                        .child(format!(
                            "Inserting {} of {} rows · {}:{:02}",
                            group_thousands(inserted as u64),
                            group_thousands(total as u64),
                            elapsed / 60,
                            elapsed % 60
                        )),
                ),
        )
        .child(
            button(
                "cancel-running-import",
                if *cancelling {
                    "Cancelling…"
                } else {
                    "Cancel"
                },
                Tone::Quiet,
                Control::Standard,
                t,
            )
            .disabled(*cancelling)
            .on_click(move |_, _, cx| {
                _ = workspace.update(cx, |workspace, cx| workspace.cancel_import(cx));
            }),
        )
        .into_any_element()
}
