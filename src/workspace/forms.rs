//! The modal surfaces: the connection form, the palette, the confirmations.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use gpui_component::checkbox::Checkbox;

use super::*;
use crate::connection_form::ConnectionTest;
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
                        .text_size(px(layout::TEXT_SM))
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
                            .child(
                                div()
                                    .size(px(28.))
                                    .flex_shrink_0()
                                    .rounded(px(layout::RADIUS_CONTROL))
                                    .bg(t.element_active)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        icon(icon::DATABASE)
                                            .size(px(layout::ICON_SIZE))
                                            .text_color(t.accent),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_size(px(layout::TEXT_LG))
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
                                                    .text_size(px(layout::TEXT_SM))
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
                    .when(!editing, |form_div| {
                        form_div.children(form.importable.iter().map(|&source| {
                            button(
                                gpui::SharedString::from(format!("import-{}", source.label())),
                                format!("Import from {}", source.label()),
                                Tone::Quiet,
                                Control::Standard,
                                t,
                            )
                            .on_click(cx.listener(
                                move |workspace, _, window, cx| {
                                    workspace.import_connections(source, window, cx);
                                },
                            ))
                        }))
                    })
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
                                            .text_size(px(layout::TEXT_SM))
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
                                            .text_size(px(layout::TEXT_XS))
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
                                    .child(
                                        div()
                                            .w(px(96.))
                                            .child(self.form_field("Port", &form.port, cx)),
                                    ),
                            )
                            .child(self.form_field("Database", &form.database, cx))
                            .child(self.form_field("Username", &form.user, cx))
                            .child(self.labelled_field(
                                "Password",
                                Input::new(&form.password).mask_toggle(),
                                cx,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(layout::SPACE_XS))
                                    .child(
                                        div()
                                            .text_size(px(layout::TEXT_SM))
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
                                            .text_size(px(layout::TEXT_XS))
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
                                    .text_size(px(layout::TEXT_SM))
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
                                            .text_size(px(layout::TEXT_XS))
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
                            .text_size(px(layout::TEXT_SM))
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
                            .text_size(px(layout::TEXT_SM))
                            .text_color(color)
                            .child(text)
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(layout::SPACE_SM))
                            // `escape` is the other way out, and on the first
                            // launch there is nowhere to back out to: the form
                            // is the whole application until a profile exists.
                            .children((editing || !self.profiles.is_empty()).then(|| {
                                button("cancel", "Cancel", Tone::Quiet, Control::Standard, t)
                                    .flex_1()
                                    .on_click(cx.listener(|workspace, _, window, cx| {
                                        workspace.show_editor(&ShowEditor, window, cx);
                                    }))
                            }))
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

    pub(crate) fn form_field(
        &self,
        label: &'static str,
        input: &Entity<InputState>,
        cx: &App,
    ) -> impl IntoElement {
        self.labelled_field(label, Input::new(input), cx)
    }

    fn labelled_field(&self, label: &'static str, input: Input, cx: &App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(layout::SPACE_XS))
            .child(
                div()
                    .text_size(px(layout::TEXT_SM))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme(cx).text_muted)
                    .child(label),
            )
            .child(input.w_full())
    }

    /// The palette, centred over everything else.
    ///
    /// `key_context` is load-bearing: the arrow keys are bound against
    /// `Palette > Input`, which is the only predicate deep enough to win the
    /// keystroke back from the search field. See `move_palette_selection`.
    pub(crate) fn render_palette(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *theme(cx);
        let list = self.palette.as_ref()?;
        let placeholder = list.read(cx).delegate().placeholder();
        let workspace = cx.entity().downgrade();

        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .justify_center()
                // Cross-axis stretch is the flex default, and it would take the
                // palette's own height with it: a box down to the bottom of the
                // window, with the list capped at its own max height near the
                // top and the rest of the panel painted empty.
                .items_start()
                .child(
                    div()
                        .id("palette")
                        .key_context("Palette")
                        // Below the titlebar rather than centred vertically:
                        // the eye is already at the top of the window, and the
                        // list grows downwards from a fixed line.
                        .mt(px(layout::TITLEBAR_HEIGHT * 2.))
                        .w(px(layout::PALETTE_WIDTH))
                        .bg(t.overlay_glass())
                        .border_1()
                        .border_color(t.border_strong)
                        .rounded(px(layout::RADIUS_PANEL))
                        .shadow_lg()
                        .overflow_hidden()
                        .child(
                            List::new(list)
                                .search_placeholder(placeholder)
                                .max_h(px(layout::PALETTE_MAX_HEIGHT)),
                        )
                        .on_mouse_down_out(move |_, window, cx| {
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.close_palette(window, cx);
                            });
                        }),
                )
                .into_any_element(),
        )
    }

    /// What `cmd+w` asks before it takes unapplied cell edits with the tab.
    pub(crate) fn render_reference_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *theme(cx);
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
                            .text_size(px(layout::TEXT_SM))
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
                        .text_size(px(layout::TEXT_SM))
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
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|workspace, _, _, cx| {
                        workspace.reference_popup = None;
                        cx.notify();
                    }),
                )
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
                        .bg(t.overlay)
                        .border_1()
                        .border_color(t.border)
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_discard_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *theme(cx);
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
                                .text_size(px(layout::TEXT_SM))
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
        let t = *theme(cx);
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
                                .text_size(px(layout::TEXT_SM))
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
        let t = *theme(cx);
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
                                .text_size(px(layout::TEXT_SM))
                                .text_color(t.text_muted)
                                .child(message),
                        )
                        // The statement itself, because a dialog asking about
                        // SQL that does not show the SQL is asking the user to
                        // trust it rather than read it. Absent for the edit
                        // refusal (Task 6), which has no statement to show.
                        .children(sql.map(|sql| {
                            div()
                                .p(px(layout::SPACE_SM))
                                .rounded(px(layout::RADIUS_CONTROL))
                                .bg(t.surface)
                                .text_size(px(layout::TEXT_SM))
                                .text_color(t.text)
                                .child(sql)
                        }))
                        // Never for `Unreadable`: silencing it would cover
                        // every future typo along with it, on the strength of
                        // one decision about one of them.
                        .children(match stop {
                            Stop::Confirm(kind) if kind.suppressible() => Some(
                                Checkbox::new("dont-ask-again")
                                    .text_size(px(layout::TEXT_SM))
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
        let t = *theme(cx);
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
                                .text_size(px(layout::TEXT_SM))
                                .text_color(t.text_muted)
                                .child(message),
                        )
                        .child(
                            div()
                                .text_size(px(layout::TEXT_SM))
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
        let t = *theme(cx);
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
                                .text_size(px(layout::TEXT_SM))
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
                                        .text_size(px(layout::TEXT_SM))
                                        .text_color(t.text_muted)
                                        .child("Refresh runs:")
                                }))
                                .child(
                                    div()
                                        .p(px(layout::SPACE_SM))
                                        .rounded(px(layout::RADIUS_CONTROL))
                                        .bg(t.surface)
                                        .text_size(px(layout::TEXT_SM))
                                        .text_color(t.text)
                                        .child(sql),
                                )
                        }))
                        .children(editing.then(|| {
                            Checkbox::new("dont-ask-stale")
                                .text_size(px(layout::TEXT_SM))
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
        let t = *theme(cx);
        let code = fonts(cx).editor.clone();
        let profile = self.profile()?;
        let review = profile.session.apply_review.as_ref()?;
        if review.tab != profile.session.active {
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
                                .max_h(px(220.))
                                .overflow_y_scroll()
                                .smooth_scroll(&smooth_scoped("apply-review-sql", &scope, cx))
                                .font_family(code)
                                .text_size(px(layout::TEXT_SM))
                                // Line by line: a single child carrying newlines
                                // is one run of text to the layout.
                                .children(lines.into_iter().map(|line| div().child(line))),
                        )
                        .children(error.map(|message| {
                            div()
                                .text_size(px(layout::TEXT_SM))
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

    /// The connection switcher: the titlebar's name for the database in front
    /// of you, which drops a floating panel below itself rather than an
    /// accordion that shoves the tree around. It is the one place the
    /// connection is named, so it stays reachable with the sidebar folded.
    pub(crate) fn render_profile_switcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let panel = self.switcher_open.then(|| {
            let add_workspace = workspace.clone();
            let dismiss_workspace = workspace.clone();
            let mut profile_rows = HashMap::<Option<String>, Vec<AnyElement>>::new();
            for (index, profile) in self.profiles.iter().enumerate().filter(|(_, profile)| {
                self.projects.is_empty() || self.is_expanded(self.group_of(&profile.id))
            }) {
                let group = self.group_of(&profile.id).map(str::to_string);
                let row = {
                    let in_project = self.group_of(&profile.id).is_some();
                    let activate_workspace = workspace.clone();
                    let leave_workspace = workspace.clone();
                    let assign_workspace = workspace.clone();
                    let joinable = self
                        .projects
                        .iter()
                        .enumerate()
                        .filter(|(_, project)| !project.connections.contains(&profile.id))
                        .collect::<Vec<_>>();
                    let assigning = self.assigning_project.as_deref() == Some(&profile.id);
                    let edit_workspace = workspace.clone();
                    let duplicate_workspace = workspace.clone();
                    let remove_workspace = workspace.clone();
                    let pending = self.pending_removal.as_deref() == Some(&profile.id);
                    let active = index == self.active;
                    let row = div()
                        .id(("profile", index))
                        .group(format!("profile-row-{index}"))
                        .h(px(30.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .px(px(layout::SPACE_SM))
                        .rounded(px(layout::RADIUS_CONTROL))
                        .hover(|style| style.bg(t.element_hover))
                        .child(row_icon_tinted(t, icon::DATABASE, profile.color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(profile.name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(layout::SPACE_XS))
                                .when(!pending, |actions| {
                                    actions
                                        .opacity(0.)
                                        .group_hover(format!("profile-row-{index}"), |style| {
                                            style.opacity(1.)
                                        })
                                })
                                .when(!joinable.is_empty(), |actions| {
                                    let id = profile.id.clone();
                                    actions.child(
                                        icon_button(
                                            ("assign-project", index),
                                            icon::ADD_TO_PROJECT,
                                            Tone::Quiet,
                                            Control::Inline,
                                            t,
                                        )
                                        .tooltip("Move to project")
                                        .on_click(
                                            move |_, _, cx| {
                                                cx.stop_propagation();
                                                _ = assign_workspace.update(cx, |workspace, cx| {
                                                    workspace.assigning_project =
                                                        (!assigning).then(|| id.clone());
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    )
                                })
                                .when(in_project, |actions| {
                                    actions.child(
                                        icon_button(
                                            ("leave-project", index),
                                            icon::LEAVE_PROJECT,
                                            Tone::Quiet,
                                            Control::Inline,
                                            t,
                                        )
                                        .tooltip("Remove from project")
                                        .on_click(
                                            move |_, _, cx| {
                                                cx.stop_propagation();
                                                _ = leave_workspace.update(cx, |workspace, cx| {
                                                    workspace.move_to_project(index, None, cx);
                                                });
                                            },
                                        ),
                                    )
                                })
                                .child(
                                    icon_button(
                                        ("edit-profile", index),
                                        icon::RENAME,
                                        Tone::Quiet,
                                        Control::Inline,
                                        t,
                                    )
                                    .tooltip("Edit connection")
                                    .on_click(
                                        move |_, window, cx| {
                                            // The row activates on click, and
                                            // activating clears `form` — so
                                            // without this the form opens and
                                            // is thrown away in the same click.
                                            cx.stop_propagation();
                                            _ = edit_workspace.update(cx, |workspace, cx| {
                                                let Some(profile) = workspace.profiles.get(index)
                                                else {
                                                    return;
                                                };
                                                let form =
                                                    ConnectionForm::editing(profile, window, cx);
                                                workspace.form = Some(form);
                                                workspace.switcher_open = false;
                                                cx.notify();
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    icon_button(
                                        ("duplicate-profile", index),
                                        icon::COPY,
                                        Tone::Quiet,
                                        Control::Inline,
                                        t,
                                    )
                                    .tooltip("Duplicate connection")
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.stop_propagation();
                                            _ = duplicate_workspace.update(cx, |workspace, cx| {
                                                workspace.duplicate_profile(index, window, cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    // Armed, it says the word and takes the
                                    // danger fill: the icon alone asks, the
                                    // red confirms.
                                    icon_button(
                                        ("remove-profile", index),
                                        icon::DELETE,
                                        if pending { Tone::Danger } else { Tone::Quiet },
                                        Control::Inline,
                                        t,
                                    )
                                    .when(pending, |armed| {
                                        armed.w_auto().px(px(layout::SPACE_XS)).child(button_label(
                                            "Remove?",
                                            Tone::Danger,
                                            Control::Inline,
                                            t,
                                        ))
                                    })
                                    .tooltip("Remove connection")
                                    .on_click(
                                        move |_, window, cx| {
                                            // Likewise: activating clears
                                            // `pending_removal`, so the first
                                            // click would never leave it armed.
                                            cx.stop_propagation();
                                            _ = remove_workspace.update(cx, |workspace, cx| {
                                                workspace.remove_profile(index, window, cx);
                                            });
                                        },
                                    ),
                                ),
                        )
                        // The mark sits at the trailing edge like a menu's
                        // checkmark, after the affordances, where the eye ends.
                        .children(active.then(|| {
                            icon(icon::CHECK)
                                .size(px(layout::ICON_SIZE))
                                .text_color(t.text_muted)
                        }))
                        .on_click(move |_, _, cx| {
                            _ = activate_workspace.update(cx, |workspace, cx| {
                                workspace.activate(index, cx);
                            });
                        });
                    let choices = assigning.then(|| {
                        joinable
                            .iter()
                            .map(|(choice, project)| {
                                let join_workspace = workspace.clone();
                                let name = project.name.clone();
                                switcher_row(("assign-to", *choice), t)
                                    .pl(px(layout::SPACE_LG))
                                    .text_color(t.text_muted)
                                    .child(row_icon(t, icon::PROJECT))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .child(project.name.clone()),
                                    )
                                    .on_click(move |_, _, cx| {
                                        _ = join_workspace.update(cx, |workspace, cx| {
                                            workspace.move_to_project(index, Some(&name), cx);
                                        });
                                    })
                            })
                            .collect::<Vec<_>>()
                    });
                    div()
                        .flex()
                        .flex_col()
                        .child(row)
                        .children(choices.into_iter().flatten())
                        .into_any_element()
                };
                profile_rows.entry(group).or_default().push(row);
            }
            let groups = self.render_project_groups(profile_rows, cx);

            div()
                .absolute()
                .occlude()
                .on_mouse_down_out(move |_, _, cx| {
                    _ = dismiss_workspace.update(cx, |workspace, cx| {
                        workspace.switcher_open = false;
                        cx.notify();
                    });
                    // `occlude` only covers what the panel is drawn over. The
                    // press that dismisses lands everywhere else, so swallow it
                    // rather than let it open a table in the tree on the way
                    // out.
                    cx.stop_propagation();
                })
                .top_full()
                .mt(px(layout::SPACE_XS))
                .left_0()
                .w(px(layout::SWITCHER_WIDTH))
                .p(px(layout::SPACE_XS))
                .bg(t.overlay_glass())
                .border_1()
                .border_color(t.border_strong)
                .rounded(px(layout::RADIUS_PANEL))
                .shadow_lg()
                .flex()
                .flex_col()
                .children(groups)
                .child(div().my(px(layout::SPACE_XS)).h(px(1.)).bg(t.border))
                .child(
                    div()
                        .id("new-connection")
                        .h(px(30.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(layout::SPACE_SM))
                        .px(px(layout::SPACE_SM))
                        .rounded(px(layout::RADIUS_CONTROL))
                        .text_color(t.text_muted)
                        .hover(|style| style.bg(t.element_hover).text_color(t.text))
                        .child(row_icon(t, icon::PLUS))
                        .child("Add connection")
                        .on_click(move |_, window, cx| {
                            _ = add_workspace.update(cx, |workspace, cx| {
                                let mut form = ConnectionForm::new(None, window, cx);
                                form.project = match workspace.expanded_groups.last() {
                                    Some(group) => group.clone(),
                                    None => workspace.current_group().map(str::to_string),
                                };
                                workspace.form = Some(form);
                                workspace.switcher_open = false;
                                cx.notify();
                            });
                        }),
                )
        });
        let active_name = self
            .profile()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "Connections".into());
        let active_color = self.profile().and_then(|profile| profile.color);
        let project_name = self.current_group().map(str::to_string);
        let toggle_workspace = workspace.clone();

        div()
            .relative()
            .flex_shrink_0()
            // An overlay is not a layer to gpui: it hit-tests every hitbox the
            // cursor lands in, in tree order, so the explorer under this panel
            // answers the same click. `deferred` puts the panel in front,
            // `occlude` stops what is behind it from answering at all.
            .children(panel.map(deferred))
            .child(
                div()
                    .id("profile-switcher")
                    .flex()
                    .items_center()
                    .gap(px(layout::SPACE_XS))
                    .h(px(layout::CONTROL_HEIGHT_COMPACT))
                    .px(px(layout::SPACE_SM))
                    .rounded(px(layout::RADIUS_CONTROL))
                    .text_size(px(layout::TEXT_SM))
                    .text_color(t.text_faint)
                    .hover(|style| style.bg(t.element_hover).text_color(t.text))
                    .when_some(project_name, |trigger, name| {
                        trigger
                            .child(row_icon(t, icon::PROJECT))
                            .child(
                                div()
                                    .max_w(px(layout::SIDEBAR_DEFAULT_WIDTH / 2.))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(name),
                            )
                            .child("/")
                    })
                    .child(row_icon_tinted(t, icon::DATABASE, active_color))
                    .child(
                        div()
                            .max_w(px(layout::SIDEBAR_DEFAULT_WIDTH))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(active_name),
                    )
                    .child(row_icon(t, icon::CHEVRON_DOWN))
                    // With a colour the name becomes the label of a pill: in
                    // its own hue, or a plain chip when the band behind it is
                    // already wearing that hue. The text keeps the normal
                    // colour, because the hue at text size on a tint of itself
                    // is the one arrangement nobody can read.
                    .when_some(active_color, |pill, color| {
                        pill.bg(if self.settings.color_titlebar {
                            t.surface
                        } else {
                            color.chip(t)
                        })
                        .text_color(t.text)
                        .font_weight(FontWeight::MEDIUM)
                    })
                    // While the panel is open its `on_mouse_down_out` already
                    // owns closing, and it fires on the press. Carrying a click
                    // handler here too would reopen on the release, so the open
                    // panel leaves the trigger without one: no handler, no
                    // click recorded, no reopen.
                    .when(!self.switcher_open, |trigger| {
                        trigger.on_click(move |_, _, cx| {
                            _ = toggle_workspace.update(cx, |workspace, cx| {
                                workspace.switcher_open = true;
                                workspace.pending_removal = None;
                                workspace.pending_project_deletion = None;
                                workspace.assigning_project = None;
                                workspace.expanded_groups =
                                    vec![workspace.current_group().map(str::to_string)];
                                cx.notify();
                            });
                        })
                    }),
            )
            .into_any_element()
    }

    /// No project, then each project, as groups at most one of which is
    /// expanded, holding `members`. The group of the connection in front is
    /// marked, but looking inside another switches nothing. With no projects
    /// there is nothing to group, and the members are listed bare.
    fn render_project_groups(
        &self,
        mut members: HashMap<Option<String>, Vec<AnyElement>>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let current = self.current_group().map(str::to_string);
        let mut rows = Vec::new();
        if self.projects.is_empty() {
            rows.push(
                div()
                    .px(px(layout::SPACE_SM))
                    .py(px(layout::SPACE_XS))
                    .child(section_label(t, "Connections"))
                    .into_any_element(),
            );
            rows.extend(members.remove(&None).unwrap_or_default());
        } else {
            let mut group_body = |group: Option<&str>, id: usize| {
                let members = members
                    .remove(&group.map(str::to_string))
                    .unwrap_or_default();
                div()
                    .pl(px(layout::SPACE_MD))
                    .flex()
                    .flex_col()
                    .when(members.is_empty(), |body| {
                        body.child(
                            switcher_row(("empty-group", id), t)
                                .text_color(t.text_faint)
                                .child("No connections yet"),
                        )
                    })
                    .children(members)
                    .into_any_element()
            };

            let expanded = self.is_expanded(None);
            let ungrouped = self
                .profiles
                .iter()
                .any(|profile| self.group_of(&profile.id).is_none());
            if ungrouped {
                let all_workspace = workspace.clone();
                rows.push(
                    group_header("no-project", current.is_none(), expanded, t)
                        .child(row_icon(t, icon::DATABASE))
                        .child(div().flex_1().child("No project"))
                        .on_click(move |_, _, cx| {
                            _ = all_workspace.update(cx, |workspace, cx| {
                                workspace.toggle_group(None, cx);
                            });
                        })
                        .into_any_element(),
                );
                if expanded {
                    rows.push(group_body(None, usize::MAX));
                }
            }

            for (index, project) in self.projects.iter().enumerate() {
                let expanded = self.is_expanded(Some(&project.name));
                let is_current = current.as_deref() == Some(project.name.as_str());
                if self.renaming_project.as_deref() == Some(project.name.as_str())
                    && let Some(input) = &self.project_name
                {
                    rows.push(name_field(input));
                } else {
                    rows.push(self.project_header(index, project, is_current, expanded, cx));
                }
                if expanded {
                    rows.push(group_body(Some(&project.name), index));
                }
            }
        }
        rows.push(match &self.project_name {
            Some(input) if self.renaming_project.is_none() => name_field(input),
            _ => switcher_row("new-project", t)
                .text_color(t.text_muted)
                .child(row_icon(t, icon::PLUS))
                .child("New project")
                .on_click(move |_, window, cx| {
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.start_naming_project(None, window, cx);
                    });
                })
                .into_any_element(),
        });
        rows
    }

    fn project_header(
        &self,
        index: usize,
        project: &store::StoredProject,
        selected: bool,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let open_workspace = workspace.clone();
        let rename_workspace = workspace.clone();
        let name = project.name.clone();
        let rename_name = project.name.clone();
        let delete_name = project.name.clone();
        let pending = self.pending_project_deletion.as_deref() == Some(project.name.as_str());
        group_header(("project", index), selected, expanded, t)
            .group(format!("project-row-{index}"))
            .child(row_icon(t, icon::PROJECT))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(project.name.clone()),
            )
            .child(
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
                                workspace.start_naming_project(
                                    Some(rename_name.clone()),
                                    window,
                                    cx,
                                );
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
                    ),
            )
            .on_click(move |_, _, cx| {
                _ = open_workspace.update(cx, |workspace, cx| {
                    workspace.toggle_group(Some(name.clone()), cx);
                });
            })
            .into_any_element()
    }

    pub(crate) fn render_explorer(
        &self,
        profile: &Profile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = *theme(cx);
        let workspace = cx.entity().downgrade();
        let leaves = profile.session.explorer_leaves.clone();
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
                        // Three ranks, three weights: a schema owns the column, a
                        // category only labels the run of objects under it, and the
                        // objects themselves are what the eye is actually hunting for.
                        let (label, row) = match (leaf, entry.depth()) {
                            (Some(_), _) => (label, ListItem::new(index).text_color(t.text)),
                            (None, 0) => (
                                label,
                                ListItem::new(index)
                                    .text_color(t.text)
                                    .font_weight(FontWeight::SEMIBOLD),
                            ),
                            (None, _) => (
                                label.to_uppercase().into(),
                                ListItem::new(index)
                                    .text_color(t.text_faint)
                                    .text_size(px(layout::TEXT_XS))
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
                            .text_size(px(layout::TEXT_MD))
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
                                    .when_some(leaf.and_then(|leaf| leaf.size), |row, size| {
                                        row.child(
                                            div()
                                                .flex_none()
                                                .whitespace_nowrap()
                                                .text_color(t.text_faint)
                                                .text_size(px(layout::TEXT_XS))
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
                    .h(px(layout::TAB_HEIGHT))
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
        .text_size(px(layout::TEXT_SM))
        .text_color(t.text_muted)
        .child(text)
        .into_any_element()
}

/// A row in the switcher's panel, the shape every one of its rows shares.
fn switcher_row(id: impl Into<gpui::ElementId>, t: Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(layout::SPACE_SM))
        .px(px(layout::SPACE_SM))
        .rounded(px(layout::RADIUS_CONTROL))
        .hover(|style| style.bg(t.element_hover))
}

/// A group's header: a chevron saying whether it is expanded, and weight
/// saying whether it holds the connection in front.
fn group_header(
    id: impl Into<gpui::ElementId>,
    current: bool,
    expanded: bool,
    t: Theme,
) -> gpui::Stateful<gpui::Div> {
    switcher_row(id, t)
        .text_color(if current { t.text } else { t.text_muted })
        .when(current, |row| row.font_weight(FontWeight::MEDIUM))
        .child(row_icon(
            t,
            if expanded {
                icon::CHEVRON_DOWN
            } else {
                icon::CHEVRON_RIGHT
            },
        ))
}

fn name_field(input: &Entity<InputState>) -> AnyElement {
    div()
        .px(px(layout::SPACE_XS))
        .py(px(layout::SPACE_XS))
        .child(gpui_component::Sizable::small(Input::new(input)))
        .into_any_element()
}
