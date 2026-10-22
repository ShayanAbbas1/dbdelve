//! The modal surfaces: the connection form, the palette, the confirmations.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use gpui::StyledText;
use gpui_component::checkbox::Checkbox;
use gpui_component::menu::PopupMenuItem;

use super::*;
use crate::connection_form::ConnectionTest;
use crate::explorer::{drop_sql, select_top_sql, truncate_sql};
use crate::scroller::{SmoothScrollable, smooth_scoped};
use crate::sql::{Destructive, Stop};

impl Workspace {
    pub(crate) fn render_connection_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        let form = self
            .form
            .as_ref()
            .expect("form is rendered only while open");
        let message = form.error.clone();
        let editing = form.editing.is_some();
        // Scoped to the profile being edited, or "new", so switching from
        // editing one profile to another (or to a fresh connection) doesn't
        // open on the last profile's scroll offset.
        let scope = form.editing.as_deref().unwrap_or("new");
        let hairline = || div().h(px(1.)).flex_1().bg(t.border);
        let labelled = |label: &'static str, control: AnyElement| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(layout::SPACE_XS))
                .child(
                    div()
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text_muted)
                        .child(label),
                )
                .child(control)
        };

        div()
            .id("connection-form-scroll")
            .size_full()
            .overflow_y_scroll()
            .smooth_scroll(&smooth_scoped("connection-form-scroll", scope, cx))
            .p(px(layout::SPACE_LG))
            .flex()
            .flex_col()
            .items_center()
            .child(
                div()
                    // Centred by auto margins rather than `justify_center`,
                    // which pushes a form taller than the window (an SSH
                    // tunnel under a verifying mode) off the top, where
                    // scrolling cannot reach.
                    .my_auto()
                    .flex_shrink_0()
                    .w(px(layout::DIALOG_WIDTH))
                    .p(px(layout::SPACE_LG))
                    .bg(t.panel)
                    .border_1()
                    .border_color(t.border)
                    .rounded(px(layout::RADIUS_PANEL))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap(px(layout::SPACE_MD))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(layout::SPACE_MD))
                            .child(icon_tile(t, icon::DATABASE))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_size(px(layout::chrome(layout::TEXT_LG)))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(if editing {
                                                "Edit connection"
                                            } else {
                                                "Connect to a database"
                                            }),
                                    )
                                    // Snowflake has no connection URL to paste,
                                    // so a fresh form for it says nothing about
                                    // one; editing keeps the generic subtitle,
                                    // which never mentioned a URL either.
                                    .children(
                                        (editing || form.engine.fields() != Fields::Account).then(
                                            || {
                                                div()
                                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                                    .text_color(t.text_muted)
                                                    .child(if editing {
                                                        "Change where this connection points."
                                                    } else {
                                                        "Paste a URL, or fill in the fields."
                                                    })
                                            },
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(layout::SPACE_SM))
                            .child(labelled("Engine", self.engine_dropdown(cx)))
                            // Only while creating: past that,
                            // `Workspace::set_mode` is the one door a mode
                            // changes through, from the titlebar, and it pushes
                            // the change into live grids this form has no
                            // route to.
                            .when(!editing, |row| {
                                row.child(labelled("Mode", self.mode_dropdown(cx)))
                            })
                            .child(labelled("Color", self.color_dropdown(cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .child(labelled("Project", self.project_dropdown(cx))),
                    )
                    // Snowflake has no connection URL: `Engine::fields()` is
                    // where that is decided, not a match on the engine here.
                    .when(form.engine.fields() != Fields::Account, |form_div| {
                        form_div
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(layout::SPACE_XS))
                                    .child(
                                        div()
                                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(t.text_muted)
                                            .child("Connection URL"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap(px(layout::SPACE_SM))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(Input::new(&form.url).w_full()),
                                            )
                                            .child(
                                                icon_button(
                                                    "apply-connection-url",
                                                    icon::FILL_DOWN,
                                                    Tone::Primary,
                                                    Control::Standard,
                                                    t,
                                                )
                                                .tooltip("Fill the fields from this URL")
                                                .on_click(cx.listener(Self::apply_connection_url)),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(layout::SPACE_SM))
                                    .child(hairline())
                                    .child(
                                        div()
                                            .text_size(px(layout::chrome(layout::TEXT_XS)))
                                            .text_color(t.text_faint)
                                            .child("OR"),
                                    )
                                    .child(hairline()),
                            )
                    })
                    .child(self.form_field("Display name", &form.name, cx))
                    // An engine that is a file has no host, no credentials and
                    // no transport, so those fields are absent rather than
                    // present and inert. A disabled field still reads as
                    // something the connection has.
                    .children(
                        (form.engine.fields() == Fields::File)
                            .then(|| self.form_field("Database file", &form.path, cx)),
                    )
                    // No encryption row: the transport is HTTPS and always
                    // verified, so there is no choice to show. No password
                    // either; the key file is the credential.
                    .children((form.engine.fields() == Fields::Account).then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_MD))
                            .child(self.form_field("Account", &form.account, cx))
                            .child(self.form_field("Username", &form.user, cx))
                            .child(self.form_field("Private key file", &form.private_key, cx))
                            .child(self.form_field("Database", &form.database, cx))
                            .child(self.form_field("Warehouse", &form.warehouse, cx))
                            .child(self.form_field("Role", &form.role, cx))
                            // Last, because it is nearly always blank: the
                            // account names its own host.
                            .child(self.form_field("Host", &form.host, cx))
                    }))
                    .children((form.engine.fields() == Fields::Server).then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(layout::SPACE_MD))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(layout::SPACE_SM))
                                    .child(
                                        div()
                                            .flex_1()
                                            .child(self.form_field("Host", &form.host, cx)),
                                    )
                                    // An SRV name takes no port: its records
                                    // name each server's own.
                                    .children((!(form.srv && form.engine.resolves_srv())).then(
                                        || {
                                            div()
                                                .w(px(96.))
                                                .child(self.form_field("Port", &form.port, cx))
                                        },
                                    )),
                            )
                            .children(form.engine.resolves_srv().then(|| {
                                Checkbox::new("srv-host")
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .font_weight(FontWeight::MEDIUM)
                                    .label("Host is an SRV name (mongodb+srv)")
                                    .checked(form.srv)
                                    .on_click(cx.listener(|workspace, on: &bool, _, cx| {
                                        if let Some(form) = &mut workspace.form {
                                            form.srv = *on;
                                            form.test = None;
                                            cx.notify();
                                        }
                                    }))
                            }))
                            .child(self.form_field("Database", &form.database, cx))
                            .child(self.form_field("Username", &form.user, cx))
                            .child(self.labelled_field(
                                "Password",
                                Input::new(&form.password).mask_toggle().w_full(),
                                cx,
                            ))
                            .children(
                                form.engine
                                    .takes_options()
                                    .then(|| self.form_field("Options", &form.options, cx)),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(layout::SPACE_XS))
                                    .child(
                                        div()
                                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(t.text_muted)
                                            .child("Encryption"),
                                    )
                                    .child(div().flex().gap(px(layout::SPACE_XS)).children(
                                        SslMode::ALL.map(|mode| self.sslmode_chip(mode, cx)),
                                    ))
                                    // Five words do not say which ones check who
                                    // answered, and that is the whole difference
                                    // between them.
                                    .child(
                                        div()
                                            .text_size(px(layout::chrome(layout::TEXT_XS)))
                                            .text_color(t.text_faint)
                                            .child(form.sslmode.explanation()),
                                    ),
                            )
                            // Only where it is consulted: on `require` a
                            // certificate file changes nothing, and a field that
                            // changes nothing reads as though it does.
                            .children(form.sslmode.checks_certificate().then(|| {
                                self.form_field("Root certificate", &form.root_certificate, cx)
                            }))
                            .child(
                                Checkbox::new("ssh-tunnel")
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .font_weight(FontWeight::MEDIUM)
                                    .label("Connect through an SSH tunnel")
                                    .checked(form.ssh)
                                    .on_click(cx.listener(|workspace, on: &bool, _, cx| {
                                        if let Some(form) = &mut workspace.form {
                                            form.ssh = *on;
                                            if *on {
                                                form.needs_focus = Some(form.ssh_host.clone());
                                            }
                                            // A pass without the tunnel did not test it.
                                            form.test = None;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .children(form.ssh.then(|| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(layout::SPACE_MD))
                                    // Host and Port are the far end of the
                                    // forward, which is easy to read as this
                                    // machine's view of the network.
                                    .child(
                                        div()
                                            .text_size(px(layout::chrome(layout::TEXT_XS)))
                                            .text_color(t.text_faint)
                                            .child(
                                                "Host and Port are as the SSH host sees them: \
                                                 localhost is the SSH host itself.",
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap(px(layout::SPACE_SM))
                                            .child(div().flex_1().child(self.form_field(
                                                "SSH host",
                                                &form.ssh_host,
                                                cx,
                                            )))
                                            .child(div().w(px(96.)).child(self.form_field(
                                                "SSH port",
                                                &form.ssh_port,
                                                cx,
                                            ))),
                                    )
                                    .child(self.form_field("SSH username", &form.ssh_user, cx))
                                    .child(self.form_field(
                                        "Identity file",
                                        &form.ssh_identity_file,
                                        cx,
                                    ))
                            }))
                    }))
                    // Outside the server block: every engine can stop a
                    // statement, and this is the only place a profile has to
                    // say for how long -- there is no settings window and is
                    // not going to be one.
                    .child(self.form_field("Statement timeout", &form.statement_timeout, cx))
                    .children(message.map(|message| {
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(t.danger)
                            .child(message)
                    }))
                    .children(form.test.as_ref().map(|test| {
                        let (color, text) = match test {
                            ConnectionTest::Running(_) => {
                                (t.text_muted, "Testing connection…".to_string())
                            }
                            ConnectionTest::Passed => {
                                (t.success, "Connection succeeded.".to_string())
                            }
                            ConnectionTest::Failed(message) => (t.danger, message.clone()),
                        };
                        div()
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .text_color(color)
                            .child(text)
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(layout::SPACE_SM))
                            .child(
                                button("cancel", "Cancel", Tone::Quiet, Control::Standard, t)
                                    .flex_1()
                                    .on_click(cx.listener(|workspace, _, window, cx| {
                                        workspace.show_editor(&ShowEditor, window, cx);
                                    })),
                            )
                            .child(
                                button(
                                    "test-connection",
                                    "Test",
                                    Tone::Quiet,
                                    Control::Standard,
                                    t,
                                )
                                .flex_1()
                                .on_click(cx.listener(Self::test_connection)),
                            )
                            .child(
                                button(
                                    "connect",
                                    if editing { "Save" } else { "Connect" },
                                    Tone::Primary,
                                    Control::Standard,
                                    t,
                                )
                                .flex_1()
                                .on_click(cx.listener(Self::connect)),
                            ),
                    ),
            )
    }

    /// Where an import's connections go, asked before any is added: a new
    /// project named for the client by default, one already made, or none.
    pub(crate) fn render_import_choice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let pending = self.pending_import.as_ref()?;
        let source = pending.source.label();
        let known = self
            .projects
            .iter()
            .map(|project| project.name.clone())
            .collect::<Vec<_>>();
        let suggested = format!("Imported from {source}");
        let mut groups = Vec::new();
        if !known.contains(&suggested) {
            groups.push(vec![Some(suggested)]);
        }
        if !known.is_empty() {
            groups.push(known.iter().cloned().map(Some).collect());
        }
        groups.push(vec![None]);
        let new = {
            let known = known.clone();
            move |project: &Option<String>| {
                project.as_ref().is_some_and(|name| !known.contains(name))
            }
        };
        let marked = new.clone();
        let picker = Self::dropdown(
            "import-project",
            pending.project.clone(),
            groups,
            move |project| match project {
                None => "No project".into(),
                Some(name) if new(project) => format!("{name} (new project)").into(),
                Some(name) => name.clone().into(),
            },
            move |project| {
                let path = match project {
                    None => return None,
                    Some(_) if marked(project) => icon::PLUS,
                    Some(_) => icon::PROJECT,
                };
                Some(row_icon(t, path).into_any_element())
            },
            |workspace, project, _, _| {
                if let Some(pending) = &mut workspace.pending_import {
                    pending.project = project;
                }
            },
            cx,
        );
        let count = pending.fresh;
        let heading = format!(
            "Import {count} connection{} from {source}",
            if count == 1 { "" } else { "s" }
        );

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
                        .child(section_label(t, &heading))
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child("Choose the project they join."),
                        )
                        .child(self.labelled_field("Project", picker, cx))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-import",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(cx.listener(
                                        |workspace, _, _, cx| {
                                            workspace.pending_import = None;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    button(
                                        "confirm-import",
                                        "Import",
                                        Tone::Primary,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(cx.listener(
                                        |workspace, _, window, cx| {
                                            workspace.confirm_import(window, cx);
                                        },
                                    )),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn form_field(
        &self,
        label: &'static str,
        input: &Entity<InputState>,
        cx: &App,
    ) -> impl IntoElement {
        self.labelled_field(label, Input::new(input).w_full(), cx)
    }

    pub(crate) fn labelled_field(
        &self,
        label: &'static str,
        control: impl IntoElement,
        cx: &App,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_XS))
            .child(
                div()
                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme(cx).text_muted)
                    .child(label),
            )
            .child(control)
    }

    /// What `cmd+w` asks before it takes unapplied cell edits with the tab.
    pub(crate) fn render_reference_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let popup = self.reference_popup.as_ref()?;
        let rows: Vec<AnyElement> = match &popup.choices {
            None => vec![note(t, "Checking…")],
            Some(choices) if choices.is_empty() => {
                vec![note(t, "No rows reference this key.")]
            }
            Some(choices) => choices
                .iter()
                .map(|(index, label, answer)| {
                    let index = *index;
                    if let Err(message) = answer {
                        return div()
                            .px(px(layout::SPACE_SM))
                            .py(px(layout::SPACE_XS))
                            .text_size(px(layout::chrome(layout::TEXT_SM)))
                            .child(div().text_color(t.text_muted).child(label.clone()))
                            .child(
                                div()
                                    .text_color(t.danger)
                                    .child(format!("Could not check: {message}")),
                            )
                            .into_any_element();
                    }
                    div()
                        .id(("reference-choice", index))
                        .px(px(layout::SPACE_SM))
                        .py(px(layout::SPACE_XS))
                        .rounded(px(layout::RADIUS_CONTROL))
                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                        .text_color(t.text)
                        .cursor_pointer()
                        .hover(|row| row.bg(t.element_hover))
                        .child(label.clone())
                        .on_click(cx.listener(move |workspace, _, window, cx| {
                            workspace.reference_popup = None;
                            workspace.open_reference(&OpenReference { index }, window, cx);
                        }))
                        .into_any_element()
                })
                .collect(),
        };

        Some(
            div()
                .id("reference-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .child(
                    div()
                        .absolute()
                        .left(popup.at.x)
                        .top(popup.at.y)
                        .min_w(px(180.))
                        .max_w(px(420.))
                        .p(px(layout::SPACE_XS))
                        .flex()
                        .flex_col()
                        .rounded(px(layout::RADIUS_CONTROL))
                        .map(|card| frosted_card(card, t))
                        .border_1()
                        .border_color(t.border)
                        .children(rows)
                        // Out of the panel, not on the backdrop: a press on a
                        // choice is also a press on the backdrop, and closing
                        // there drops the choice before its click lands.
                        .on_mouse_down_out(cx.listener(|workspace, _, _, cx| {
                            workspace.reference_popup = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_discard_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        self.profile()?.session.pending_discard.as_ref()?;
        let cancel_workspace = cx.entity().downgrade();
        let discard_workspace = cancel_workspace.clone();

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
                        .child(section_label(t, "Close tab"))
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                // The edits are held against the fetched rows
                                // and never written to them, so closing the
                                // tab is the moment they stop existing.
                                .child(
                                    "This tab has cell edits that have not been applied. \
                                     Closing it discards them.",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-discard-close",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = cancel_workspace.update(cx, |workspace, cx| {
                                                workspace.cancel_discard_close(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    button(
                                        "confirm-discard-close",
                                        "Discard",
                                        Tone::Danger,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            _ = discard_workspace.update(cx, |workspace, cx| {
                                                workspace.confirm_discard_close(window, cx);
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// What `cmd+w` asks before it takes a saved query with the tab.
    pub(crate) fn render_close_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let name = self.profile()?.session.pending_close.clone()?;
        let cancel_workspace = cx.entity().downgrade();
        let delete_workspace = cancel_workspace.clone();
        let deleted = name.clone();

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
                        .child(section_label(t, "Close query"))
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                // The whole point of the dialog: a saved query
                                // is listed while its file exists, so closing
                                // its tab and deleting it are one act.
                                .child(format!(
                                    "{name} is a saved query. Closing its tab deletes it."
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-close-tab",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = cancel_workspace.update(cx, |workspace, cx| {
                                                workspace.cancel_close_tab(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    button(
                                        "confirm-close-tab",
                                        "Delete",
                                        Tone::Danger,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            _ = delete_workspace.update(cx, |workspace, cx| {
                                                workspace.delete_saved_query(
                                                    deleted.clone(),
                                                    window,
                                                    cx,
                                                );
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The statement `execute_and_then` stopped, in one of three shapes
    /// derived from `sql::gate` -- never stored, so the shape shown and the
    /// verdict behind it cannot disagree about what they are asking (spec
    /// §5).
    pub(crate) fn render_pending_run(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let profile = self.profile()?;
        let pending = profile.session.pending_run.as_ref()?;
        // Re-derived rather than trusted from when the prompt was raised: the
        // mode or the silenced list may have changed underneath it (the
        // upgrade arm changes the mode itself, mid-prompt).
        let stop = sql::gate(&pending.verdict, profile.mode, &profile.confirmed)?;
        let name = profile.name.clone();
        let current_mode = profile.mode.label();
        let sql = pending.resume.as_ref().map(|resume| resume.sql.clone());
        let dont_ask = pending.dont_ask;

        let (title, message, confirm, tone) = match stop {
            Stop::Upgrade(needed) => (
                "Mode",
                // `Destructive::Unreadable` only reaches `Upgrade` on an engine
                // with no server-side read-only setting to fall back on
                // (`Engine::holds_read_only`): there is nothing stopping a
                // write it cannot parse, so the reason is worth spelling out
                // rather than reading like an ordinary mode shortfall.
                if pending
                    .verdict
                    .destructive
                    .contains(&Destructive::Unreadable)
                {
                    format!(
                        "{name} is in {current_mode} mode. dbdelve can't parse this, and \
                         this database has no server-side read-only setting to stop it if \
                         it writes, so it needs {}.",
                        needed.label()
                    )
                } else {
                    format!(
                        "{name} is in {current_mode} mode. This needs {}.",
                        needed.label()
                    )
                },
                if sql.is_some() {
                    format!("Switch to {} and run", needed.label())
                } else {
                    format!("Switch to {}", needed.label())
                },
                Tone::Primary,
            ),
            Stop::Confirm(kind) => (
                "Confirm",
                format!("This is a {}. It cannot be undone.", kind.label()),
                "Run".to_string(),
                Tone::Danger,
            ),
            Stop::RunOnce => (
                "Unreadable statement",
                "dbdelve can't parse this, so it can't tell what it does or whether \
                 this connection's mode covers it."
                    .to_string(),
                "Run once".to_string(),
                Tone::Danger,
            ),
        };

        let cancel = cx.entity().downgrade();
        let approve = cancel.clone();
        let tick = cancel.clone();

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
                        .child(section_label(t, title))
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(message),
                        )
                        // The statement itself, because a dialog asking about
                        // SQL that does not show the SQL is asking the user to
                        // trust it rather than read it. Absent for the edit
                        // refusal (Task 6), which has no statement to show.
                        .children(sql.map(|sql| {
                            div()
                                .id("pending-run-sql")
                                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .p(px(layout::SPACE_SM))
                                .rounded(px(layout::RADIUS_CONTROL))
                                .bg(t.surface)
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text)
                                .child(sql)
                        }))
                        // Never for `Unreadable`: silencing it would cover
                        // every future typo along with it, on the strength of
                        // one decision about one of them.
                        .children(match stop {
                            Stop::Confirm(kind) if kind.suppressible() => Some(
                                Checkbox::new("dont-ask-again")
                                    .text_size(px(layout::chrome(layout::TEXT_SM)))
                                    .font_weight(FontWeight::BOLD)
                                    .label(format!(
                                        "Don't ask again for {} on {name}",
                                        kind.label()
                                    ))
                                    .checked(dont_ask)
                                    .on_click(move |_, _, cx| {
                                        _ = tick.update(cx, |workspace, cx| {
                                            workspace.toggle_dont_ask(cx);
                                        });
                                    }),
                            ),
                            _ => None,
                        })
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-pending-run",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = cancel.update(cx, |workspace, cx| {
                                                workspace.cancel_pending_run(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    button(
                                        "approve-pending-run",
                                        confirm,
                                        tone,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = approve.update(cx, |workspace, cx| {
                                                workspace.approve_pending_run(cx);
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// A queue's statement failed with more of the queue left to run, so the
    /// rest waits here until the user says whether it still runs.
    pub(crate) fn render_queue_failure(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let profile = self.profile()?;
        let Tab::Query(id) = profile.session.queue_failure? else {
            return None;
        };
        let queue = profile.session.query_tab(id)?.queue.as_ref()?;
        let failed = queue.done.last()?;
        let QueryState::Failed(error) = &failed.state else {
            return None;
        };
        // The buffer may have been typed in since the run started, so this
        // is the line the statement ran from, in the text it ran from.
        let line = queue.sql.get(..failed.start)?.matches('\n').count() + 1;
        let message = format!(
            "Statement {} of the selection, at line {line}, {}, failed: {} {} {} not run.",
            queue.done.len(),
            crate::session::query_label(&failed.sql),
            error.message,
            queue.remaining.len(),
            match queue.remaining.len() {
                1 => "statement after it has",
                _ => "statements after it have",
            },
        );

        let stop = cx.entity().downgrade();
        let carry_on = stop.clone();

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
                        .child(section_label(t, "Run stopped"))
                        .child(
                            div()
                                .id("queue-failure-message")
                                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(message),
                        )
                        .child(
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(
                                    "Stop leaves the results so far on screen and sends none of \
                                     the rest. Continue sends the statement after the one that \
                                     failed.",
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button("stop-queue", "Stop", Tone::Quiet, Control::Standard, t)
                                        .on_click(move |_, _, cx| {
                                            _ = stop.update(cx, |workspace, cx| {
                                                workspace.stop_queue(cx);
                                            });
                                        }),
                                )
                                .child(
                                    button(
                                        "continue-queue",
                                        "Continue",
                                        Tone::Primary,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = carry_on.update(cx, |workspace, cx| {
                                                workspace.continue_queue(cx);
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Asked before the first edit on rows restored from an earlier session,
    /// which may no longer be what the database holds.
    pub(crate) fn render_stale_edit(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let profile = self.profile()?;
        let stale = profile.session.stale_edit.as_ref()?;
        let dont_ask = stale.dont_ask;
        let editing = stale.resume.is_some();
        let age = profile
            .session
            .active_results()
            .and_then(|results| results.read(cx).delegate().captured())
            .map(|captured| relative_age(store::captured_at().saturating_sub(captured)))
            .unwrap_or_else(|| "moments".into());
        // What Refresh sends, shown for the same reason the mode prompt shows
        // its statement. Only a buffer's: a relation's is dbdelve's own
        // preview of the table.
        let refresh_sql = profile
            .session
            .active_query_tab()
            .and_then(|tab| tab.last_query.clone());

        let cancel = cx.entity().downgrade();
        let refresh = cancel.clone();
        let approve = cancel.clone();
        let tick = cancel.clone();

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
                        .child(section_label(
                            t,
                            match editing {
                                true => "Edit rows restored from your last session?",
                                false => "Refresh with this query?",
                            },
                        ))
                        // From the status bar's Refresh, the statement alone:
                        // it is what the question is about.
                        .children(editing.then(|| {
                            div()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.text_muted)
                                .child(format!(
                                    "These rows were fetched {age} ago, before dbdelve was \
                                     last closed, and were restored from that session rather \
                                     than read again. Anything changed in the database since \
                                     isn't shown here, and an edit overwrites whatever the \
                                     cell holds now."
                                ))
                        }))
                        .children(refresh_sql.map(|sql| {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_XS))
                                .children(editing.then(|| {
                                    div()
                                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                                        .text_color(t.text_muted)
                                        .child("Refresh runs:")
                                }))
                                .child(
                                    div()
                                        .id("stale-edit-sql")
                                        .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                        .overflow_y_scroll()
                                        .p(px(layout::SPACE_SM))
                                        .rounded(px(layout::RADIUS_CONTROL))
                                        .bg(t.surface)
                                        .text_size(px(layout::chrome(layout::TEXT_SM)))
                                        .text_color(t.text)
                                        .child(sql),
                                )
                        }))
                        .children(editing.then(|| {
                            Checkbox::new("dont-ask-stale")
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .font_weight(FontWeight::BOLD)
                                .label("Don't ask again for this connection")
                                .checked(dont_ask)
                                .on_click(move |_, _, cx| {
                                    _ = tick.update(cx, |workspace, cx| {
                                        workspace.toggle_stale_dont_ask(cx);
                                    });
                                })
                        }))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-stale-edit",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = cancel.update(cx, |workspace, cx| {
                                                workspace.cancel_stale_edit(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    button(
                                        "refresh-stale-edit",
                                        "Refresh",
                                        match editing {
                                            true => Tone::Quiet,
                                            false => Tone::Primary,
                                        },
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            _ = refresh.update(cx, |workspace, cx| {
                                                workspace.refresh_stale(window, cx);
                                            });
                                        },
                                    ),
                                )
                                .children(editing.then(|| {
                                    button(
                                        "approve-stale-edit",
                                        "Edit anyway",
                                        Tone::Primary,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            _ = approve.update(cx, |workspace, cx| {
                                                workspace.edit_stale_anyway(window, cx);
                                            });
                                        },
                                    )
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// A relation tab's generated batch, on screen before it runs.
    ///
    /// The statement is the point of the panel: a relation tab has no buffer, so
    /// this is where rule 1's "the statement that runs is the statement on
    /// screen" is satisfied, and Run is the ask.
    ///
    /// It stays up until the batch succeeds. `execute_sql` clears the grid as it
    /// starts and the pending edits go with it, so after a failure this is the
    /// only remaining copy of what was attempted.
    pub(crate) fn render_apply_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = popup_theme(cx);
        let code = fonts(cx).editor.clone();
        let profile = self.profile()?;
        let review = profile.session.apply_review.as_ref()?;
        if !review.covers(profile.session.active) {
            return None;
        }
        let scope = review.tab.scroll_scope(&profile.id);
        // The batch is the only thing this tab can have run while the panel is
        // open, so a failure on it is this batch's failure.
        let error = match profile.session.active_query() {
            Some(QueryState::Failed(error)) => Some(error.message.clone()),
            _ => None,
        };
        let running = matches!(
            profile.session.active_query(),
            Some(QueryState::Running { .. })
        );
        let lines: Vec<String> = review.sql.lines().map(str::to_string).collect();
        let cancel_workspace = cx.entity().downgrade();
        let run_workspace = cancel_workspace.clone();

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
                        .child(section_label(t, review.title))
                        .child(
                            div()
                                .id("apply-review-sql")
                                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .smooth_scroll(&smooth_scoped("apply-review-sql", &scope, cx))
                                .font_family(code)
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                // Line by line: a single child carrying newlines
                                // is one run of text to the layout.
                                .children(lines.into_iter().map(|line| div().child(line))),
                        )
                        .children(error.map(|message| {
                            div()
                                .id("apply-review-error")
                                .max_h(px(layout::DIALOG_TEXT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .text_size(px(layout::chrome(layout::TEXT_SM)))
                                .text_color(t.danger)
                                .child(message)
                        }))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(layout::SPACE_SM))
                                .child(
                                    button(
                                        "cancel-apply",
                                        "Cancel",
                                        Tone::Quiet,
                                        Control::Standard,
                                        t,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = cancel_workspace.update(cx, |workspace, cx| {
                                                workspace.close_apply_review(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    // Quiet while it runs, because the library's
                                    // disabled fill is the only thing that
                                    // dims and our pinned label would stay
                                    // bright over it.
                                    button(
                                        "run-apply",
                                        "Run",
                                        if running { Tone::Quiet } else { Tone::Primary },
                                        Control::Standard,
                                        t,
                                    )
                                    .disabled(running)
                                    .on_click(
                                        move |_, _, cx| {
                                            _ = run_workspace.update(cx, |workspace, cx| {
                                                workspace.run_apply_review(cx);
                                            });
                                        },
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The titlebar's project and connection, the first steps of its path,
    /// each of which opens the palette over every connection. It is the one
    /// place the connection is named, so it stays reachable with the sidebar
    /// folded.
    pub(crate) fn render_connection_name(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let active_name = self
            .profile()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "Connections".into());
        let active_color = self.profile().and_then(|profile| profile.color);
        // The logo keeps its brand here, never the connection's colour: the
        // band or pill behind it is already wearing that colour, and the logo
        // would sink into it. So its lightness is tuned to whatever it sits on.
        let behind_logo = match active_color {
            Some(color) if self.settings.color_titlebar => color.band(t).flatten(t.surface),
            Some(color) => color.chip(t),
            None => t.surface,
        };
        let active_logo = match self.profile() {
            Some(profile) => {
                engine_logo(t, profile.config.engine(), &[behind_logo]).into_any_element()
            }
            None => row_icon(t, icon::DATABASE).into_any_element(),
        };
        let project_name = self.current_group().map(str::to_string);
        let open_from_project = cx.listener(|workspace, _, window, cx| {
            workspace.open_connections(window, cx);
        });
        let open_from_connection = cx.listener(|workspace, _, window, cx| {
            workspace.open_connections(window, cx);
        });

        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            // The titlebar's own gap, so this `/` spaces like the
            // breadcrumb's after it.
            .gap(px(layout::SPACE_SM))
            .when_some(project_name, |path, name| {
                path.child(
                    crumb(
                        "project-switcher",
                        t,
                        Some(row_icon(t, icon::PROJECT).into_any_element()),
                        name,
                        false,
                        layout::SIDEBAR_DEFAULT_WIDTH / 2.,
                    )
                    .on_click(open_from_project),
                )
                .child(crumb_separator(t))
            })
            .child(
                crumb(
                    "profile-switcher",
                    t,
                    Some(active_logo),
                    active_name,
                    false,
                    layout::SIDEBAR_DEFAULT_WIDTH,
                )
                // Without a coloured band to say which connection this is,
                // the name becomes a pill in its own hue.
                .when_some(
                    active_color.filter(|_| !self.settings.color_titlebar),
                    |pill, color| pill.bg(color.chip(t)),
                )
                .on_click(open_from_connection),
            )
            .into_any_element()
    }

    /// The database the connection is on and, where the engine has one, the
    /// active schema, each a step after the connection that opens its picker.
    pub(crate) fn render_breadcrumb(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = *theme(cx);
        let Some(profile) = self.profile() else {
            return Vec::new();
        };
        let segment = |id: &'static str, label: String, placeholder: bool| {
            crumb(
                id,
                t,
                None,
                label,
                placeholder,
                layout::SIDEBAR_DEFAULT_WIDTH / 2.,
            )
        };
        let separator = || crumb_separator(t);
        let mut steps = Vec::new();
        if profile.config.engine().switches_database() {
            let database = profile
                .databases
                .current
                .clone()
                .or_else(|| profile.config.database_name())
                .unwrap_or_else(|| "Database".into());
            steps.push(separator());
            steps.push(
                segment("database-switcher", database, false)
                    .on_click(cx.listener(|workspace, _, window, cx| {
                        workspace.select_database(&SelectDatabase, window, cx);
                    }))
                    .into_any_element(),
            );
        }
        if profile.config.engine().picks_schema() {
            let schema = profile.active_schema();
            steps.push(separator());
            steps.push(
                segment(
                    "schema-switcher",
                    schema.unwrap_or("Schema").to_string(),
                    schema.is_none(),
                )
                .on_click(cx.listener(|workspace, _, window, cx| {
                    workspace.select_schema(window, cx);
                }))
                .into_any_element(),
            );
        }
        steps
    }

    /// Rename and delete, revealed by hovering a row grouped as
    /// `project-row-{index}`, or held in view while the delete is armed.
    pub(super) fn project_actions(
        &self,
        index: usize,
        name: &str,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let workspace = cx.entity().downgrade();
        let rename_workspace = workspace.clone();
        let rename_name = name.to_string();
        let delete_name = name.to_string();
        let pending = self.pending_project_deletion.as_deref() == Some(name);
        div()
            .flex()
            .items_center()
            .gap(px(layout::SPACE_XS))
            .when(!pending, |actions| {
                actions
                    .opacity(0.)
                    .group_hover(format!("project-row-{index}"), |style| style.opacity(1.))
            })
            .child(
                icon_button(
                    ("rename-project", index),
                    icon::RENAME,
                    Tone::Quiet,
                    Control::Inline,
                    t,
                )
                .tooltip("Rename project")
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    _ = rename_workspace.update(cx, |workspace, cx| {
                        workspace.start_naming_project(Some(rename_name.clone()), window, cx);
                    });
                }),
            )
            .child(
                icon_button(
                    ("delete-project", index),
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
                .tooltip("Delete project (its connections are kept)")
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.delete_project(&delete_name, cx);
                    });
                }),
            )
    }

    pub(crate) fn render_explorer(
        &self,
        profile: &Profile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let leaves = profile.session.explorer_leaves.clone();
        let highlights = profile.session.explorer_highlights.clone();
        let menu_leaves = leaves.clone();
        let menu_workspace = workspace.clone();
        let active_schema = profile.active_schema().map(gpui::SharedString::from);
        let menu_schema = active_schema.clone();
        let picks_schema = profile.config.engine().picks_schema();
        let content = match &profile.catalog {
            CatalogState::Loading => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.text_muted)
                .child("Loading database objects…")
                .into_any_element(),
            CatalogState::Failed(message) => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.danger)
                .child(message.clone())
                .into_any_element(),
            CatalogState::Loaded(catalog, _) if catalog.schemas.is_empty() => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.text_muted)
                .child("No database objects found.")
                .into_any_element(),
            CatalogState::Loaded(..) => {
                render_tree(
                    &profile.session.explorer_tree,
                    move |index, entry, _, _, cx| {
                        let t = *theme(cx);
                        let leaf = leaves.get(entry.item().id.as_str()).copied();
                        let label = entry.item().label.clone();
                        let active = leaf.is_none()
                            && entry.depth() == 0
                            && active_schema.as_ref() == Some(&label);
                        let matched = highlights
                            .get(entry.item().id.as_str())
                            .map_or(&[][..], Vec::as_slice);
                        // Three ranks, three weights: a schema owns the column, a
                        // category only labels the run of objects under it, and the
                        // objects themselves are what the eye is actually hunting for.
                        let (label, row) = match (leaf, entry.depth()) {
                            (Some(_), _) => (
                                highlighted(label, matched, t),
                                ListItem::new(index).text_color(t.text),
                            ),
                            (None, 0) => (
                                highlighted(label, matched, t),
                                ListItem::new(index)
                                    .text_color(t.text)
                                    .font_weight(FontWeight::SEMIBOLD),
                            ),
                            (None, _) => (
                                StyledText::new(label.to_uppercase()),
                                ListItem::new(index)
                                    .text_color(t.text_faint)
                                    .text_size(px(layout::chrome(layout::TEXT_XS)))
                                    .font_weight(FontWeight::MEDIUM),
                            ),
                        };
                        // A folder shows which way it is facing; an object shows what
                        // kind of object it is. Both occupy the same slot, so the
                        // labels line up down the column either way.
                        let row_icon_path = match leaf {
                            Some(leaf) => object_icon(leaf.kind),
                            None if entry.is_expanded() => icon::CHEVRON_DOWN,
                            None => icon::CHEVRON_RIGHT,
                        };
                        let row = row
                            .mx(px(layout::SPACE_XS))
                            .rounded(px(layout::RADIUS_CONTROL))
                            // `ListItem` sizes its text in `rems`, which tracks
                            // the library's 16 rather than dbdelve's body size.
                            .text_size(px(layout::chrome(layout::TEXT_MD)))
                            .pl(px(
                                layout::SPACE_SM + entry.depth() as f32 * layout::SPACE_MD
                            ))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(layout::SPACE_SM))
                                    .min_w_0()
                                    .flex_1()
                                    .child(row_icon(t, row_icon_path))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .child(label),
                                    )
                                    .when(active, |row| {
                                        row.child(
                                            div()
                                                .flex_none()
                                                .whitespace_nowrap()
                                                .text_color(t.text_faint)
                                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                                .font_weight(FontWeight::NORMAL)
                                                .child("active"),
                                        )
                                    })
                                    .when_some(leaf.and_then(|leaf| leaf.size), |row, size| {
                                        row.child(
                                            div()
                                                .flex_none()
                                                .whitespace_nowrap()
                                                .text_color(t.text_faint)
                                                .text_size(px(layout::chrome(layout::TEXT_XS)))
                                                .child(human_bytes(size)),
                                        )
                                    }),
                            );
                        let Some(leaf) = leaf else {
                            return row;
                        };
                        let workspace = workspace.clone();
                        // Every opened object gets a tab that stays until it is
                        // closed, so a second click on the same row is the same
                        // gesture as the first.
                        row.on_click(move |_, window, cx| {
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.open_explorer_target(leaf.target, window, cx);
                            });
                        })
                    },
                )
                .context_menu(move |_, entry, menu, _, cx| {
                    let Some(leaf) = menu_leaves.get(entry.item().id.as_str()).copied() else {
                        if !picks_schema || entry.depth() != 0 {
                            return menu;
                        }
                        let workspace = menu_workspace.clone();
                        let schema = entry.item().label.to_string();
                        return menu.item(
                            PopupMenuItem::new("Set as Active Schema")
                                .checked(menu_schema.as_ref() == Some(&entry.item().label))
                                .on_click(move |_, _, cx| {
                                    let schema = schema.clone();
                                    _ = workspace.update(cx, |workspace, cx| {
                                        workspace.set_schema(schema, cx);
                                    });
                                }),
                        );
                    };
                    let Some(workspace) = menu_workspace.upgrade() else {
                        return menu;
                    };
                    let workspace = workspace.read(cx);
                    let Some((schema, relation, kind)) = workspace.relation_at(leaf.target) else {
                        return menu;
                    };
                    let engine = workspace.engine();
                    let copy = |label: &'static str, text: String| {
                        PopupMenuItem::new(label).on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        })
                    };
                    let ddl_workspace = menu_workspace.clone();
                    let ddl_names = (schema.clone(), relation.clone());
                    let open = |label: &'static str, structure: bool| {
                        let workspace = menu_workspace.clone();
                        let (schema, relation) = (schema.clone(), relation.clone());
                        PopupMenuItem::new(label).on_click(move |_, window, cx| {
                            let (schema, relation) = (schema.clone(), relation.clone());
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.open_relation_view(
                                    schema, relation, kind, structure, window, cx,
                                );
                            });
                        })
                    };
                    let import_workspace = menu_workspace.clone();
                    let import_names = (schema.clone(), relation.clone());
                    let mut menu = menu
                        .item(open("Open Data", false))
                        .item(open("Open Structure", true));
                    if engine.takes_imports(kind) {
                        menu = menu.item(PopupMenuItem::new("Import Rows…").on_click(
                            move |_, window, cx| {
                                let (schema, relation) = import_names.clone();
                                _ = import_workspace.update(cx, |workspace, cx| {
                                    workspace.import_rows_into(schema, relation, window, cx);
                                });
                            },
                        ));
                    }
                    let menu = menu
                        .separator()
                        .when(engine.copies_ddl(), |menu| {
                            menu.item(PopupMenuItem::new("Copy DDL").on_click(move |_, _, cx| {
                                let (schema, relation) = ddl_names.clone();
                                _ = ddl_workspace.update(cx, |workspace, cx| {
                                    workspace.copy_ddl(schema, relation, kind, cx);
                                });
                            }))
                        })
                        .when_some(select_top_sql(engine, &schema, &relation), |menu, sql| {
                            menu.item(copy("Copy SELECT (Top 100)", sql))
                        })
                        .separator()
                        .when_some(drop_sql(engine, &schema, &relation, kind), |menu, sql| {
                            menu.item(copy("Copy DROP", sql))
                        });
                    if matches!(kind, RelationKind::Table | RelationKind::PartitionedTable) {
                        menu.item(copy(
                            "Copy TRUNCATE",
                            truncate_sql(engine, &schema, &relation),
                        ))
                    } else {
                        menu
                    }
                })
                .into_any_element()
            }
        };

        div()
            .size_full()
            .h_full()
            .flex()
            .flex_col()
            // No border of its own: the resizable split's handle already
            // paints the one hairline this edge gets.
            .child(
                // A quiet filter row rather than a boxed field: on chrome, an
                // outlined input is the loudest thing in the column, and the
                // filter is the least interesting thing in it.
                div()
                    .w_full()
                    .h(px(layout::chrome(layout::TAB_HEIGHT)))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_XS))
                    .px(px(layout::SPACE_SM))
                    .border_b_1()
                    .border_color(t.border)
                    .child(row_icon(t, icon::SEARCH))
                    // Clipped by a box of its own: the input lays its placeholder
                    // out at its full width and ellipsises it against that,
                    // so without a clip the text runs on past the row.
                    .child(
                        div().flex_1().min_w_0().overflow_hidden().child(
                            Input::new(&profile.session.explorer_filter)
                                .w_full()
                                .min_w_0()
                                .appearance(false),
                        ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(content))
    }
}

fn note(t: Theme, text: &'static str) -> AnyElement {
    div()
        .px(px(layout::SPACE_SM))
        .py(px(layout::SPACE_XS))
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .text_color(t.text_muted)
        .child(text)
        .into_any_element()
}

/// A row in the welcome surface's list of projects.
pub(super) fn welcome_row(id: impl Into<gpui::ElementId>, t: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(layout::chrome(30.)))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .hover(|style| style.bg(t.element_hover))
}

/// The mark a card leads with: its icon, in the accent, on a tile.
fn icon_tile(t: Theme, path: &'static str) -> gpui::Div {
    div()
        .size(px(layout::chrome(28.)))
        .flex_shrink_0()
        .rounded(px(layout::RADIUS_CONTROL))
        .bg(t.element_active)
        .flex()
        .items_center()
        .justify_center()
        .child(
            icon(path)
                .size(px(layout::chrome(layout::ICON_SIZE)))
                .text_color(t.accent),
        )
}

pub(super) fn name_field(input: &Entity<InputState>) -> AnyElement {
    div()
        .px(px(layout::SPACE_XS))
        .py(px(layout::SPACE_XS))
        .child(gpui_component::Sizable::small(Input::new(input)))
        .into_any_element()
}

/// One step of the titlebar's path. The project, the connection, its database
/// and its schema all look and hover alike, so the path reads as one thing.
fn crumb(
    id: &'static str,
    t: Theme,
    marker: Option<AnyElement>,
    label: impl Into<gpui::SharedString>,
    placeholder: bool,
    max_width: f32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(layout::SPACE_XS))
        .h(px(layout::chrome(layout::CONTROL_HEIGHT_COMPACT)))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .text_color(t.text)
        .font_weight(FontWeight::MEDIUM)
        .hover(|style| style.bg(t.element_hover))
        .children(marker)
        .child(
            div()
                .max_w(px(max_width))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .when(placeholder, |label| label.italic())
                .child(label.into()),
        )
        .child(row_icon(t, icon::CHEVRON_DOWN))
}

fn crumb_separator(t: Theme) -> AnyElement {
    div()
        .flex_shrink_0()
        .text_size(px(layout::chrome(layout::TEXT_SM)))
        .text_color(t.text_faint)
        .child("/")
        .into_any_element()
}
