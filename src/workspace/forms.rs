//! The modal surfaces: the connection form, the palette, the confirmations.
//!
//! These were methods on `Workspace` in main.rs. Rust lets one inherent
//! impl live in as many modules as it has concerns; they moved out whole.

use gpui_component::checkbox::Checkbox;

use super::*;
use crate::sql::Stop;

impl Workspace {
    pub(crate) fn render_connection_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);
        let form = self
            .form
            .as_ref()
            .expect("form is rendered only while open");
        let message = form.error.clone();
        let editing = form.editing.is_some();
        let hairline = || div().h(px(1.)).flex_1().bg(t.border);

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
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
                                    .child(
                                        div()
                                            .text_size(px(layout::TEXT_SM))
                                            .text_color(t.text_muted)
                                            .child(if editing {
                                                "Change where this connection points."
                                            } else {
                                                "Paste a URL, or fill in the fields."
                                            }),
                                    ),
                            ),
                    )
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
                                    .child("Engine"),
                            )
                            .child(
                                div().flex().gap(px(layout::SPACE_XS)).children(
                                    Engine::ALL.map(|engine| self.engine_chip(engine, cx)),
                                ),
                            ),
                    )
                    // Only while creating: past that, `Workspace::set_mode` is
                    // the one door a mode changes through, from the titlebar,
                    // and it pushes the change into live grids this form has
                    // no route to.
                    .when(!editing, |form| {
                        form.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(layout::SPACE_XS))
                                .child(
                                    div()
                                        .text_size(px(layout::TEXT_SM))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(t.text_muted)
                                        .child("Mode"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap(px(layout::SPACE_XS))
                                        .children(Mode::ALL.map(|mode| self.mode_chip(mode, cx))),
                                ),
                        )
                    })
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
                    .child(self.form_field("Display name", &form.name, cx))
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
                                    .child("Color"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap(px(layout::SPACE_XS))
                                    .child(self.color_chip(None, cx))
                                    .children(
                                        ConnectionColor::ALL
                                            .map(|color| self.color_chip(Some(color), cx)),
                                    ),
                            ),
                    )
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
                            .child(self.form_field("Host (optional)", &form.host, cx))
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
                            .child(self.form_field("Password", &form.password, cx))
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
            .child(Input::new(input).w_full())
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
                        .on_mouse_down_out(move |_, _, cx| {
                            _ = workspace.update(cx, |workspace, cx| {
                                workspace.close_palette(cx);
                            });
                        }),
                )
                .into_any_element(),
        )
    }

    /// What `cmd+w` asks before it takes unapplied cell edits with the tab.
    pub(crate) fn render_discard_confirmation(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = *theme(cx);
        self.profile()?.session.pending_discard.as_ref()?;
        let cancel_workspace = cx.entity().downgrade();
        let discard_workspace = cancel_workspace.clone();

        Some(
            div()
                .absolute()
                .inset_0()
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
                                        move |_, _, cx| {
                                            _ = discard_workspace.update(cx, |workspace, cx| {
                                                workspace.confirm_discard_close(cx);
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
                                        move |_, _, cx| {
                                            _ = delete_workspace.update(cx, |workspace, cx| {
                                                workspace.delete_saved_query(deleted.clone(), cx);
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
                format!(
                    "{name} is in {current_mode} mode. This needs {}.",
                    needed.label()
                ),
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
            let profile_rows = self
                .profiles
                .iter()
                .enumerate()
                .map(|(index, profile)| {
                    let activate_workspace = workspace.clone();
                    let edit_workspace = workspace.clone();
                    let remove_workspace = workspace.clone();
                    let pending = self.pending_removal.as_deref() == Some(&profile.id);
                    let active = index == self.active;
                    div()
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
                        })
                        .into_any_element()
                })
                .collect::<Vec<_>>();

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
                .w(px(layout::SIDEBAR_DEFAULT_WIDTH))
                .p(px(layout::SPACE_XS))
                .bg(t.overlay_glass())
                .border_1()
                .border_color(t.border_strong)
                .rounded(px(layout::RADIUS_PANEL))
                .shadow_lg()
                .flex()
                .flex_col()
                .child(
                    div()
                        .px(px(layout::SPACE_SM))
                        .py(px(layout::SPACE_XS))
                        .child(section_label(t, "Connections")),
                )
                .children(profile_rows)
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
                                workspace.form = Some(ConnectionForm::new(None, window, cx));
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
                    // With a colour the name is the one thing in the titlebar
                    // wearing it, so it stops being a subtitle and becomes the
                    // label of a pill filled with its own hue. The fill and the
                    // icon carry the colour; the text does not, because the
                    // same hue at text size on a tint of itself is the one
                    // arrangement nobody can read.
                    .when_some(active_color, |pill, color| {
                        pill.bg(color.fill())
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
                                cx.notify();
                            });
                        })
                    }),
            )
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
            CatalogState::Loaded(catalog) if catalog.schemas.is_empty() => div()
                .p(px(layout::SPACE_MD))
                .text_color(t.text_muted)
                .child("No database objects found.")
                .into_any_element(),
            CatalogState::Loaded(_) => {
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
                                    ),
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
                    .pl(px(layout::SPACE_MD))
                    .pr(px(layout::SPACE_SM))
                    .child(row_icon(t, icon::SEARCH))
                    .child(
                        Input::new(&profile.session.explorer_filter)
                            .min_w_0()
                            .flex_1()
                            .appearance(false),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .py(px(layout::SPACE_XS))
                    .child(content),
            )
    }
}
